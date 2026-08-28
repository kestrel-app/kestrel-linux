//! RTSP ingest.
//!
//! One `StreamWorker` owns one RTSP connection on its own thread and fans the
//! demuxed packets out to two consumers: the decoder, which produces RGBA
//! frames for the UI, and the recorder, which *remuxes* packets straight to MP4
//! with no re-encode. Keeping both on one connection matters — a tile that
//! opened a second stream to record would double the load on the camera.
//!
//! A worker can also run *warm*: connected and demuxing, but not decoding,
//! holding the packets from the most recent keyframe. Going hot then produces a
//! picture almost immediately instead of paying for a reconnect. Measured
//! against an RLN36: 6.6s cold versus 0.09s from warm.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling;
use log::{debug, error, info, warn};

use crate::api::vendor::StreamSource;
use crate::api::redact_rtsp;

/// Tuned for live viewing rather than throughput: never buffer ahead, fail fast
/// so reconnects are quick, and skip ffmpeg's stream analysis — we already know
/// what a Reolink stream contains, and probing costs ~0.9s per connect.
fn rtsp_options(source: &StreamSource) -> ffmpeg::Dictionary<'static> {
    let mut opts = ffmpeg::Dictionary::new();
    // UniFi serves its streams behind the same session as its API, so the
    // cookie and CSRF token have to travel with the request.
    if let Some(headers) = source.header_blob() {
        opts.set("headers", &headers);
    }
    opts.set("rtsp_transport", "tcp"); // UDP drops badly on congested wifi
    opts.set("fflags", "nobuffer");
    opts.set("flags", "low_delay");
    opts.set("max_delay", "500000");
    opts.set("reorder_queue_size", "0");
    // Socket I/O timeout, in microseconds. `timeout` is the spelling ffmpeg 7
    // takes — checked against the bundled tree, where the RTSP demuxer's option
    // table carries `timeout` and no longer has `stimeout` at all. The old
    // spelling is still set for anyone building against an older ffmpeg, where
    // it was the only one that worked; an unknown option is ignored.
    //
    // This is what bounds recovery: a connection that dies silently is noticed
    // when this expires, and the read loop below turns that into a reconnect.
    opts.set("stimeout", "8000000");
    opts.set("timeout", "8000000");
    opts.set("probesize", "32768");
    opts.set("analyzeduration", "0");
    opts
}

/// Reconnect backoff: quick first retries for transient blips, then back off so
/// a powered-down camera is not hammered.
const RECONNECT_DELAYS: [u64; 6] = [1, 2, 4, 8, 15, 30];

/// A connection that lasted at least this long counts as having worked, so the
/// backoff starts from scratch next time.
///
/// Without this the counter only ever climbs: a camera that was unreachable at
/// launch, then streamed happily for an hour, would still wait 30s to come back
/// after a momentary blip.
const HEALTHY_CONNECTION: Duration = Duration::from_secs(30);

/// How long a run of read errors is tolerated before the connection is torn
/// down and rebuilt.
///
/// Cameras do emit the occasional malformed packet, and reconnecting over one
/// would be worse than ignoring it — a reconnect costs seconds of black. A
/// *run* of them is different: it means the socket is gone, which is what a
/// suspended machine, a dropped Wi-Fi link or a rebooted camera looks like from
/// here.
const READ_ERROR_GRACE: Duration = Duration::from_secs(2);

/// How long the stream may go without a video packet before it is treated as
/// dead.
///
/// The demuxer's own socket timeout catches a connection that has gone silent.
/// This catches the other shape of the same problem: one that is still
/// delivering *something* — RTCP, audio, keepalives — while the video has
/// stopped. Comfortably longer than any keyframe interval worth using.
const STALL_TIMEOUT: Duration = Duration::from_secs(15);

/// A warm stream keeps at most this many packets from the latest keyframe. One
/// GOP is far smaller; the cap only guards a source that stops sending them.
const MAX_WARM_PACKETS: usize = 300;

/// How many decode threads a stream may claim.
///
/// Not `0`, which means one per core. A wall is sixteen decoders at once, and
/// sixteen of those on an eight-core machine is over a hundred threads fighting
/// for it — which does not merely waste time, it makes every worker's read loop
/// late, and a late reader on an RTSP-over-TCP socket is a source that gets
/// throttled and starts dropping. That arrives here as exactly the damage this
/// file works to keep off the screen.
///
/// Two rather than one because slice threading needs somewhere to overlap, and
/// no more than two because most cameras encode one slice per picture and there
/// is nothing there to parallelise anyway.
const DECODE_THREADS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Idle,
    Connecting,
    Playing,
    Reconnecting,
    Error,
    Stopped,
}

#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    pub fps: f32,
    pub bitrate_kbps: f32,
    pub width: u32,
    pub height: u32,
    pub codec: String,
    pub dropped: u64,
    /// How long the picture has been held back waiting for a clean keyframe,
    /// in seconds. Zero whenever a frame last went to screen.
    ///
    /// Kept here rather than as a [`StreamState`] because the stream is not in
    /// trouble in the way the other states mean: it is connected, reading, and
    /// deliberately refusing to show what it is being sent.
    pub recovering_for: f32,
}

/// A decoded frame, ready for upload as a texture.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Monotonically increasing, so the UI can tell a new frame from a repaint.
    pub sequence: u64,
}

#[derive(Default)]
struct RecordRequest {
    start: Option<PathBuf>,
    stop: bool,
}

struct Shared {
    /// Whether this stream's sound should be played. Only one camera has this
    /// set at a time: sixteen cameras of audio at once is noise, and decoding
    /// them all would be waste.
    audio: AtomicBool,
    stop: AtomicBool,
    warm: AtomicBool,
    frame_seq: AtomicU64,
    latest: Mutex<Option<Arc<Frame>>>,
    state: Mutex<(StreamState, String)>,
    stats: Mutex<StreamStats>,
    record: Mutex<RecordRequest>,
    recording_to: Mutex<Option<PathBuf>>,
}

