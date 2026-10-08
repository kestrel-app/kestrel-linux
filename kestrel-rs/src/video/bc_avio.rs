//! A custom ffmpeg input that reads from a byte source instead of a URL.
//!
//! Reolink's P2P video is not RTSP: it is a raw H.264/H.265 elementary stream
//! produced by [`crate::api::vendor::baichuan::video`]. ffmpeg-next can only open an
//! input by path or URL, so to decode that stream in-process we build the
//! `AVFormatContext` by hand with a custom `AVIOContext` whose read callback pulls
//! from a [`ByteSource`], and wrap it back into a safe ffmpeg-next [`Input`] for the
//! ordinary decode loop.
//!
//! This is the one piece of the P2P path that is `unsafe` FFI. The lifecycle is the
//! fiddly part: with `AVFMT_FLAG_CUSTOM_IO` set, `avformat_close_input` does **not**
//! free the `AVIOContext` we supplied, so [`AvioInput`] frees it (and its buffer and
//! the boxed source) itself, in order, on drop. The read-callback plumbing is covered
//! by the test below, which decodes a real (synthetic) H.264 stream end to end.
//!
// Staged: validated by its own tests and ready for the StreamWorker wiring that will
// decode a UID camera, but not reached from the binary yet, so dead-code warnings are
// allowed for this module alone rather than left as noise.
#![allow(dead_code)]

use std::ffi::{c_void, CString};
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};

use ffmpeg_next as ffmpeg;
use ffmpeg::format::context::Input;

/// A source of elementary-stream bytes for the decoder to pull from.
///
/// `read` fills `buf` and returns how many bytes it wrote; `0` means end of stream.
/// For a live camera this blocks until bytes arrive (or the stream stops); for a test
/// it reads from a slice.
pub trait ByteSource: Send {
    fn read(&mut self, buf: &mut [u8]) -> usize;
}

/// Size of the buffer ffmpeg reads through. 32 KiB is a comfortable amount for video.
const IO_BUFFER: usize = 32 * 1024;

/// An ffmpeg [`Input`] backed by a [`ByteSource`] through a custom `AVIOContext`.
pub struct AvioInput {
    // Dropped first (runs `avformat_close_input`), before we free the AVIO below.
    input: ManuallyDrop<Input>,
    avio: *mut ffmpeg::sys::AVIOContext,
    // The boxed source, freed last so the read callback can never run against it after.
    source: *mut Box<dyn ByteSource>,
}

// The source is `Send`; the raw pointers are owned solely by this struct.
unsafe impl Send for AvioInput {}

impl AvioInput {
    /// Open an input that reads from `source`. `format` forces the demuxer (e.g.
    /// `"h264"` or `"hevc"`); `None` lets ffmpeg probe the raw stream, which works
    /// when it begins at a keyframe with its parameter sets.
    pub fn open(source: Box<dyn ByteSource>, format: Option<&str>) -> Result<AvioInput, String> {
        ffmpeg::init().map_err(|e| e.to_string())?;
        let source = Box::into_raw(Box::new(source));

        unsafe {
            let fmt_ctx = ffmpeg::sys::avformat_alloc_context();
            if fmt_ctx.is_null() {
                drop(Box::from_raw(source));
                return Err("avformat_alloc_context failed".into());
            }

            let buffer = ffmpeg::sys::av_malloc(IO_BUFFER) as *mut u8;
            let avio = ffmpeg::sys::avio_alloc_context(
                buffer,
                IO_BUFFER as i32,
                0, // read-only
                source as *mut c_void,
                Some(read_packet),
                None,
                None,
            );
            if avio.is_null() {
                ffmpeg::sys::avformat_free_context(fmt_ctx);
                drop(Box::from_raw(source));
                return Err("avio_alloc_context failed".into());
            }

            (*fmt_ctx).pb = avio;
            (*fmt_ctx).flags |= ffmpeg::sys::AVFMT_FLAG_CUSTOM_IO;

            // Force the demuxer when asked; otherwise null lets ffmpeg probe.
            let fmt_name = match format {
                Some(f) => Some(CString::new(f).map_err(|_| "bad format name")?),
                None => None,
            };
            let input_format = match &fmt_name {
                Some(name) => {
                    let f = ffmpeg::sys::av_find_input_format(name.as_ptr());
                    if f.is_null() {
                        free_avio(avio);
                        ffmpeg::sys::avformat_free_context(fmt_ctx);
                        drop(Box::from_raw(source));
                        return Err("ffmpeg does not know that demuxer".into());
                    }
                    f
                }
                None => std::ptr::null(),
            };

            // avformat_open_input takes the context by ** p; on failure it frees it.
            let mut ps = fmt_ctx;
            let rc = ffmpeg::sys::avformat_open_input(
                &mut ps,
                std::ptr::null(),
                input_format,
                std::ptr::null_mut(),
            );
            if rc < 0 {
                // open_input freed the format context (and with it stopped using pb),
                // so free the AVIO and the source ourselves.
                free_avio(avio);
                drop(Box::from_raw(source));
                return Err(format!("avformat_open_input failed ({rc})"));
            }

            if ffmpeg::sys::avformat_find_stream_info(ps, std::ptr::null_mut()) < 0 {
                ffmpeg::sys::avformat_close_input(&mut ps);
                free_avio(avio);
                drop(Box::from_raw(source));
                return Err("avformat_find_stream_info failed".into());
            }

            Ok(AvioInput {
                input: ManuallyDrop::new(Input::wrap(ps)),
                avio,
                source,
            })
        }
    }
}

