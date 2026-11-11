//! Turning the device's video *messages* into a BCMedia byte stream.
//!
//! Live video comes back as a run of control messages (`msg_id` [`cmd::VIDEO`], class
//! `0x0000`), each one `[extension][binary payload]`. The extension is an encrypted
//! XML snippet that says how the payload is protected:
//!
//! - `<binaryData>1</binaryData>` — the payload is plaintext.
//! - `<encryptLen>N</encryptLen>` — the first `N` bytes of the payload are AES-
//!   encrypted (the "FullAes" scheme: only a prefix), the rest plaintext.
//!
//! [`bcmedia_bytes`] undoes that, yielding the raw BCMedia bytes [`super::media`]
//! then demuxes into an elementary stream. This was all confirmed against a live NVR;
//! see `docs/untested.md`.

use std::time::{Duration, Instant};

use super::media::VideoStream;
use super::session::Session;
use super::wire::{message_frame, Encryption, Message};
use super::{cmd, xml};
use crate::api::error::Result;
use crate::api::models::StreamType;

/// Find `<encryptLen>N</encryptLen>` in a decrypted extension, if present.
fn encrypt_len(extension: &str) -> Option<usize> {
    xml::tag_text(extension, "encryptLen").and_then(|v| v.parse().ok())
}

/// Recover the BCMedia bytes of one video message.
///
/// `body` is the raw (still-encrypted) message body, `payload_offset` the extension
/// length, and `cipher` the session's current cipher. The extension is decrypted to
/// learn whether — and how much of — the payload is encrypted.
pub fn bcmedia_bytes(body: &[u8], payload_offset: usize, cipher: Encryption) -> Vec<u8> {
    let split = payload_offset.min(body.len());
    let (ext_raw, payload) = body.split_at(split);

    let extension = cipher.decrypt(ext_raw);
    let extension = String::from_utf8_lossy(&extension);

    match encrypt_len(&extension) {
        Some(n) => {
            // Only the first N payload bytes are encrypted; the rest are plaintext.
            let n = n.min(payload.len());
            let mut out = cipher.decrypt(&payload[..n]);
            out.extend_from_slice(&payload[n..]);
            out
        }
        // `<binaryData>` (or no extension): the payload is already plaintext.
        None => payload.to_vec(),
    }
}

/// A live video stream from one channel, decoded to an elementary bitstream.
///
/// **Verification status.** The message decode ([`bcmedia_bytes`]) and the demuxer
/// are tested; the drive loop here talks to the live transport, which runs only
/// against a real device. The elementary stream it produces still needs a decoder
/// (the custom ffmpeg input) to become pictures — see `docs/untested.md`.
pub struct VideoStreamer {
    channel: u32,
    stream: VideoStream,
}

impl VideoStreamer {
    /// Ask the device to start sending a channel's video.
    pub fn start(session: &mut Session, channel: u32, stream: StreamType) -> Result<VideoStreamer> {
        let handle = match stream {
            StreamType::Main => 0,
            StreamType::Sub => 1,
        };
        let name = match stream {
            StreamType::Main => "mainStream",
            StreamType::Sub => "subStream",
        };
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
             <body><Preview version=\"1.1\"><channelId>{channel}</channelId>\
             <handle>{handle}</handle><streamType>{name}</streamType></Preview></body>\n"
        );
        let mut preview = Message::modern(cmd::VIDEO, 2, session.encryption(), body);
        // The stream type also travels in the header (0 main, 1 sub).
        preview.stream_type = u8::from(matches!(stream, StreamType::Sub));
        session.send_oneway(preview)?;
        Ok(VideoStreamer {
            channel,
            stream: VideoStream::new(),
        })
    }

    pub fn channel(&self) -> u32 {
        self.channel
    }

    /// The codec, known once the first video frame has arrived.
    pub fn codec(&self) -> Option<super::media::VideoCodec> {
        self.stream.codec()
    }

    /// Service the socket for up to `budget`, decode any video messages that arrived,
    /// and return the elementary-stream bytes produced.
    pub fn poll(&mut self, session: &mut Session, budget: Duration) -> Result<Vec<u8>> {
        let deadline = Instant::now() + budget;
        loop {
            session.decode_video_into(&mut self.stream)?;
            let out = self.stream.take();
            if !out.is_empty() || Instant::now() >= deadline {
                return Ok(out);
            }
        }
    }
}

