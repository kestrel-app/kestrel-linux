//! Playback of recorded clips.
//!
//! Unlike the live path this must respect presentation timestamps: a recording
//! decoded as fast as possible races through at hundreds of frames a second.
//! The worker paces output against a wall clock anchored to the stream's own
//! PTS, and supports pause, seek and variable speed.
//!
//! Clips are streamed straight from the device over HTTP, so scrubbing does not
//! require downloading the whole file first. A recording a device replays over
//! Baichuan is streamed too ([`PlaybackWorker::start_recording`]): asked for from
//! the point wanted and played as it arrives, so the picture starts in a second or
//! two however long the recording — where fetching it whole first took as long as
//! the transfer.

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

    /// Play a recording the device replays over Baichuan, as it arrives.
    pub fn start_recording(spec: crate::api::vendor::RecordingStream) -> Self {
        let shared = Arc::new(Shared {
            speed_pct: AtomicU64::new(100),
            ..Default::default()
        });
        let join = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("playback-bc".into())
                .spawn(move || {
                    if let Err(err) = run_recording(&shared, &spec) {
                        if !shared.stop.load(Ordering::Relaxed) {
                            warn!("playback failed: {err}");
                            *shared.failed.lock().unwrap() = Some(err);
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

/// How long a replay may say nothing, once it has begun, before it is taken as over.
const REPLAY_QUIET: Duration = Duration::from_secs(15);

/// How much each download asks for, where the device will not replay: enough to
/// play on while the next is asked for, not so much that a seek wastes the link.
const WINDOW: f64 = 30.0;

/// The replay speeds asked of the device. Slower than real time is paced here.
fn device_speed(speed: f64) -> u32 {
    match speed {
        s if s >= 8.0 => 8,
        s if s >= 4.0 => 4,
        s if s >= 2.0 => 2,
        _ => 1,
    }
}

/// One demuxed frame waiting to be decoded, and when in the recording it falls.
struct Queued {
    codec: crate::api::vendor::baichuan::media::VideoCodec,
    at: f64,
    data: Vec<u8>,
}

/// Play a Baichuan recording as it arrives.
///
/// Asks the device to replay from the point wanted. A device that refuses replay
/// (an NVR at an address answered 405) is played by downloading it a window at a
/// time instead — asked for ahead of the picture, so it plays on across the joins.
/// Either way frames are demuxed as they come into a queue, then decoded straight
/// from their BCMedia payloads and shown at their own timestamps. Seeking, pausing
/// and a change of speed end the request (the hub sends the stop) and, when playing
/// again, ask afresh from where it was.
fn run_recording(shared: &Arc<Shared>, spec: &crate::api::vendor::RecordingStream) -> Result<(), String> {
    use crate::api::vendor::baichuan::hub::{self, FetchEvent};
    use crate::api::vendor::baichuan::media::{demux, starts_with_frame, Frame as Media, VideoCodec};
    use crate::api::vendor::baichuan::{cmd, replay};
    use std::collections::VecDeque;
    use std::sync::mpsc::TryRecvError;

    ffmpeg::init().map_err(|e| e.to_string())?;
    let span = (spec.end - spec.start).num_milliseconds().max(0) as f64 / 1000.0;
    *shared.duration.lock().unwrap() = span;
    let timeout = Duration::from_secs(spec.timeout_secs);
    // Held for the whole playback: every seek or fallback ends one request and
    // asks another, and the connection must not close in between.
    let _connection = hub::hold(&spec.reach, &spec.username, &spec.password, timeout).map_err(|e| e.to_string())?;
    let at_offset = |offset: f64| spec.start + chrono::Duration::milliseconds((offset * 1000.0) as i64);

    // Replay until the device refuses it; then download windows.
    let mut by_download = false;
    let mut fetch: Option<hub::Fetch> = None;
    // The current request has been answered in full (a download's 300).
    let mut request_done = false;
    // Where the next window starts, in seconds into the recording.
    let mut next_window = 0.0f64;
    // The playing point to ask from, after a seek or a pause.
    let mut restart: Option<f64> = Some(0.0);
    let mut asked_speed = 1u32;

    // Maps the device's frame timestamps to time in the recording: `base_micros`
    // is at `base_at` seconds. Set by the first frame after asking afresh.
    let mut base: Option<(u32, f64)> = None;
    // Where the request that will set `base` started, in seconds into the recording.
    let mut asked_from = 0.0f64;
    let mut pending: Vec<u8> = Vec::new();
    let mut seen_keyframe = false;
    let mut queue: VecDeque<Queued> = VecDeque::new();
    // The last frame queued, so the overlap a download starts with is skipped.
    let mut queued_to = f64::NEG_INFINITY;
    let mut last_data = Instant::now();
    let mut showed_any = false;

    let mut decoder: Option<(VideoCodec, ffmpeg::decoder::Video)> = None;
    let mut scaler: Option<scaling::Context> = None;
    let mut clock_start: Option<Instant> = None;
    let mut stream_start = 0.0f64;

    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let position = *shared.position.lock().unwrap();
        if let Some(target) = shared.seek_to.lock().unwrap().take() {
            restart = Some(target.min(span));
            *shared.position.lock().unwrap() = target.min(span);
        }
        if shared.paused.load(Ordering::Relaxed) {
            // Nothing is shown while paused, so let the device stop rather than
            // pile footage up; playing again asks from here.
            if fetch.is_some() && !by_download {
                restart = Some(position);
            }
            clock_start = None;
            std::thread::sleep(Duration::from_millis(30));
            continue;
        }
        let speed = shared.speed_pct.load(Ordering::Relaxed) as f64 / 100.0;
        if fetch.is_some() && !by_download && device_speed(speed) != asked_speed {
            restart = Some(position);
        }

        // --- asking ------------------------------------------------------------
        if let Some(from) = restart.take() {
            fetch = None;
            queue.clear();
            pending.clear();
            base = None;
            seen_keyframe = false;
            queued_to = f64::NEG_INFINITY;
            clock_start = None;
            if let Some((_, dec)) = decoder.as_mut() {
                dec.flush();
            }
            if from >= span {
                return Ok(());
            }
            next_window = from;
        }
        let ahead = queue.back().map(|q| q.at - position).unwrap_or(0.0);
        let want_next = fetch.is_none() || (by_download && request_done && ahead < WINDOW / 2.0);
        if want_next && next_window < span {
            let from = next_window;
            let (msg_id, stop_id, body) = if by_download {
                let to = (from + WINDOW).min(span);
                next_window = to;
                (
                    cmd::DOWNLOAD_BY_TIME,
                    cmd::DOWNLOAD_BY_TIME_STOP,
                    replay::download_request(spec.channel, spec.stream, at_offset(from), at_offset(to)),
                )
            } else {
                asked_speed = device_speed(speed);
                next_window = span;
                (
                    cmd::PLAYBACK_BY_TIME_V2,
                    cmd::PLAYBACK_BY_TIME_STOP_V2,
                    replay::replay_request(spec.channel, spec.stream, at_offset(from), spec.end, asked_speed),
                )
            };
            fetch = Some(
                hub::fetch(&spec.reach, &spec.username, &spec.password, msg_id, stop_id, spec.channel, body, timeout)
                    .map_err(|e| e.to_string())?,
            );
            request_done = false;
            pending.clear();
            seen_keyframe = false;
            last_data = Instant::now();
            // A fresh request (after a seek, or the first) anchors the timestamps
            // on its first frame; a following window keeps the anchor, so its
            // overlap with what is queued can be recognised.
            if base.is_none() {
                asked_from = from;
            }
        }

        // --- receiving ---------------------------------------------------------
        let mut refused = None;
        if let Some(f) = fetch.as_ref() {
            loop {
                match f.events.try_recv() {
                    Ok(FetchEvent::Data(bcmedia)) => {
                        if !pending.is_empty() || starts_with_frame(&bcmedia) {
                            pending.extend_from_slice(&bcmedia);
                        }
                        last_data = Instant::now();
                    }
                    Ok(FetchEvent::Status(300)) => request_done = true,
                    Ok(FetchEvent::Status(code)) if code >= 400 => {
                        refused = Some(code);
                        break;
                    }
                    Ok(FetchEvent::Status(_)) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        request_done = true;
                        break;
                    }
                }
            }
        }
        if let Some(code) = refused {
            if !by_download && !showed_any && queue.is_empty() {
                // Will not replay: play it by downloading instead.
                debug!("replay refused ({code}); playing by download");
                by_download = true;
                restart = Some(position);
                continue;
            }
            return Err(format!("the device would not play this recording ({code})"));
        }
        let (frames, used) = demux(&pending);
        pending.drain(..used);
        for frame in frames {
            let Media::Video { codec, keyframe, micros, data } = frame else { continue };
            seen_keyframe |= keyframe;
            if !seen_keyframe {
                continue;
            }
            let (base_micros, base_at) = *base.get_or_insert((micros, asked_from));
            let mut at = base_at + f64::from(micros.wrapping_sub(base_micros) as i32) / 1e6;
            // Timestamps that do not carry on from the last window: start afresh
            // from where it got to rather than trust them.
            if queued_to.is_finite() && (at - queued_to).abs() > WINDOW {
                base = Some((micros, queued_to));
                at = queued_to;
            }
            // A download starts at the keyframe before the time asked for; what
            // was already queued is the same footage again.
            if at <= queued_to {
                continue;
            }
            queued_to = at;
            queue.push_back(Queued { codec, at, data });
        }

        // --- showing ----------------------------------------------------------
        let Some(next) = queue.pop_front() else {
            let finished = fetch.is_none() || (request_done && next_window >= span);
            if finished && showed_any {
                return Ok(());
            }
            if last_data.elapsed() > REPLAY_QUIET {
                return if showed_any {
                    Ok(())
                } else {
                    Err("the device sent no footage for this recording".into())
                };
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        if decoder.as_ref().map(|(c, _)| *c) != Some(next.codec) {
            let id = match next.codec {
                VideoCodec::H264 => ffmpeg::codec::Id::H264,
                VideoCodec::H265 => ffmpeg::codec::Id::HEVC,
            };
            let found = ffmpeg::decoder::find(id).ok_or("no decoder for this recording's codec")?;
            let mut context = ffmpeg::codec::context::Context::new_with_codec(found);
            context.set_threading(ffmpeg::threading::Config {
                kind: ffmpeg::threading::Type::Frame,
                count: 0,
            });
            decoder = Some((next.codec, context.decoder().video().map_err(|e| e.to_string())?));
        }
        let (_, dec) = decoder.as_mut().unwrap();
        let mut packet = ffmpeg::Packet::copy(&next.data);
        // Milliseconds into the recording, carried through the decoder so a
        // frame's time survives any reordering inside it.
        packet.set_pts(Some((next.at * 1000.0) as i64));
        if dec.send_packet(&packet).is_err() {
            continue;
        }
        let mut decoded = ffmpeg::frame::Video::empty();
        while dec.receive_frame(&mut decoded).is_ok() {
            if shared.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let at = decoded.pts().map(|p| p as f64 / 1000.0).unwrap_or(next.at);
            match clock_start {
                None => {
                    clock_start = Some(Instant::now());
                    stream_start = at;
                }
                Some(anchor) => {
                    let expected = (at - stream_start) / speed;
                    let elapsed = anchor.elapsed().as_secs_f64();
                    if expected > elapsed {
                        std::thread::sleep(Duration::from_secs_f64((expected - elapsed).min(1.0)));
                    } else if elapsed - expected > 1.0 {
                        clock_start = Some(Instant::now());
                        stream_start = at;
                    }
                }
            }
            if let Some(frame) = to_frame(shared, &mut scaler, &decoded) {
                *shared.latest.lock().unwrap() = Some(Arc::new(frame));
                *shared.position.lock().unwrap() = at.clamp(0.0, span);
                showed_any = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the real device: how long from asking to the first picture, and
    /// that it keeps playing. Ignored; set KESTREL_TEST_UID / KESTREL_TEST_PASS and
    /// KESTREL_TEST_REPLAY ("YYYY-mm-dd HH:MM:SS+secs").
    #[test]
    #[ignore]
    fn a_recording_starts_playing_as_it_arrives() {
        use crate::api::models::StreamType;
        use crate::api::vendor::baichuan::link::Reach;
        let (Ok(uid), Ok(password), Ok(when)) = (
            std::env::var("KESTREL_TEST_UID"),
            std::env::var("KESTREL_TEST_PASS"),
            std::env::var("KESTREL_TEST_REPLAY"),
        ) else {
            eprintln!("KESTREL_TEST_UID / _PASS / _REPLAY not set");
            return;
        };
        let (start, secs) = when.split_once('+').unwrap();
        let start = chrono::NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M:%S").unwrap();
        let reach = Reach::Uid(uid);
        // A tile streaming already, as on the wall, when asked.
        let live = std::env::var("KESTREL_TEST_WITH_LIVE").is_ok().then(|| {
            let sub = crate::api::vendor::baichuan::hub::subscribe(&reach, "admin", &password, 0, StreamType::Sub, Duration::from_secs(10)).unwrap();
            sub.codec.recv_timeout(Duration::from_secs(60)).unwrap();
            sub
        });
        let asked = Instant::now();
        let player = PlaybackWorker::start_recording(crate::api::vendor::RecordingStream {
            reach,
            username: "admin".into(),
            password,
            channel: 0,
            stream: StreamType::Sub,
            start,
            end: start + chrono::Duration::seconds(secs.parse().unwrap()),
            timeout_secs: 10,
        });
        while player.latest_frame().is_none() && asked.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let first = asked.elapsed();
        let frame = player.latest_frame().expect("a picture");
        std::thread::sleep(Duration::from_secs(5));
        println!(
            "first picture {}x{} after {:.1}s; 5s later at {:.1}s into the clip (error: {:?})",
            frame.width,
            frame.height,
            first.as_secs_f64(),
            player.position(),
            player.error()
        );
        assert!(player.position() > 2.0, "it keeps playing");
        drop(live);
    }

    #[test]
    fn the_device_is_asked_for_the_nearest_speed_it_offers() {
        assert_eq!(device_speed(0.5), 1, "slower than real time is paced here");
        assert_eq!(device_speed(1.0), 1);
        assert_eq!(device_speed(2.0), 2);
        assert_eq!(device_speed(4.0), 4);
        assert_eq!(device_speed(16.0), 8);
    }
}