impl Deref for AvioInput {
    type Target = Input;
    fn deref(&self) -> &Input {
        &self.input
    }
}

impl DerefMut for AvioInput {
    fn deref_mut(&mut self) -> &mut Input {
        &mut self.input
    }
}

impl Drop for AvioInput {
    fn drop(&mut self) {
        unsafe {
            // Close the format context first (does not touch our custom pb)...
            ManuallyDrop::drop(&mut self.input);
            // ...then free the AVIO it was reading through, then the source.
            free_avio(self.avio);
            drop(Box::from_raw(self.source));
        }
    }
}

/// Free an `AVIOContext` and the buffer it currently holds. ffmpeg may have replaced
/// the buffer we supplied, so free the one the context points at now.
unsafe fn free_avio(avio: *mut ffmpeg::sys::AVIOContext) {
    if avio.is_null() {
        return;
    }
    ffmpeg::sys::av_free((*avio).buffer as *mut c_void);
    let mut avio = avio;
    ffmpeg::sys::avio_context_free(&mut avio);
}

/// The C read callback: pull up to `buf_size` bytes from the boxed [`ByteSource`].
unsafe extern "C" fn read_packet(opaque: *mut c_void, buf: *mut u8, buf_size: i32) -> i32 {
    if opaque.is_null() || buf.is_null() || buf_size <= 0 {
        return ffmpeg::sys::AVERROR_EOF;
    }
    let source = &mut *(opaque as *mut Box<dyn ByteSource>);
    let out = std::slice::from_raw_parts_mut(buf, buf_size as usize);
    match source.read(out) {
        0 => ffmpeg::sys::AVERROR_EOF,
        n => n as i32,
    }
}

/// A [`ByteSource`] over an in-memory slice, for tests and for decoding a captured
/// stream. Returns 0 (EOF) once exhausted.
pub struct SliceSource {
    data: Vec<u8>,
    pos: usize,
}

impl SliceSource {
    pub fn new(data: Vec<u8>) -> Self {
        SliceSource { data, pos: 0 }
    }
}

impl ByteSource for SliceSource {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        let n = (self.data.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        n
    }
}

/// A [`ByteSource`] fed by a channel — the bridge from a live Baichuan video feeder
/// thread (which polls the session and sends elementary-stream chunks) to the decoder
/// thread (which pulls through the custom AVIO). `read` blocks for the next chunk and
/// returns 0 only once the sender has gone, which ends the stream cleanly.
pub struct ChannelSource {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    leftover: Vec<u8>,
    pos: usize,
    /// With a limit, a feed silent for that long reads as the end of the stream.
    stall_limit: Option<std::time::Duration>,
    /// Raised when the decoder lets go of this source, so a feeder that has nothing
    /// to send (and so would never see the channel close) can still stop.
    closed: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Whatever must live as long as this source does — the shared connection the
    /// bytes come from.
    _holding: Option<Box<dyn Send>>,
}

impl ChannelSource {
    pub fn new(rx: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        ChannelSource {
            rx,
            leftover: Vec::new(),
            pos: 0,
            stall_limit: None,
            closed: None,
            _holding: None,
        }
    }

    /// Keep `value` alive until this source is dropped.
    pub fn holding(mut self, value: Box<dyn Send>) -> Self {
        self._holding = Some(value);
        self
    }