/// Handle to a running stream. Dropping it stops the worker.
pub struct StreamWorker {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
    pub url: String,
    pub name: String,
}

impl StreamWorker {
    pub fn start(source: StreamSource, name: impl Into<String>, warm: bool) -> Self {
        let url = source.url.clone();
        let name = name.into();

        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            warm: AtomicBool::new(warm),
            audio: AtomicBool::new(false),
            frame_seq: AtomicU64::new(0),
            latest: Mutex::new(None),
            state: Mutex::new((StreamState::Idle, String::new())),
            stats: Mutex::new(StreamStats::default()),
            record: Mutex::new(RecordRequest::default()),
            recording_to: Mutex::new(None),
        });

        let join = {
            let shared = Arc::clone(&shared);
            let source = source.clone();
            let name = name.clone();
            std::thread::Builder::new()
                .name(format!("stream:{name}"))
                .spawn(move || run(shared, source, name))
                .expect("failed to spawn stream thread")
        };

        StreamWorker {
            shared,
            join: Some(join),
            url,
            name,
        }
    }

    // ------------------------------------------------------------- control

    /// Ask the loop to exit. Returns immediately; the worker may still be
    /// unwinding a blocking read.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    /// Play this stream's sound, or stop playing it.
    pub fn set_audio(&self, on: bool) {
        self.shared.audio.store(on, Ordering::Relaxed);
    }

    pub fn has_audio(&self) -> bool {
        self.shared.audio.load(Ordering::Relaxed)
    }

    pub fn is_warm(&self) -> bool {
        self.shared.warm.load(Ordering::Relaxed)
    }

    /// Start decoding and emitting frames. Cheap to call repeatedly.
    pub fn go_hot(&self) {
        self.shared.warm.store(false, Ordering::Relaxed);
    }

    /// Stop decoding but stay connected.
    pub fn go_warm(&self) {
        self.shared.warm.store(true, Ordering::Relaxed);
    }

    pub fn start_recording(&self, path: PathBuf) {
        let mut request = self.shared.record.lock().unwrap();
        request.start = Some(path);
        request.stop = false;
    }

    pub fn stop_recording(&self) {
        let mut request = self.shared.record.lock().unwrap();
        request.start = None;
        request.stop = true;
    }

    pub fn recording_path(&self) -> Option<PathBuf> {
        self.shared.recording_to.lock().unwrap().clone()
    }

    // ------------------------------------------------------------- observation

    /// The newest decoded frame, or None if nothing has been decoded yet.
    ///
    /// Deliberately "latest wins": for live viewing an older queued frame is
    /// worse than no frame, so the worker overwrites rather than queues.
    pub fn latest_frame(&self) -> Option<Arc<Frame>> {
        self.shared.latest.lock().unwrap().clone()
    }

    pub fn state(&self) -> (StreamState, String) {
        self.shared.state.lock().unwrap().clone()
    }

    pub fn stats(&self) -> StreamStats {
        self.shared.stats.lock().unwrap().clone()
    }

    /// A reader's handle on this stream, for anything that wants the pictures
    /// without owning the connection.
    pub fn view(&self) -> StreamView {
        StreamView { shared: Arc::clone(&self.shared) }
    }
}

/// A reader's view of a running stream: its frames, its state and its stats,
/// with no claim on the connection behind them.
///
/// This is what lets one camera appear on the wall more than once. A camera and
/// every virtual camera cropped out of it hold a view of the same worker, so
/// the picture is demuxed once and decoded once however many rectangles of it
/// are on screen; each tile uploads its own texture and draws its own crop, and
/// none of them has to agree with the others about anything.
///
/// Dropping one does nothing. The worker lives exactly as long as [`Streams`]
/// keeps it, which is what makes the arithmetic of who is still watching a
/// question the grid answers once per rebuild rather than a refcount threaded
/// through the tiles.
///
/// [`Streams`]: crate::ui::grid::Streams
#[derive(Clone)]
pub struct StreamView {
    shared: Arc<Shared>,
}

impl StreamView {
    pub fn latest_frame(&self) -> Option<Arc<Frame>> {
        self.shared.latest.lock().unwrap().clone()
    }

    pub fn state(&self) -> (StreamState, String) {
        self.shared.state.lock().unwrap().clone()
    }

    pub fn stats(&self) -> StreamStats {
        self.shared.stats.lock().unwrap().clone()
    }

    pub fn recording_path(&self) -> Option<PathBuf> {
        self.shared.recording_to.lock().unwrap().clone()
    }
}

impl Drop for StreamWorker {
    fn drop(&mut self) {
        self.stop();
        if let Some(join) = self.join.take() {
            // The demux loop checks the stop flag between packets, so this is
            // normally instant. A wedged camera can hold it for the socket
            // timeout; callers that cannot afford to wait should retire the
            // worker on a background thread rather than dropping it inline.
            let _ = join.join();
        }
    }
}

// ==================================================================== worker

fn set_state(shared: &Shared, state: StreamState, detail: impl Into<String>) {
    *shared.state.lock().unwrap() = (state, detail.into());
}

/// The attempt counter after one connection, which decides how long to wait
/// before the next.
///
/// Split out from the loop so the rule is testable without a camera: a
/// connection that ran for a while resets the backoff whether or not it ended
/// badly, and only a *quick* failure escalates it.
///
/// The reset is the part worth having. Without it the counter only climbs, so a
/// camera that was unreachable at launch and then streamed happily for an hour
/// would still sit out the full thirty seconds after a momentary blip.
fn next_attempt(attempt: usize, ran_for: Duration, failed: bool) -> usize {
    if ran_for >= HEALTHY_CONNECTION {
        return 0;
    }
    if failed {
        attempt.saturating_add(1)
    } else {
        0
    }
}

