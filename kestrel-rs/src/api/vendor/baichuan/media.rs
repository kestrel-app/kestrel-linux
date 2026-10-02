//! Turning the device's video frames back into a byte stream ffmpeg can read.
//!
//! Over the control channel video does **not** arrive as RTSP. It comes as
//! "BCMedia" frames: each a small magic-tagged header followed by a slice of an
//! H.264 or H.265 elementary stream (or an audio frame). This module recognises
//! those frames and concatenates the video payloads into an Annex-B elementary
//! stream — the shape an H.26x decoder expects.
//!
//! **Verification status.** The frame *parser* ([`demux`]) is pure and tested
//! below against synthetic frames. Of the magics, only the info-frame magic
//! (`1001`) was confirmed present in the official app's binary from this machine;
//! the per-frame video/audio magics are the understood protocol values and are
//! **not** individually confirmed here. The final hop — feeding this elementary
//! stream into the in-process `ffmpeg_next` decoder through a custom AVIO read
//! callback — is **not implemented**: `video::stream` currently opens an RTSP URL,
//! and a byte-fed input is a separate piece of work. Both are in `docs/untested.md`.
//! See [`super`].

/// A recognised BCMedia frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A slice of the video elementary stream. `keyframe` marks an IDR/I-frame.
    Video { keyframe: bool, data: Vec<u8> },
    /// A slice of the audio stream, left to the caller to route.
    Audio(Vec<u8>),
    /// A stream-info header (resolution, codec); carried for completeness.
    Info(Vec<u8>),
}

// The four-byte magics that open each frame kind. `INFO_V1` (`1001`) is the one
// confirmed present in the shipped binary from here; the others are the understood
// protocol values. All are matched as byte strings so endianness is not a trap.
const MAGIC_IFRAME: &[u8; 4] = b"00dc";
const MAGIC_PFRAME: &[u8; 4] = b"01dc";
const MAGIC_AAC: &[u8; 4] = b"05wb";
const MAGIC_ADPCM: &[u8; 4] = b"00wb";
const MAGIC_INFO_V1: &[u8; 4] = b"1001";
const MAGIC_INFO_V2: &[u8; 4] = b"1002";

/// How many header bytes follow each magic before the payload length and body.
/// Video frames carry a longer header (timestamps, codec) than audio; these are
/// the understood sizes and are part of what a capture would confirm.
const VIDEO_HEADER_LEN: usize = 32;
const AUDIO_HEADER_LEN: usize = 8;
const INFO_HEADER_LEN: usize = 32;

/// Parse as many whole frames as `buf` holds, returning them and the number of
/// bytes consumed. Trailing partial frames are left for the next call.
///
/// The payload length is read from the four bytes immediately after the magic, as
/// a little-endian u32 — the field every BCMedia frame carries. A frame whose
/// declared length overruns the buffer is treated as "not yet complete".
pub fn demux(buf: &[u8]) -> (Vec<Frame>, usize) {
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let magic = &buf[pos..pos + 4];
        let payload_len =
            u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;

        let (header_len, make): (usize, fn(&[u8]) -> Frame) = match magic {
            m if m == MAGIC_IFRAME => (VIDEO_HEADER_LEN, |d| Frame::Video {
                keyframe: true,
                data: d.to_vec(),
            }),
            m if m == MAGIC_PFRAME => (VIDEO_HEADER_LEN, |d| Frame::Video {
                keyframe: false,
                data: d.to_vec(),
            }),
            m if m == MAGIC_AAC || m == MAGIC_ADPCM => {
                (AUDIO_HEADER_LEN, |d| Frame::Audio(d.to_vec()))
            }
            m if m == MAGIC_INFO_V1 || m == MAGIC_INFO_V2 => {
                (INFO_HEADER_LEN, |d| Frame::Info(d.to_vec()))
            }
            // Unrecognised: we cannot know the frame's length, so stop and let the
            // caller resynchronise rather than guess past it.
            _ => break,
        };

        let start = pos + header_len;
        let end = start + payload_len;
        if end > buf.len() {
            break; // frame not fully arrived yet
        }
        frames.push(make(&buf[start..end]));
        pos = end;
    }
    (frames, pos)
}

/// Accumulates demuxed video into an Annex-B elementary stream.
///
/// Successive video payloads are concatenated; this is what would be handed to the
/// decoder. Audio and info frames are dropped here (audio routing is a later
/// concern). The accumulator keeps a small buffer of undecoded tail bytes so a
/// frame split across two transport reads is not lost.
#[derive(Default)]
pub struct VideoStream {
    pending: Vec<u8>,
    elementary: Vec<u8>,
    seen_keyframe: bool,
}

impl VideoStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw transport bytes; appends any newly-complete video payloads to the
    /// elementary stream. Video is dropped until the first keyframe, so a decoder
    /// does not start mid-GOP on garbage.
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let (frames, used) = demux(&self.pending);
        for frame in frames {
            if let Frame::Video { keyframe, data } = frame {
                if keyframe {
                    self.seen_keyframe = true;
                }
                if self.seen_keyframe {
                    self.elementary.extend_from_slice(&data);
                }
            }
        }
        self.pending.drain(..used);
    }

    /// Take the elementary-stream bytes accumulated so far.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.elementary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_frame(magic: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(magic);
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(&[0u8; VIDEO_HEADER_LEN - 8]); // rest of header
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn demuxes_an_iframe_then_a_pframe() {
        let mut stream = video_frame(MAGIC_IFRAME, b"IDR-data");
        stream.extend_from_slice(&video_frame(MAGIC_PFRAME, b"P-data"));
        let (frames, used) = demux(&stream);
        assert_eq!(used, stream.len());
        assert_eq!(
            frames,
            vec![
                Frame::Video {
                    keyframe: true,
                    data: b"IDR-data".to_vec()
                },
                Frame::Video {
                    keyframe: false,
                    data: b"P-data".to_vec()
                },
            ]
        );
    }

    #[test]
    fn a_partial_trailing_frame_is_left_unconsumed() {
        let whole = video_frame(MAGIC_IFRAME, b"keyframe-payload");
        let truncated = &whole[..whole.len() - 3];
        let (frames, used) = demux(truncated);
        assert!(frames.is_empty());
        assert_eq!(used, 0);
    }

    #[test]
    fn video_is_dropped_until_the_first_keyframe() {
        let mut s = VideoStream::new();
        s.push(&video_frame(MAGIC_PFRAME, b"orphan-p"));
        assert!(s.take().is_empty(), "no keyframe yet, nothing emitted");

        s.push(&video_frame(MAGIC_IFRAME, b"KEY"));
        s.push(&video_frame(MAGIC_PFRAME, b"then-p"));
        assert_eq!(s.take(), b"KEYthen-p");
    }

    #[test]
    fn a_frame_split_across_two_pushes_is_reassembled() {
        let frame = video_frame(MAGIC_IFRAME, b"splitpayload");
        let (head, tail) = frame.split_at(frame.len() - 4);
        let mut s = VideoStream::new();
        s.push(head);
        assert!(s.take().is_empty());
        s.push(tail);
        assert_eq!(s.take(), b"splitpayload");
    }

    #[test]
    fn unrecognised_bytes_stop_the_demuxer_for_resync() {
        let junk = b"NOPExxxxxxxx";
        let (frames, used) = demux(junk);
        assert!(frames.is_empty());
        assert_eq!(used, 0);
    }
}
