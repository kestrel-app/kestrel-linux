//! Turning the device's video frames back into a byte stream a decoder can read.
//!
//! Over the control channel video does **not** arrive as RTSP. It comes as
//! "BCMedia" frames: each a magic-tagged header followed by a slice of an H.264 or
//! H.265 elementary stream (or an audio frame). This module recognises those frames
//! and concatenates the video payloads into the elementary stream an H.26x decoder
//! expects.
//!
//! **Verification status.** The frame format and the H.265 codec were confirmed live
//! against the NVR: a `Preview` request produced an `INFO_V1` frame then an `IFRAME`
//! whose decrypted header read `00dc` `H265`. The header layout (magic, type,
//! payload size, additional-header size, 8-byte padding) matches both that capture
//! and the neolink reference. What this module does *not* do is the layer above it —
//! stripping the per-message extension and decrypting the FullAes-encrypted prefix of
//! each video message — which belongs to the session; nor the layer below — feeding
//! the elementary stream into the decoder through a custom ffmpeg input. Both are
//! noted in `docs/untested.md`. See [`super`].

/// Which codec a video frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

/// A recognised BCMedia frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A slice of the video elementary stream. `keyframe` marks an I-frame.
    Video {
        codec: VideoCodec,
        keyframe: bool,
        /// The device's timestamp for the frame, in microseconds. Wraps at u32.
        micros: u32,
        data: Vec<u8>,
    },
    /// A slice of the audio stream, left to the caller to route.
    Audio(Vec<u8>),
    /// A stream-info header, carrying the picture dimensions.
    Info { width: u32, height: u32 },
}

// Frame magics. The I-frame and P-frame magics are ranges: the low digit varies (a
// fragment marker), so any value in the range is that frame kind.
const INFO_V1: u32 = 0x3130_3031; // "1001"
const INFO_V2: u32 = 0x3230_3031; // "1002"
const IFRAME: u32 = 0x6364_3030; // "00dc"
const IFRAME_LAST: u32 = 0x6364_3039;
const PFRAME: u32 = 0x6364_3130; // "01dc"
const PFRAME_LAST: u32 = 0x6364_3139;
const AAC: u32 = 0x6277_3530; // "05wb"
const ADPCM: u32 = 0x6277_3030; // "00wb"

/// BCMedia pads each frame's data to an 8-byte boundary.
const PAD: usize = 8;

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn pad_for(len: usize) -> usize {
    match len % PAD {
        0 => 0,
        n => PAD - n,
    }
}

/// Whether `bytes` begins with a BCMedia frame this demuxer knows. A replay opens
/// with a message that is not one (the stream-info header); this is how it is
/// told apart from footage.
pub fn starts_with_frame(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && matches!(
            le32(bytes, 0),
            INFO_V1 | INFO_V2 | IFRAME..=IFRAME_LAST | PFRAME..=PFRAME_LAST | AAC | ADPCM
        )
}

/// Parse as many whole frames as `buf` holds, returning them and the number of bytes
/// consumed. A trailing partial frame, or an unrecognised magic, stops the walk so
/// the caller keeps the remainder for next time.
pub fn demux(buf: &[u8]) -> (Vec<Frame>, usize) {
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 4 <= buf.len() {
        let rest = &buf[pos..];
        let magic = le32(rest, 0);
        let consumed = match magic {
            INFO_V1 | INFO_V2 => parse_info(rest, &mut frames),
            IFRAME..=IFRAME_LAST => parse_video(rest, VideoKind::I, &mut frames),
            PFRAME..=PFRAME_LAST => parse_video(rest, VideoKind::P, &mut frames),
            AAC | ADPCM => parse_audio(rest, &mut frames),
            _ => None, // desynchronised or not yet enough bytes to know
        };
        match consumed {
            Some(n) => pos += n,
            None => break,
        }
    }
    (frames, pos)
}

enum VideoKind {
    I,
    P,
}