fn run(shared: Arc<Shared>, source: StreamSource, name: String) {
    if let Err(err) = ffmpeg::init() {
        error!("ffmpeg init failed: {err}");
        set_state(&shared, StreamState::Error, "ffmpeg unavailable");
        return;
    }
    // We probe deliberately small for fast starts, so ffmpeg complains on every
    // connect that it cannot estimate the frame rate or parse the audio stream
    // we never decode. Keep its own logging to real errors — but allow it to be
    // raised, because decode corruption is only visible at warning level.
    ffmpeg::util::log::set_level(match std::env::var("KESTREL_FFMPEG_LOG").as_deref() {
        Ok("trace") => ffmpeg::util::log::Level::Trace,
        Ok("debug") => ffmpeg::util::log::Level::Debug,
        Ok("verbose") => ffmpeg::util::log::Level::Verbose,
        Ok("info") => ffmpeg::util::log::Level::Info,
        Ok("warning") => ffmpeg::util::log::Level::Warning,
        _ => ffmpeg::util::log::Level::Error,
    });

    let mut attempt = 0usize;
    while !shared.stop.load(Ordering::Relaxed) {
        set_state(
            &shared,
            if attempt == 0 { StreamState::Connecting } else { StreamState::Reconnecting },
            if attempt == 0 { String::new() } else { format!("attempt {}", attempt + 1) },
        );

        let started = Instant::now();
        let outcome = run_once(&shared, &source);
        let ran_for = started.elapsed();

        let failed = match outcome {
            Ok(()) => false, // clean EOF still means reconnect for RTSP
            Err(err) => {
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                let message = redact_rtsp(&err);
                warn!("stream {name} failed after {:.1}s: {message}", ran_for.as_secs_f32());
                set_state(&shared, StreamState::Error, message);
                true
            }
        };
        attempt = next_attempt(attempt, ran_for, failed);

        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let delay = RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)];
        // Sleep in slices so a stop request is honoured during a long backoff.
        let deadline = Instant::now() + Duration::from_secs(delay);
        while Instant::now() < deadline && !shared.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    set_state(&shared, StreamState::Stopped, "");
}

/// The audio side of one stream: decode, convert, play.
///
/// The device is opened lazily and closed as soon as sound is switched off, so
/// a client that is only watching never holds the sound card open — and on a
/// machine with no sound card at all, this quietly does nothing.
struct AudioTrack {
    index: usize,
    decoder: ffmpeg::decoder::Audio,
    /// Turns the decoder's output — planar float, at whatever rate the camera
    /// sends — into the interleaved 16-bit the device was opened for.
    resampler: Option<ffmpeg::software::resampling::Context>,
    device: Option<crate::video::audio::Playback>,
    rate: u32,
    channels: u16,
    /// Set once the device has refused to open, so it is not retried on every
    /// packet for the life of the stream.
    unavailable: bool,
}

impl AudioTrack {
    fn open(input: &ffmpeg::format::context::Input) -> Option<AudioTrack> {
        let stream = input.streams().best(Type::Audio)?;
        let index = stream.index();
        let context =
            ffmpeg::codec::context::Context::from_parameters(stream.parameters()).ok()?;
        let decoder = match context.decoder().audio() {
            Ok(decoder) => decoder,
            Err(err) => {
                // Most likely a codec left out of the bundled ffmpeg.
                debug!("no audio decoder for this stream: {err}");
                return None;
            }
        };

        let rate = decoder.rate();
        let channels = decoder.channels();
        debug!("audio track: {} at {rate} Hz, {channels} channel(s)", decoder.id().name());
        Some(AudioTrack {
            index,
            decoder,
            resampler: None,
            device: None,
            rate,
            channels,
            unavailable: false,
        })
    }

    /// Handle one audio packet.
    ///
    /// When sound is off the packet is dropped without decoding: the point of
    /// muting is to not spend the CPU.
    fn service(&mut self, wanted: bool, packet: &ffmpeg::Packet) {
        if !wanted {
            if self.device.is_some() {
                // Let go of the card, and drop what was queued so switching
                // cameras does not play a second of the previous one.
                if let Some(mut device) = self.device.take() {
                    device.discard();
                }
                self.resampler = None;
            }
            return;
        }
        if self.unavailable {
            return;
        }
        if self.device.is_none() && !self.start_device() {
            return;
        }

        if self.decoder.send_packet(packet).is_err() {
            return;
        }
        let mut decoded = ffmpeg::frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            self.play(&decoded);
        }
    }

    fn start_device(&mut self) -> bool {
        match crate::video::audio::Playback::open(self.rate, self.channels) {
            Some(device) => {
                self.device = Some(device);
                true
            }
            None => {
                self.unavailable = true;
                false
            }
        }
    }

    fn play(&mut self, frame: &ffmpeg::frame::Audio) {
        let Some(device) = self.device.as_mut() else { return };

        if self.resampler.is_none() {
            match ffmpeg::software::resampler(
                (frame.format(), frame.channel_layout(), frame.rate()),
                (
                    ffmpeg::format::Sample::I16(ffmpeg::format::sample::Type::Packed),
                    frame.channel_layout(),
                    device.rate,
                ),
            ) {
                Ok(resampler) => self.resampler = Some(resampler),
                Err(err) => {
                    warn!("cannot convert this audio for playback: {err}");
                    self.unavailable = true;
                    self.device = None;
                    return;
                }
            }
        }
        let Some(resampler) = self.resampler.as_mut() else { return };

        let mut converted = ffmpeg::frame::Audio::empty();
        if resampler.run(frame, &mut converted).is_err() {
            return;
        }
        // Packed 16-bit: one plane holding every channel, interleaved.
        let samples = converted.samples() * converted.channels() as usize;
        let plane: &[i16] = converted.plane(0);
        let end = samples.min(plane.len());
        if !device.write(&plane[..end]) {
            // The device went away — a USB headset unplugged, say.
            self.device = None;
            self.resampler = None;
        }
    }
}

