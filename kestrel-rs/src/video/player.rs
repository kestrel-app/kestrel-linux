//! Playback of recorded clips.
//!
//! Unlike the live path this must respect presentation timestamps: a recording
//! decoded as fast as possible races through at hundreds of frames a second.
//! The worker paces output against a wall clock anchored to the stream's own
//! PTS, and supports pause, seek and variable speed.
//!
//! Clips are streamed straight from the device over HTTP, so scrubbing does not
//! require downloading the whole file first.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling;
use log::{debug, warn};

use super::stream::Frame;

/// The device is a slow origin; allow generous timeouts and let ffmpeg
/// reconnect mid-file rather than abandoning a long clip on one dropped socket.
fn http_options() -> ffmpeg::Dictionary<'static> {
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("timeout", "15000000");
    opts.set("reconnect", "1");
    opts.set("reconnect_streamed", "1");
    opts.set("reconnect_delay_max", "5");
    opts
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    finished: AtomicBool,
    failed: Mutex<Option<String>>,
    /// Seek target in milliseconds, taken by the loop when it next looks.
    seek_to: Mutex<Option<f64>>,
    /// Speed as a percentage, so it can live in an atomic.
    speed_pct: AtomicU64,
    position: Mutex<f64>,
    duration: Mutex<f64>,
    latest: Mutex<Option<Arc<Frame>>>,
    sequence: AtomicU64,
}

pub struct PlaybackWorker {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl PlaybackWorker {
    pub fn start(url: String) -> Self {
        let shared = Arc::new(Shared {
            speed_pct: AtomicU64::new(100),
            ..Default::default()
        });
        let join = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("playback".into())
                .spawn(move || {
                    if let Err(err) = run(&shared, &url) {
                        if !shared.stop.load(Ordering::Relaxed) {
                            warn!("playback failed: {err}");
                            *shared.failed.lock().unwrap() = Some(err.to_string());
                        }
                    }
                    shared.finished.store(true, Ordering::Relaxed);
                })
                .expect("failed to spawn the playback thread")
        };
        PlaybackWorker {
            shared,
            join: Some(join),
        }
    }

    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    pub fn toggle_pause(&self) -> bool {
        let paused = !self.shared.paused.load(Ordering::Relaxed);
        self.shared.paused.store(paused, Ordering::Relaxed);
        paused
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    pub fn seek(&self, seconds: f64) {
        *self.shared.seek_to.lock().unwrap() = Some(seconds.max(0.0));
    }

    pub fn set_speed(&self, speed: f32) {
        let clamped = speed.clamp(0.1, 16.0);
        self.shared
            .speed_pct
            .store((clamped * 100.0) as u64, Ordering::Relaxed);
    }

    pub fn speed(&self) -> f32 {
        self.shared.speed_pct.load(Ordering::Relaxed) as f32 / 100.0
    }

    pub fn position(&self) -> f64 {
        *self.shared.position.lock().unwrap()
    }

    pub fn duration(&self) -> f64 {
        *self.shared.duration.lock().unwrap()
    }

    pub fn latest_frame(&self) -> Option<Arc<Frame>> {
        self.shared.latest.lock().unwrap().clone()
    }

    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.shared.failed.lock().unwrap().clone()
    }
}

impl Drop for PlaybackWorker {
    fn drop(&mut self) {
        self.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(shared: &Arc<Shared>, url: &str) -> Result<(), ffmpeg::Error> {
    ffmpeg::init()?;
    let mut input = ffmpeg::format::input_with_dictionary(&url, http_options())?;

    if input.duration() > 0 {
        *shared.duration.lock().unwrap() =
            input.duration() as f64 / ffmpeg::ffi::AV_TIME_BASE as f64;
    }

    let stream = input
        .streams()
        .best(Type::Video)
        .ok_or(ffmpeg::Error::StreamNotFound)?;
    let stream_index = stream.index();
    let time_base = f64::from(stream.time_base());

    let mut decoder = {
        let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
        context.set_threading(ffmpeg::threading::Config {
            kind: ffmpeg::threading::Type::Frame,
            count: 0,
        });
        context.decoder().video()?
    };
    let mut scaler: Option<scaling::Context> = None;

    // Anchors mapping stream time to wall-clock time. Reset on seek, resume and
    // speed change so drift never accumulates across a transport action.
    let mut clock_start: Option<Instant> = None;
    let mut stream_start = 0.0f64;

    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        if let Some(target) = shared.seek_to.lock().unwrap().take() {
            let position = (target / time_base) as i64;
            if let Err(err) = input.seek(position, ..position) {
                debug!("seek to {target:.2}s failed: {err}");
            }
            decoder.flush();
            clock_start = None;
        }

        if shared.paused.load(Ordering::Relaxed) {
            clock_start = None;
            std::thread::sleep(Duration::from_millis(30));
            continue;
        }

        let Some((packet_stream, packet)) = input.packets().next() else {
            return Ok(()); // end of clip
        };
        if packet_stream.index() != stream_index {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }

        let mut decoded = ffmpeg::frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if shared.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let position = decoded.pts().unwrap_or(0) as f64 * time_base;

            // --- pacing ------------------------------------------------------
            let speed = shared.speed_pct.load(Ordering::Relaxed) as f64 / 100.0;
            match clock_start {
                None => {
                    clock_start = Some(Instant::now());
                    stream_start = position;
                }
                Some(anchor) => {
                    let expected = (position - stream_start) / speed;
                    let elapsed = anchor.elapsed().as_secs_f64();
                    if expected > elapsed {
                        let wait = (expected - elapsed).min(1.0);
                        std::thread::sleep(Duration::from_secs_f64(wait));
                    } else if elapsed - expected > 1.0 {
                        // Badly behind (slow link, heavy seek): re-anchor rather
                        // than burning CPU trying to catch up.
                        clock_start = Some(Instant::now());
                        stream_start = position;
                    }
                }
            }

            if let Some(frame) = to_frame(shared, &mut scaler, &decoded) {
                *shared.latest.lock().unwrap() = Some(Arc::new(frame));
                *shared.position.lock().unwrap() = position;
            }
        }
    }
}

fn to_frame(
    shared: &Arc<Shared>,
    scaler: &mut Option<scaling::Context>,
    decoded: &ffmpeg::frame::Video,
) -> Option<Frame> {
    let (width, height) = (decoded.width(), decoded.height());
    if width == 0 || height == 0 {
        return None;
    }
    let stale = match scaler {
        Some(existing) => {
            existing.input().width != width
                || existing.input().height != height
                || existing.input().format != decoded.format()
        }
        None => true,
    };
    if stale {
        *scaler = scaling::Context::get(
            decoded.format(),
            width,
            height,
            Pixel::RGBA,
            width,
            height,
            scaling::Flags::BILINEAR,
        )
        .ok();
    }
    let scaler = scaler.as_mut()?;

    let mut rgba = ffmpeg::frame::Video::empty();
    scaler.run(decoded, &mut rgba).ok()?;

    let stride = rgba.stride(0);
    let row_bytes = width as usize * 4;
    let mut packed = Vec::with_capacity(row_bytes * height as usize);
    let data = rgba.data(0);
    for row in 0..height as usize {
        packed.extend_from_slice(&data[row * stride..row * stride + row_bytes]);
    }

    Some(Frame {
        width,
        height,
        rgba: packed,
        sequence: shared.sequence.fetch_add(1, Ordering::Relaxed) + 1,
    })
}