    /// Raise `flag` when this source is dropped.
    pub fn signalling_close(mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.closed = Some(flag);
        self
    }

    /// End the stream if no chunk arrives for `limit`, rather than waiting forever
    /// on a feeder whose device has stopped sending.
    pub fn with_stall_limit(mut self, limit: std::time::Duration) -> Self {
        self.stall_limit = Some(limit);
        self
    }
}

impl Drop for ChannelSource {
    fn drop(&mut self) {
        if let Some(flag) = &self.closed {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl ByteSource for ChannelSource {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        if self.pos >= self.leftover.len() {
            let next = match self.stall_limit {
                Some(limit) => self.rx.recv_timeout(limit).ok(),
                None => self.rx.recv().ok(),
            };
            match next {
                Some(chunk) => {
                    self.leftover = chunk;
                    self.pos = 0;
                }
                None => return 0, // feeder gone, or silent too long: end of stream
            }
        }
        let n = (self.leftover.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.leftover[self.pos..self.pos + n]);
        self.pos += n;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg::media::Type;

    /// Decode a real (synthetic) H.264 elementary stream through the custom AVIO,
    /// proving the FFI plumbing, the forced demuxer, and the decode path all work.
    #[test]
    fn decodes_h264_through_custom_avio() {
        let bytes = include_bytes!("testdata/synthetic_h264.bin").to_vec();
        let source = Box::new(SliceSource::new(bytes));
        let mut input = AvioInput::open(source, Some("h264")).expect("open custom avio input");

        let stream = input.streams().best(Type::Video).expect("a video stream");
        let index = stream.index();
        let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .unwrap()
            .decoder()
            .video()
            .expect("h264 decoder");

        let mut decoded = 0;
        let mut frame = ffmpeg::frame::Video::empty();
        // Collect packets first so the immutable borrow from `.packets()` ends before
        // the decoder (which also borrows nothing of `input`) runs.
        let packets: Vec<_> = input
            .packets()
            .filter(|(s, _)| s.index() == index)
            .map(|(_, p)| p)
            .collect();
        for packet in &packets {
            if decoder.send_packet(packet).is_ok() {
                while decoder.receive_frame(&mut frame).is_ok() {
                    decoded += 1;
                }
            }
        }
        decoder.send_eof().ok();
        while decoder.receive_frame(&mut frame).is_ok() {
            decoded += 1;
        }

        assert_eq!(decoder.width(), 320);
        assert_eq!(decoder.height(), 240);
        assert!(decoded > 0, "no frames decoded through the custom AVIO");
    }

    #[test]
    fn slice_source_reads_then_reports_eof() {
        let mut s = SliceSource::new(b"abcdef".to_vec());
        let mut buf = [0u8; 4];
        assert_eq!(s.read(&mut buf), 4);
        assert_eq!(&buf, b"abcd");
        assert_eq!(s.read(&mut buf), 2);
        assert_eq!(&buf[..2], b"ef");
        assert_eq!(s.read(&mut buf), 0, "EOF");
    }

    #[test]
    fn channel_source_spans_chunks_and_ends_when_sender_drops() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(b"ABC".to_vec()).unwrap();
        tx.send(b"DE".to_vec()).unwrap();
        drop(tx);
        let mut s = ChannelSource::new(rx);
        let mut buf = [0u8; 2];
        assert_eq!(s.read(&mut buf), 2);
        assert_eq!(&buf, b"AB");
        assert_eq!(s.read(&mut buf), 1); // rest of first chunk
        assert_eq!(buf[0], b'C');
        assert_eq!(s.read(&mut buf), 2); // second chunk
        assert_eq!(&buf, b"DE");
        assert_eq!(s.read(&mut buf), 0, "sender dropped -> EOF");
    }

    /// A feed that goes quiet ends the stream after the limit instead of blocking
    /// forever, even though its sender is still alive.
    #[test]
    fn a_silent_feed_ends_after_the_stall_limit() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(b"AB".to_vec()).unwrap();
        let mut s = ChannelSource::new(rx).with_stall_limit(std::time::Duration::from_millis(100));
        let mut buf = [0u8; 4];
        assert_eq!(s.read(&mut buf), 2);
        let started = std::time::Instant::now();
        assert_eq!(s.read(&mut buf), 0, "silence reads as the end");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        drop(tx);
    }
}