/// One connection, from open to teardown.
///
/// Returns `Ok` when the source ended cleanly and `Err` when it failed; either
/// way the caller reconnects, because that is what a live camera warrants. The
/// error is a string rather than an `ffmpeg::Error` because two of the ways
/// this gives up — a stalled stream, a run of read failures — are this loop's
/// own judgement rather than anything ffmpeg reported.
fn run_once(shared: &Arc<Shared>, source: &StreamSource) -> Result<(), String> {
    let mut input = ffmpeg::format::input_with_dictionary(&source.url, rtsp_options(source))
        .map_err(|err| err.to_string())?;

    let stream = input
        .streams()
        .best(Type::Video)
        .ok_or_else(|| "this stream has no video track".to_string())?;
    let stream_index = stream.index();
    let parameters = stream.parameters();
    let time_base = stream.time_base();

    // Audio is optional: a camera without a microphone, or one whose codec is
    // not built into the bundled ffmpeg, simply plays no sound.
    let mut audio = AudioTrack::open(&input);

    let mut decoder = {
        let mut context = ffmpeg::codec::context::Context::from_parameters(parameters.clone())
            .map_err(|err| err.to_string())?;
        // Slice threading keeps latency down; frame threading pipelines several
        // frames before emitting the first, which is pointless for live view.
        context.set_threading(ffmpeg::threading::Config {
            kind: ffmpeg::threading::Type::Slice,
            count: DECODE_THREADS,
        });
        context.decoder().video().map_err(|err| err.to_string())?
    };

    {
        let mut stats = shared.stats.lock().unwrap();
        stats.width = decoder.width();
        stats.height = decoder.height();
        stats.codec = decoder.id().name().to_string();
    }
    set_state(
        shared,
        StreamState::Playing,
        format!("{}x{}", decoder.width(), decoder.height()),
    );

    let mut scaler: Option<Rescaler> = None;
    let mut recorder: Option<Remuxer> = None;
    let mut warm_packets: Vec<ffmpeg::Packet> = Vec::new();

    let mut frames = 0u32;
    let mut bytes_seen = 0u64;
    let mut window_start = Instant::now();

    // Whether the decoder has been given a keyframe since it last started
    // producing pictures.
    //
    // A warm stream demuxes without decoding, so the decoder never sees the
    // keyframes going past. If such a stream goes hot partway through a GOP
    // with nothing retained, the first packets handed to the decoder are
    // P-frames referencing pictures it has never decoded, and the output is
    // visibly broken until the next keyframe arrives. Withhold frames until
    // there is a keyframe behind them.
    let mut primed = false;
    let mut was_warm = shared.warm.load(Ordering::Relaxed);

    // Read packets by hand rather than through `input.packets()`.
    //
    // That iterator ends only on EOF: every other error is swallowed and the
    // read retried against the same context, forever. Which means a connection
    // that dies for any reason short of a clean close — the machine suspending,
    // the screen locking long enough for the socket to be reset, Wi-Fi
    // dropping, the camera rebooting — never returns from here, so the
    // reconnect loop above never runs and the tile sits on its last frame for
    // good. It also spins the CPU on errors that return immediately, and never
    // looks at the stop flag, so quitting waits on it too.
    //
    // Reading directly costs a few lines and gives all three back: errors are
    // judged, EOF is distinguished from failure, and stop is honoured.
    let mut last_read = Instant::now();
    let mut last_video = Instant::now();
    // When a picture last reached the screen, so a stream that is connected and
    // reading but never producing anything clean can say so instead of sitting
    // on a frozen frame.
    let mut last_good = Instant::now();
    // Reused rather than allocated per packet: `receive_frame` unrefs whatever
    // is in it first, so one frame serves the life of the connection.
    let mut decoded = ffmpeg::frame::Video::empty();

    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }

        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => last_read = Instant::now(),
            // The source ended. For RTSP that is still worth reconnecting for,
            // which is the caller's job.
            Err(ffmpeg::Error::Eof) => break,
            Err(err) => {
                if last_read.elapsed() >= READ_ERROR_GRACE {
                    return Err(format!("read failed: {err}"));
                }
                // Some failures return instantly; without this the retry would
                // spin a core until the grace period expired.
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        }

        // Still connected, still being sent something, but the video has
        // stopped. Checked on every read rather than only on video packets —
        // the whole point is that video packets are what stopped arriving.
        if last_video.elapsed() >= STALL_TIMEOUT {
            return Err(format!(
                "no video for {}s",
                last_video.elapsed().as_secs()
            ));
        }

        if let Some(track) = audio.as_mut() {
            if packet.stream() == track.index {
                // Warm streams stay silent, and so does everything that is not
                // the one camera the user chose to listen to.
                let wanted =
                    shared.audio.load(Ordering::Relaxed) && !shared.warm.load(Ordering::Relaxed);
                track.service(wanted, &packet);
                continue;
            }
        }
        if packet.stream() != stream_index {
            continue;
        }

        // A packet ffmpeg has already flagged as damaged decodes to macroblock
        // garbage. Dropping it costs one frame and keeps it off the screen.
        //
        // Deliberately before the clock below, so it does not count as video
        // having arrived: a stream where *every* packet is damaged is a dead
        // stream, and should end up reconnecting rather than sitting there
        // discarding rubbish indefinitely.
        if packet.is_corrupt() {
            shared.stats.lock().unwrap().dropped += 1;
            continue;
        }

        last_video = Instant::now();
        bytes_seen += packet.size() as u64;

        // --- warm mode: demux only -------------------------------------------
        if shared.warm.load(Ordering::Relaxed) {
            if !was_warm {
                // Going warm stops decoding, so the next hot period has to
                // establish its own reference frame again.
                was_warm = true;
                primed = false;
            }
            buffer_warm(&mut warm_packets, &packet, recorder.is_some());
            service_recording(shared, &mut recorder, &parameters, &packet, time_base);
            continue;
        }
        was_warm = false;

        if !warm_packets.is_empty() {
            // Just went hot: decode the buffered GOP so a picture appears at
            // once rather than after the next keyframe. It starts at a keyframe
            // by construction, so the decoder is primed by the end of it —
            // unless the replay itself was damaged, in which case there is
            // nothing here to build on and the live path waits for a fresh one.
            primed = flush_warm(shared, &mut decoder, &mut scaler, &mut warm_packets);
            if primed {
                last_good = Instant::now();
            } else {
                decoder.flush();
            }
        }

        if !primed {
            if packet.is_key() {
                primed = true;
            } else {
                // Nothing to reference yet. Keep the connection and the
                // recorder fed, but do not put a broken picture on screen.
                service_recording(shared, &mut recorder, &parameters, &packet, time_base);
                continue;
            }
        }

        // --- decode for display ----------------------------------------------
        // This must precede the recording fan-out: remuxing rebases the
        // packet's timestamps and rebinds it to the output stream, and decoding
        // a packet already bound to the muxer fails.
        //
        // Every way this can go wrong ends in the same answer — drop the
        // picture, reset the decoder, and show nothing until a keyframe rebuilds
        // it from scratch — so the three of them are gathered rather than
        // handled where they arise.
        let mut resync: Option<String> = None;
        match decoder.send_packet(&packet) {
            Ok(()) => loop {
                match decoder.receive_frame(&mut decoded) {
                    Ok(()) => {
                        if let Some(why) = damage(&decoded) {
                            resync = Some(why);
                            break;
                        }
                        if let Some(frame) = to_rgba(shared, &mut scaler, &decoded) {
                            publish(shared, frame);
                            frames += 1;
                            last_good = Instant::now();
                        }
                    }
                    // Nothing more to give until it is fed again, which is what
                    // it says after almost every packet.
                    Err(ffmpeg::Error::Other { errno })
                        if errno == ffmpeg::util::error::EAGAIN =>
                    {
                        break
                    }
                    Err(ffmpeg::Error::Eof) => break,
                    Err(err) => {
                        resync = Some(err.to_string());
                        break;
                    }
                }
            },
            Err(err) => resync = Some(err.to_string()),
        }

        if let Some(why) = resync {
            // The decoder's reference frames can no longer be trusted:
            // everything built on them from here is macroblock garbage, and
            // P-frames keep referring back to it, so it does *not* clear on its
            // own. Reset and wait for the next keyframe.
            //
            // Waiting costs nothing that was not already lost. Left running, a
            // decoder with a damaged reference flags every picture it produces
            // until the next keyframe anyway — so the frames this skips are the
            // same frames, and the only difference is that they do not reach
            // the screen on the way past.
            debug!("decoder resync: {why}");
            decoder.flush();
            primed = false;
            shared.stats.lock().unwrap().dropped += 1;
        }

        service_recording(shared, &mut recorder, &parameters, &packet, time_base);

        // --- periodic stats ---------------------------------------------------
        let elapsed = window_start.elapsed().as_secs_f32();
        if elapsed >= 2.0 {
            let mut stats = shared.stats.lock().unwrap();
            stats.fps = frames as f32 / elapsed;
            stats.bitrate_kbps = (bytes_seen as f32 * 8.0 / 1000.0) / elapsed;
            // Zero while pictures are arriving; it only climbs on a stream
            // whose data is damaged faster than keyframes can rebuild it.
            stats.recovering_for = last_good.elapsed().as_secs_f32();
            drop(stats);
            frames = 0;
            bytes_seen = 0;
            window_start = Instant::now();
        }
    }

    if let Some(mut recorder) = recorder.take() {
        recorder.close();
        *shared.recording_to.lock().unwrap() = None;
    }
    Ok(())
}