/// Walk a buffer of raw control bytes and feed every video message's BCMedia into
/// `stream`, returning the number of bytes consumed. Non-video messages are skipped.
/// Shared by [`Session`] so the frame-walking logic is testable without a socket.
pub(crate) fn drain_video(
    buffer: &[u8],
    cipher: Encryption,
    stream: &mut VideoStream,
) -> Result<usize> {
    let mut pos = 0;
    while let Some((header, total)) = message_frame(&buffer[pos..])? {
        if header.msg_id == cmd::VIDEO {
            let body = &buffer[pos + header.header_len..pos + total];
            let poff = header.payload_offset.unwrap_or(0) as usize;
            let bcmedia = bcmedia_bytes(body, poff, cipher);
            stream.push(&bcmedia);
        }
        pos += total;
    }
    Ok(pos)
}

/// Walk a buffer of raw control bytes and hand every whole message to `route` —
/// its header, its still-encrypted body, and the cipher to read it with — returning
/// the bytes consumed. For a connection that carries several things at once: live
/// streams, fetches and replies to commands are sorted by its owner.
pub(crate) fn drain_routed(
    buffer: &[u8],
    cipher: Encryption,
    mut route: impl FnMut(&super::wire::MsgHeader, &[u8], Encryption),
) -> Result<usize> {
    let mut pos = 0;
    while let Some((header, total)) = message_frame(&buffer[pos..])? {
        route(&header, &buffer[pos + header.header_len..pos + total], cipher);
        pos += total;
    }
    Ok(pos)
}

/// The BCMedia a video-carrying message holds (live video, a replay or a
/// download), or nothing for a bodiless status.
pub(crate) fn media_of(header: &super::wire::MsgHeader, body: &[u8], cipher: Encryption) -> Vec<u8> {
    if body.is_empty() {
        return Vec::new();
    }
    bcmedia_bytes(body, header.payload_offset.unwrap_or(0) as usize, cipher)
}