/// `magic(4) size(4) width(4) height(4) ...` — the size field is the frame's **total**
/// length (e.g. 32), not the length of what follows it. Confirmed live (a 640×360
/// sub-stream INFO frame read size 32 with width/height at offsets 8 and 12).
fn parse_info(b: &[u8], out: &mut Vec<Frame>) -> Option<usize> {
    if b.len() < 8 {
        return None;
    }
    let total = le32(b, 4) as usize;
    if total < 16 || b.len() < total {
        return None;
    }
    out.push(Frame::Info {
        width: le32(b, 8),
        height: le32(b, 12),
    });
    Some(total)
}

/// `magic(4) type(4) payload_size(4) additional_header_size(4) microseconds(4)
/// unknown(4) [additional_header bytes] [payload_size data] [pad to 8]`.
fn parse_video(b: &[u8], kind: VideoKind, out: &mut Vec<Frame>) -> Option<usize> {
    const BASE: usize = 24; // magic + type + payload_size + addl_size + microseconds + unknown
    if b.len() < BASE {
        return None;
    }
    let codec = match &b[4..8] {
        b"H264" => VideoCodec::H264,
        b"H265" => VideoCodec::H265,
        _ => return None, // unrecognised: let the caller resynchronise
    };
    let payload_size = le32(b, 8) as usize;
    let addl_size = le32(b, 12) as usize;
    let data_start = BASE + addl_size;
    let pad = pad_for(payload_size);
    let total = data_start + payload_size + pad;
    if b.len() < total {
        return None;
    }
    out.push(Frame::Video {
        codec,
        keyframe: matches!(kind, VideoKind::I),
        micros: le32(b, 16),
        data: b[data_start..data_start + payload_size].to_vec(),
    });
    Some(total)
}

/// `magic(4) payload_size(u16) payload_size_dup(u16) [data] [pad to 8]`.
fn parse_audio(b: &[u8], out: &mut Vec<Frame>) -> Option<usize> {
    if b.len() < 8 {
        return None;
    }
    let payload_size = le16(b, 4) as usize;
    let pad = pad_for(payload_size);
    let total = 8 + payload_size + pad;
    if b.len() < total {
        return None;
    }
    out.push(Frame::Audio(b[8..8 + payload_size].to_vec()));
    Some(total)
}

/// Accumulates demuxed video into an elementary stream for the decoder.
///
/// Successive video payloads are concatenated. Video is dropped until the first
/// keyframe, so a decoder does not start mid-GOP on a P-frame it cannot resolve. A
/// frame split across two transport reads is preserved by keeping the undecoded tail.
#[derive(Default)]
pub struct VideoStream {
    pending: Vec<u8>,
    elementary: Vec<u8>,
    seen_keyframe: bool,
    codec: Option<VideoCodec>,
}