/// What the decoder says is wrong with a picture it has just produced, if
/// anything.
///
/// This is the check that was missing, and the whole of why damage kept
/// reaching the screen. `send_packet` returning `Ok` says nothing about the
/// picture: h264 and hevc are error-resilient by design, so handed a bitstream
/// with a slice missing they *conceal* it — patch the hole from neighbouring
/// blocks or from the last reference, hand back a frame, and report success.
/// The concealment is what a solid magenta or green rectangle in the middle of
/// a camera is. Only the frame itself carries the fact, in two places:
///
///   * `AV_FRAME_FLAG_CORRUPT`, which the binding exposes as `is_corrupt`;
///   * `decode_error_flags`, which it does not, so it is read off the frame
///     ffmpeg filled in. Set when a reference was missing, when concealment
///     ran, when slices failed to decode, or when the bitstream was invalid —
///     every one of which is visible.
///
/// So a resync waiting on an error return was waiting on something that
/// essentially never comes, and everything the decoder papered over went
/// straight to the tile.
fn damage(frame: &ffmpeg::frame::Video) -> Option<String> {
    if frame.is_corrupt() {
        return Some("the decoder marked the picture corrupt".into());
    }
    // Safety: the frame was just filled in by `receive_frame`, so it is a live
    // AVFrame, and this is a plain read of a scalar field.
    let flags = unsafe { (*frame.as_ptr()).decode_error_flags };
    if flags == 0 {
        return None;
    }
    let mut why: Vec<&str> = Vec::new();
    for (bit, name) in [
        (ffmpeg::ffi::FF_DECODE_ERROR_INVALID_BITSTREAM, "invalid bitstream"),
        (ffmpeg::ffi::FF_DECODE_ERROR_MISSING_REFERENCE, "a reference frame was missing"),
        (ffmpeg::ffi::FF_DECODE_ERROR_CONCEALMENT_ACTIVE, "concealment ran"),
        (ffmpeg::ffi::FF_DECODE_ERROR_DECODE_SLICES, "slices failed to decode"),
    ] {
        if flags & bit != 0 {
            why.push(name);
        }
    }
    Some(why.join(", "))
}