/// The Preview body for one stream. `handle` must be distinct per stream on a
/// session: measured, a second Preview reusing a handle replaced the first.
pub(crate) fn preview_body(channel: u32, handle: u32, stream: StreamType) -> String {
    let name = match stream {
        StreamType::Main => "mainStream",
        StreamType::Sub => "subStream",
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><Preview version=\"1.1\"><channelId>{channel}</channelId>\
         <handle>{handle}</handle><streamType>{name}</streamType></Preview></body>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::vendor::baichuan::crypto::{aes128_cfb_encrypt, make_aes_key, AES_IV};
    use crate::api::vendor::baichuan::media::VideoCodec;

    fn aes_enc(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
        aes128_cfb_encrypt(key, &AES_IV, data)
    }

    /// `<binaryData>` extension: the payload is already plaintext and passes through.
    #[test]
    fn plaintext_payload_passes_through() {
        let ext = b"<Extension><binaryData>1</binaryData></Extension>";
        let payload = b"\x31\x30\x30\x31rawinfo"; // INFO_V1 magic + bytes
        let mut body = ext.to_vec();
        body.extend_from_slice(payload);
        assert_eq!(bcmedia_bytes(&body, ext.len(), Encryption::None), payload);
    }

    /// `<encryptLen>N</encryptLen>`: only the first N payload bytes are AES-encrypted;
    /// `bcmedia_bytes` decrypts them and keeps the plaintext tail.
    #[test]
    fn encrypted_prefix_is_decrypted_and_the_tail_kept() {
        let key = make_aes_key("nonce", "pw");
        let cipher = Encryption::Aes(key);
        let prefix = b"00dcH265 header bytes here"; // the encrypted part
        let tail = b"....rest of the frame, plaintext....";

        let ext_plain = format!("<Extension><encryptLen>{}</encryptLen></Extension>", prefix.len());
        let ext_wire = aes_enc(&key, ext_plain.as_bytes());

        let mut body = ext_wire.clone();
        body.extend_from_slice(&aes_enc(&key, prefix)); // encrypted prefix
        body.extend_from_slice(tail); // plaintext tail

        let out = bcmedia_bytes(&body, ext_wire.len(), cipher);
        let mut expected = prefix.to_vec();
        expected.extend_from_slice(tail);
        assert_eq!(out, expected);
    }

    /// A plaintext video message is walked and its BCMedia fed to the stream; the
    /// keyframe establishes the codec and emits the elementary bytes.
    #[test]
    fn drain_video_feeds_the_stream() {
        let mut vs = VideoStream::new();
        let ext = b"<Extension><binaryData>1</binaryData></Extension>";
        let iframe = iframe_bytes(b"H265", b"KEYFRAME");
        let wire = video_message(ext, &iframe);

        let used = drain_video(&wire, Encryption::None, &mut vs).unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(vs.codec(), Some(VideoCodec::H265));
        assert_eq!(vs.take(), b"KEYFRAME");
    }

    /// Against a real device, through the real Rust path: connect, log in, start the
    /// sub stream, collect a few seconds of elementary stream, and decode it through
    /// the custom ffmpeg input. Ignored by default; run with
    ///   KESTREL_TEST_UID=... KESTREL_TEST_PASS=... \
    ///     cargo test -- --ignored streams_real_video_by_uid --nocapture
    #[test]
    #[ignore]
    fn streams_real_video_by_uid() {
        use crate::api::vendor::baichuan::{login, session::Session, transport::Transport};
        use crate::video::bc_avio::{AvioInput, SliceSource};

        let (Ok(uid), Ok(password)) = (
            std::env::var("KESTREL_TEST_UID"),
            std::env::var("KESTREL_TEST_PASS"),
        ) else {
            eprintln!("KESTREL_TEST_UID / KESTREL_TEST_PASS not set");
            return;
        };
        let user = std::env::var("KESTREL_TEST_USER").unwrap_or_else(|_| "admin".into());
        let timeout = Duration::from_secs(10);

        let transport = Transport::connect(&uid, timeout).expect("connect");
        let mut session = Session::new(Box::new(transport));
        login::login(&mut session, &user, &password, timeout).expect("login");
        let mut streamer = VideoStreamer::start(&mut session, 0, StreamType::Sub).expect("preview");

        let mut elementary = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            elementary.extend(streamer.poll(&mut session, Duration::from_millis(500)).unwrap());
        }
        println!("elementary stream: {} bytes, codec {:?}", elementary.len(), streamer.stream.codec());
        assert!(!elementary.is_empty(), "no video arrived");

        let mut input = AvioInput::open(Box::new(SliceSource::new(elementary)), None)
            .expect("open custom avio on live bytes");
        let stream = input.streams().best(ffmpeg_next::media::Type::Video).expect("video");
        let index = stream.index();
        let mut decoder = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())
            .unwrap()
            .decoder()
            .video()
            .unwrap();
        let packets: Vec<_> = input
            .packets()
            .filter(|(s, _)| s.index() == index)
            .map(|(_, p)| p)
            .collect();
        let mut frame = ffmpeg_next::frame::Video::empty();
        let mut decoded = 0;
        for packet in &packets {
            if decoder.send_packet(packet).is_ok() {
                while decoder.receive_frame(&mut frame).is_ok() {
                    decoded += 1;
                }
            }
        }
        println!("decoded {decoded} frame(s) at {}x{}", decoder.width(), decoder.height());
        assert!(decoded > 0, "nothing decoded from live video");
    }

    fn iframe_bytes(codec: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0x6364_3030u32.to_le_bytes()); // IFRAME magic
        v.extend_from_slice(codec);
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // additional_header_size
        v.extend_from_slice(&0u32.to_le_bytes()); // microseconds
        v.extend_from_slice(&0u32.to_le_bytes()); // unknown
        v.extend_from_slice(data);
        v.extend_from_slice(&vec![0u8; (8 - data.len() % 8) % 8]); // pad
        v
    }

    /// A raw class-0x0000 video message with the given (plaintext) extension + payload.
    fn video_message(ext: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut body = ext.to_vec();
        body.extend_from_slice(payload);
        let mut out = Vec::new();
        out.extend_from_slice(&0x0abc_def0u32.to_le_bytes()); // magic
        out.extend_from_slice(&cmd::VIDEO.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes()); // body_len
        out.push(0); // channel
        out.push(0); // stream
        out.extend_from_slice(&1u16.to_le_bytes()); // msg_num
        out.extend_from_slice(&200u16.to_le_bytes()); // response_code
        out.extend_from_slice(&0x0000u16.to_le_bytes()); // class 0x0000
        out.extend_from_slice(&(ext.len() as u32).to_le_bytes()); // payload_offset
        out.extend_from_slice(&body);
        out
    }

    /// Two streams interleaved on one connection reach their own demuxers by the
    /// message number each Preview was sent with.
    #[test]
    fn routed_video_goes_by_message_number() {
        let ext = b"<Extension><binaryData>1</binaryData></Extension>";
        let mut wire = video_message(ext, &iframe_bytes(b"H264", b"CHANNEL-A"));
        let mut b = video_message(ext, &iframe_bytes(b"H265", b"CHANNEL-B"));
        b[14..16].copy_from_slice(&7u16.to_le_bytes()); // msg_num 7
        wire.extend_from_slice(&b);

        let mut streams = std::collections::HashMap::new();
        let used = drain_routed(&wire, Encryption::None, |header, body, cipher| {
            streams
                .entry(header.msg_num)
                .or_insert_with(VideoStream::new)
                .push(&media_of(header, body, cipher));
        })
        .unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(streams.get_mut(&1).unwrap().take(), b"CHANNEL-A");
        assert_eq!(streams.get_mut(&7).unwrap().take(), b"CHANNEL-B");
    }
}