impl VideoStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// The codec, known once the first video frame has been seen.
    pub fn codec(&self) -> Option<VideoCodec> {
        self.codec
    }

    /// Feed raw BCMedia bytes; append any newly-complete video payloads to the
    /// elementary stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let (frames, used) = demux(&self.pending);
        for frame in frames {
            if let Frame::Video {
                codec,
                keyframe,
                data,
                ..
            } = frame
            {
                self.codec = Some(codec);
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

    /// Build a video frame in the real layout.
    fn video_frame(magic: u32, codec: &[u8; 4], addl: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&magic.to_le_bytes());
        v.extend_from_slice(codec);
        v.extend_from_slice(&(data.len() as u32).to_le_bytes()); // payload_size
        v.extend_from_slice(&(addl.len() as u32).to_le_bytes()); // additional_header_size
        v.extend_from_slice(&0u32.to_le_bytes()); // microseconds
        v.extend_from_slice(&0u32.to_le_bytes()); // unknown
        v.extend_from_slice(addl); // additional header
        v.extend_from_slice(data);
        v.extend_from_slice(&vec![0u8; pad_for(data.len())]); // padding
        v
    }

    /// The captured iframe header parses into the right codec and field values.
    #[test]
    fn parses_the_captured_iframe_header() {
        // First 16 decrypted bytes of a real iframe: "00dc" "H265" payload_size
        // 0x0010d0cd, additional_header_size 0x80. We only assert the header read,
        // so give a tiny payload and matching addl header.
        let addl = vec![0u8; 0x80];
        let data = b"\x00\x00\x00\x01hevc-nal";
        let frame = video_frame(IFRAME, b"H265", &addl, data);
        // Sanity: the header bytes line up with the capture.
        assert_eq!(&frame[0..4], &[0x30, 0x30, 0x64, 0x63], "00dc magic");
        assert_eq!(&frame[4..8], b"H265");
        let (frames, used) = demux(&frame);
        assert_eq!(used, frame.len());
        assert_eq!(
            frames,
            vec![Frame::Video {
                codec: VideoCodec::H265,
                keyframe: true,
                micros: 0,
                data: data.to_vec(),
            }]
        );
    }

    #[test]
    fn demuxes_an_iframe_then_a_pframe_with_padding() {
        let mut stream = video_frame(IFRAME, b"H265", &[], b"IDR"); // 3 bytes -> 5 pad
        stream.extend_from_slice(&video_frame(PFRAME, b"H265", &[0u8; 8], b"PPPP"));
        let (frames, used) = demux(&stream);
        assert_eq!(used, stream.len(), "both frames and their padding consumed");
        assert_eq!(frames.len(), 2);
        assert!(matches!(frames[0], Frame::Video { keyframe: true, .. }));
        assert!(matches!(frames[1], Frame::Video { keyframe: false, .. }));
    }

    #[test]
    fn a_partial_trailing_frame_is_left_unconsumed() {
        let whole = video_frame(IFRAME, b"H264", &[], b"keyframe-data");
        let (frames, used) = demux(&whole[..whole.len() - 3]);
        assert!(frames.is_empty());
        assert_eq!(used, 0);
    }

    #[test]
    fn video_is_dropped_until_the_first_keyframe() {
        let mut s = VideoStream::new();
        s.push(&video_frame(PFRAME, b"H265", &[], b"orphan"));
        assert!(s.take().is_empty(), "no keyframe yet");
        s.push(&video_frame(IFRAME, b"H265", &[], b"KEY"));
        s.push(&video_frame(PFRAME, b"H265", &[], b"then-p"));
        assert_eq!(s.take(), b"KEYthen-p");
        assert_eq!(s.codec(), Some(VideoCodec::H265));
    }

    #[test]
    fn a_frame_split_across_two_pushes_is_reassembled() {
        let frame = video_frame(IFRAME, b"H265", &[], b"splitpayload");
        let (head, tail) = frame.split_at(frame.len() - 5);
        let mut s = VideoStream::new();
        s.push(head);
        assert!(s.take().is_empty());
        s.push(tail);
        assert_eq!(s.take(), b"splitpayload");
    }

    #[test]
    fn an_info_frame_yields_dimensions_and_is_consumed() {
        // The size field is the total frame length (32), mirroring the live capture.
        let mut b = Vec::new();
        b.extend_from_slice(&INFO_V1.to_le_bytes()); // magic (4)
        b.extend_from_slice(&32u32.to_le_bytes()); // size = total (4)
        b.extend_from_slice(&2560u32.to_le_bytes()); // width (4)
        b.extend_from_slice(&1920u32.to_le_bytes()); // height (4)
        b.extend_from_slice(&[0u8; 16]); // rest, to a 32-byte total
        assert_eq!(b.len(), 32);
        let (frames, used) = demux(&b);
        assert_eq!(used, 32);
        assert_eq!(frames, vec![Frame::Info { width: 2560, height: 1920 }]);
    }

    #[test]
    fn unrecognised_bytes_stop_the_demuxer_for_resync() {
        let (frames, used) = demux(b"NOPExxxxxxxx");
        assert!(frames.is_empty());
        assert_eq!(used, 0);
    }

    #[test]
    fn tells_a_frame_from_other_bytes() {
        assert!(starts_with_frame(b"00dcH264...."));
        assert!(starts_with_frame(b"01dcH265...."));
        assert!(!starts_with_frame(&[0x06, 0x4b, 0x5c, 0x52, 0xdd]));
        assert!(!starts_with_frame(b"00"));
    }
}