/// Retain packets from the most recent keyframe onward.
///
/// Skipped while recording: the remuxer rebases timestamps and rebinds packets,
/// so a retained copy would no longer decode.
fn buffer_warm(buffer: &mut Vec<ffmpeg::Packet>, packet: &ffmpeg::Packet, recording: bool) {
    if recording {
        buffer.clear();
        return;
    }
    if packet.is_key() {
        buffer.clear();
        buffer.push(packet.clone());
    } else if !buffer.is_empty() {
        buffer.push(packet.clone());
        if buffer.len() > MAX_WARM_PACKETS {
            // No keyframe for a very long time; wait for a fresh one.
            buffer.clear();
        }
    }
}

/// Decode the retained GOP, publishing only the newest frame.
///
/// The intermediate frames are needed to reconstruct the picture but are already
/// stale, so pushing them all at the UI would just be a burst of obsolete images.
///
/// Returns whether the replay left the decoder in a state worth trusting. The
/// retained packets are held from a keyframe and are usually clean, but they are
/// whatever came off the wire — so they get the same judgement as the live path,
/// and a damaged one is a picture withheld rather than a wall that shows
/// garbage for the first second after a camera is brought forward.
fn flush_warm(
    shared: &Arc<Shared>,
    decoder: &mut ffmpeg::decoder::Video,
    scaler: &mut Option<Rescaler>,
    buffer: &mut Vec<ffmpeg::Packet>,
) -> bool {
    let mut newest = None;
    let mut clean = true;
    let mut decoded = ffmpeg::frame::Video::empty();
    for packet in buffer.drain(..) {
        if decoder.send_packet(&packet).is_err() {
            clean = false;
            continue;
        }
        while decoder.receive_frame(&mut decoded).is_ok() {
            if damage(&decoded).is_some() {
                clean = false;
                continue;
            }
            newest = to_rgba(shared, scaler, &decoded);
        }
    }
    // Undamaged by construction — a picture that survived the check above is
    // still worth showing even if something earlier in the GOP did not.
    if let Some(frame) = newest {
        publish(shared, frame);
    }
    clean
}

/// The RGBA conversion, and the frame it converts into.
///
/// They are kept together so they are rebuilt together. The destination is
/// reused across frames rather than allocated per frame: `scaling::Context::run`
/// will allocate one when handed an empty frame, and an expanded camera on its
/// main stream is 14MB a go at 1440p — at 25fps that is a third of a gigabyte a
/// second of allocate-and-free for a buffer whose size never changes.
///
/// Pairing them is what keeps the two in step. `run` checks the destination's
/// size and refuses a mismatch rather than writing past it, so the failure mode
/// if this ever drifts is a picture that stops, not one that is corrupt.
struct Rescaler {
    context: scaling::Context,
    into: ffmpeg::frame::Video,
}

fn to_rgba(
    shared: &Arc<Shared>,
    scaler: &mut Option<Rescaler>,
    decoded: &ffmpeg::frame::Video,
) -> Option<Frame> {
    let (width, height) = (decoded.width(), decoded.height());
    if width == 0 || height == 0 {
        return None;
    }

    // Rebuild the scaler when the source format changes, which happens on a
    // reconnect to a differently configured stream.
    let needs_new = match scaler {
        Some(existing) => {
            existing.context.input().width != width
                || existing.context.input().height != height
                || existing.context.input().format != decoded.format()
        }
        None => true,
    };
    if needs_new {
        *scaler = scaling::Context::get(
            decoded.format(),
            width,
            height,
            Pixel::RGBA,
            width,
            height,
            scaling::Flags::BILINEAR,
        )
        .ok()
        .map(|context| Rescaler {
            context,
            // Left empty so the first run sizes it for the new stream.
            into: ffmpeg::frame::Video::empty(),
        });
    }
    let scaler = scaler.as_mut()?;
    scaler.context.run(decoded, &mut scaler.into).ok()?;

    // ffmpeg pads each row to its own stride; copy row by row so the buffer the
    // UI uploads is tightly packed.
    let stride = scaler.into.stride(0);
    let row_bytes = width as usize * 4;
    let mut packed = Vec::with_capacity(row_bytes * height as usize);
    let data = scaler.into.data(0);
    for row in 0..height as usize {
        let start = row * stride;
        packed.extend_from_slice(&data[start..start + row_bytes]);
    }

    Some(Frame {
        width,
        height,
        rgba: packed,
        sequence: shared.frame_seq.fetch_add(1, Ordering::Relaxed) + 1,
    })
}

fn publish(shared: &Arc<Shared>, frame: Frame) {
    *shared.latest.lock().unwrap() = Some(Arc::new(frame));
}

fn service_recording(
    shared: &Arc<Shared>,
    recorder: &mut Option<Remuxer>,
    parameters: &ffmpeg::codec::Parameters,
    packet: &ffmpeg::Packet,
    time_base: ffmpeg::Rational,
) {
    // Apply any pending request. Runs per packet, so it stays cheap.
    let (request, stop) = {
        let mut guard = shared.record.lock().unwrap();
        (guard.start.take(), std::mem::take(&mut guard.stop))
    };

    if stop {
        if let Some(mut active) = recorder.take() {
            active.close();
        }
        *shared.recording_to.lock().unwrap() = None;
    }

    if let Some(path) = request {
        if recorder.is_none() {
            match Remuxer::new(&path, parameters, time_base) {
                Ok(new) => {
                    info!("recording to {}", path.display());
                    *shared.recording_to.lock().unwrap() = Some(path);
                    *recorder = Some(new);
                }
                Err(err) => error!("could not start recording: {err}"),
            }
        }
    }

    if let Some(active) = recorder.as_mut() {
        if let Err(err) = active.write(packet) {
            error!("recording write failed: {err}");
            let mut done = recorder.take().unwrap();
            done.close();
            *shared.recording_to.lock().unwrap() = None;
        }
    }
}

// ==================================================================== remux

/// Writes incoming packets to MP4 without re-encoding.
///
/// Timestamps are rebased to the first packet so the file starts at zero, and
/// packets before the first keyframe are dropped — otherwise the file opens on a
/// burst of macroblock garbage.
struct Remuxer {
    output: ffmpeg::format::context::Output,
    source_time_base: ffmpeg::Rational,
    target_time_base: ffmpeg::Rational,
    first_dts: Option<i64>,
    seen_keyframe: bool,
    closed: bool,
}

impl Remuxer {
    fn new(
        path: &PathBuf,
        parameters: &ffmpeg::codec::Parameters,
        source_time_base: ffmpeg::Rational,
    ) -> Result<Self, ffmpeg::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut output = ffmpeg::format::output(path)?;
        let mut stream = output.add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None))?;
        stream.set_parameters(parameters.clone());
        let target_time_base = stream.time_base();
        output.write_header()?;

        Ok(Remuxer {
            output,
            source_time_base,
            target_time_base,
            first_dts: None,
            seen_keyframe: false,
            closed: false,
        })
    }

    fn write(&mut self, packet: &ffmpeg::Packet) -> Result<(), ffmpeg::Error> {
        let (Some(pts), Some(dts)) = (packet.pts(), packet.dts()) else {
            return Ok(());
        };
        if !self.seen_keyframe {
            if !packet.is_key() {
                return Ok(());
            }
            self.seen_keyframe = true;
        }
        let first = *self.first_dts.get_or_insert(dts);
        if pts - first < 0 || dts - first < 0 {
            return Ok(());
        }

        let mut copy = packet.clone();
        copy.set_pts(Some(pts - first));
        copy.set_dts(Some(dts - first));
        copy.set_position(-1);
        copy.set_stream(0);
        copy.rescale_ts(self.source_time_base, self.target_time_base);
        copy.write_interleaved(&mut self.output)
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if let Err(err) = self.output.write_trailer() {
            debug!("closing recording: {err}");
        }
    }
}

impl Drop for Remuxer {
    fn drop(&mut self) {
        self.close();
    }
}

/// Retire a worker without blocking the caller.
///
/// Dropping a `StreamWorker` joins its thread, which can take as long as the
/// socket timeout when a camera has stopped answering. The UI must never wait
/// for that — in the Python client, doing so froze the window for 3s per tile
/// and then aborted the process.
pub struct Retirer {
    pending: Arc<Mutex<HashMap<u64, JoinHandle<()>>>>,
    next_id: AtomicU64,
}

impl Default for Retirer {
    fn default() -> Self {
        Retirer {
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(0),
        }
    }
}

impl Retirer {
    pub fn retire(&self, worker: StreamWorker) {
        worker.stop();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let pending = Arc::clone(&self.pending);
        let handle = std::thread::Builder::new()
            .name("retire-stream".into())
            .spawn(move || {
                drop(worker); // joins the stream thread here, off the UI thread
                pending.lock().unwrap().remove(&id);
            })
            .expect("failed to spawn retirement thread");
        self.pending.lock().unwrap().insert(id, handle);
    }

    pub fn pending(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Wait for outstanding retirements, for shutdown only.
    pub fn drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.pending.lock().unwrap().is_empty() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let left = self.pending();
        if left > 0 {
            warn!("{left} video worker(s) still running at shutdown");
        }
        left == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera that has just come back should not be made to wait out a
    /// backoff earned by an earlier outage.
    #[test]
    fn a_connection_that_worked_resets_the_backoff() {
        let long = HEALTHY_CONNECTION + Duration::from_secs(1);
        // However high the counter had climbed.
        assert_eq!(next_attempt(5, long, true), 0);
        assert_eq!(next_attempt(5, long, false), 0);
        assert_eq!(RECONNECT_DELAYS[0], 1, "and so gets the quick retry");
    }

    /// A camera that is simply not there must be backed off, or it gets
    /// hammered all night.
    #[test]
    fn repeated_quick_failures_back_off() {
        let quick = Duration::from_millis(200);
        let mut attempt = 0;
        for expected in [1, 2, 3, 4] {
            attempt = next_attempt(attempt, quick, true);
            assert_eq!(attempt, expected);
        }
        // And the delay it selects climbs with it, up to the cap.
        assert!(RECONNECT_DELAYS[1] > RECONNECT_DELAYS[0]);
        assert_eq!(
            RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)],
            RECONNECT_DELAYS[4]
        );
    }

    /// The check that was missing, and the whole of why damage reached the
    /// screen: a decoder that conceals a hole and reports success is the
    /// ordinary case rather than the exceptional one, so what has to be judged
    /// is the picture and not the return value.
    #[test]
    fn a_picture_the_decoder_patched_up_is_refused() {
        let mut frame = ffmpeg::frame::Video::empty();
        assert!(damage(&frame).is_none(), "a clean picture goes to screen");

        // Every kind of damage ffmpeg reports is refused, and said in words —
        // these end up in the log, where "2" would be no use to anyone.
        for (bit, expected) in [
            (ffmpeg::ffi::FF_DECODE_ERROR_MISSING_REFERENCE, "reference"),
            (ffmpeg::ffi::FF_DECODE_ERROR_CONCEALMENT_ACTIVE, "concealment"),
            (ffmpeg::ffi::FF_DECODE_ERROR_INVALID_BITSTREAM, "bitstream"),
            (ffmpeg::ffi::FF_DECODE_ERROR_DECODE_SLICES, "slices"),
        ] {
            unsafe {
                (*frame.as_mut_ptr()).decode_error_flags = bit;
            }
            let why = damage(&frame).expect("this picture is damaged");
            assert!(why.contains(expected), "flag {bit} came back as {why:?}");
        }

        // Several at once read as several, rather than as whichever was checked
        // first.
        unsafe {
            (*frame.as_mut_ptr()).decode_error_flags =
                ffmpeg::ffi::FF_DECODE_ERROR_MISSING_REFERENCE
                    | ffmpeg::ffi::FF_DECODE_ERROR_CONCEALMENT_ACTIVE;
        }
        let why = damage(&frame).unwrap();
        assert!(why.contains("reference") && why.contains("concealment"), "{why:?}");

        // And the frame's own flag, which is the other place ffmpeg says so.
        unsafe {
            (*frame.as_mut_ptr()).decode_error_flags = 0;
            (*frame.as_mut_ptr()).flags = ffmpeg::ffi::AV_FRAME_FLAG_CORRUPT;
        }
        assert!(damage(&frame).is_some(), "the corrupt flag alone is enough");
    }

    /// Sixteen decoders each taking a core's worth of threads is what makes
    /// every read loop late — and a late reader on an RTSP socket is a source
    /// that throttles and starts dropping, which arrives as the damage above.
    #[test]
    fn a_stream_does_not_claim_the_machine() {
        assert!(DECODE_THREADS >= 1, "zero would mean one thread per core");
        assert!(
            DECODE_THREADS * 16 <= 32,
            "a full wall would ask for {} threads",
            DECODE_THREADS * 16
        );
    }

    /// A clean end is not a failure, so it does not escalate.
    #[test]
    fn a_clean_end_does_not_escalate() {
        assert_eq!(next_attempt(3, Duration::from_millis(200), false), 0);
    }

    /// The counter is only ever used to index the delay table, so it must not
    /// be able to run away.
    #[test]
    fn the_backoff_is_capped() {
        let quick = Duration::from_millis(1);
        let mut attempt = 0;
        for _ in 0..1000 {
            attempt = next_attempt(attempt, quick, true);
        }
        let delay = RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)];
        assert_eq!(delay, *RECONNECT_DELAYS.last().unwrap());
    }
}

#[cfg(test)]
mod audio_tests {
    use super::*;

    /// Watch one stream for a while and report what it did.
    ///
    /// The thing this is for cannot be reproduced synthetically. Whether a
    /// broken RTSP connection surfaces as EOF or as an errno is the demuxer's
    /// business, and the raw demuxers available to a test here answer
    /// differently from the RTSP one — so a fake server proves nothing about
    /// the case that matters. What does prove it is a real camera and the two
    /// events worth watching for: a decode resync, and a reconnect.
    ///
    /// Run it against a camera and then break the connection on purpose —
    /// suspend the machine, pull the network, power-cycle the camera. It must
    /// come back on its own.
    ///
    ///   KESTREL_TEST_RTSP=rtsp://… KESTREL_TEST_MINUTES=15 \
    ///     cargo test -- --ignored --nocapture watch_one_stream
    #[test]
    #[ignore]
    fn watch_one_stream() {
        let Ok(url) = std::env::var("KESTREL_TEST_RTSP") else {
            eprintln!("KESTREL_TEST_RTSP not set");
            return;
        };
        let minutes: u64 = std::env::var("KESTREL_TEST_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(12);

        let worker = StreamWorker::start(StreamSource::new(url), "watch", false);
        let started = Instant::now();
        let deadline = started + Duration::from_secs(minutes * 60);

        let (mut frames, mut reconnects, mut last_state) = (0u64, 0u32, StreamState::Idle);
        let mut last_sequence = 0u64;
        let mut last_report = Instant::now();

        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));

            if let Some(frame) = worker.latest_frame() {
                if frame.sequence != last_sequence {
                    last_sequence = frame.sequence;
                    frames += 1;
                }
            }
            let (state, detail) = worker.state();
            if state != last_state {
                // A reconnect is the event this exists to catch: after the
                // connection is broken, the stream has to come back without
                // anyone touching it.
                if matches!(state, StreamState::Reconnecting | StreamState::Error) {
                    reconnects += 1;
                }
                println!("  {:>6.1}s  {state:?} {detail}", started.elapsed().as_secs_f32());
                last_state = state;
            }

            if last_report.elapsed() >= Duration::from_secs(30) {
                let stats = worker.stats();
                println!(
                    "  {frames} frames  {:.0} fps  {:.1} Mb/s  {} resync(s)  \
                     {:.0}s since a clean picture  {reconnects} reconnect(s)",
                    stats.fps,
                    stats.bitrate_kbps / 1000.0,
                    stats.dropped,
                    stats.recovering_for,
                );
                last_report = Instant::now();
            }
        }

        let stats = worker.stats();
        println!("\n  {frames} frames over {minutes} minute(s)");
        // Resyncs are the number to watch for artefacting. A handful over a
        // quarter of an hour is an ordinary link; a steady stream of them means
        // the source is losing more data than keyframes can rebuild, and no
        // amount of care at this end will make that picture whole.
        println!("  {} resync(s), {reconnects} reconnect(s)", stats.dropped);
        assert!(frames > 0, "no video arrived at all");
        assert_eq!(
            worker.state().0,
            StreamState::Playing,
            "the stream did not end up playing — it never recovered"
        );
    }

    /// End-to-end: connect, find the audio track, decode it and play it.
    ///
    /// Ignored by default — it needs a camera and a sound card. Run with
    /// KESTREL_TEST_RTSP set to a stream URL:
    ///   cargo test --  --ignored audio_from_a_real_camera
    #[test]
    #[ignore]
    fn audio_from_a_real_camera() {
        let Ok(url) = std::env::var("KESTREL_TEST_RTSP") else {
            eprintln!("KESTREL_TEST_RTSP not set");
            return;
        };
        ffmpeg::init().expect("ffmpeg");
        let source = StreamSource::new(url);
        let mut input = ffmpeg::format::input_with_dictionary(&source.url, rtsp_options(&source))
            .expect("the stream should open");
        let mut track = AudioTrack::open(&input).expect("the camera should publish audio");
        println!("audio: {} Hz, {} channel(s)", track.rate, track.channels);

        let index = track.index;
        let mut played = 0usize;
        for (stream, packet) in input.packets().take(400) {
            if stream.index() != index {
                continue;
            }
            track.service(true, &packet);
            played += 1;
            if played >= 40 {
                break;
            }
        }
        assert!(played > 0, "no audio packets arrived");
        assert!(
            track.device.is_some(),
            "the device should still be open after playing {played} packets"
        );
        println!("played {played} audio packets");
    }
}
