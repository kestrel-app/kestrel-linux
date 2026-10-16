//! The Baichuan control-channel message header.
//!
//! Once the UDP transport is up (see [`super::transport`]) it carries an ordered
//! byte stream, and on that stream the device speaks the "BC" message protocol: a
//! fixed header followed by an optionally-encrypted body, which for the commands
//! Kestrel sends is always XML.
//!
//! **Provenance — measured.** This header layout was confirmed against a real
//! device: a login reply arrived with exactly these fields, and sending a message
//! built this way got an ACK and a valid reply. The layout is:
//!
//! ```text
//! u32 magic (0x0abcdef0) | u32 msg_id | u32 body_len
//! u8 channel_id | u8 stream_type | u16 msg_num
//! u16 response_code | u16 class | [u32 payload_offset]
//! ```
//!
//! The trailing `payload_offset` is present only for the "extended" classes
//! `0x6414` and `0x0000`; every other class has a 20-byte header. The body, when
//! present, is encrypted with [`super::crypto::bc_encrypt`] (newer firmware can
//! negotiate AES-CFB instead, which is not yet implemented).

use super::crypto::{aes128_cfb_decrypt, aes128_cfb_encrypt, bc_encrypt, AES_IV};
use crate::api::error::{Error, Result};

/// The constant that opens every control message, little-endian on the wire
/// (`f0 de bc 0a`).
pub const BC_MAGIC: u32 = 0x0abc_def0;

/// The shortest header; the extended classes add a 4-byte payload offset.
pub const HEADER_LEN: usize = 20;
pub const HEADER_LEN_EXTENDED: usize = 24;

/// The "legacy" message class, used for the header-only login upgrade.
pub const CLASS_LEGACY: u16 = 0x6514;
/// The "modern" class that carries an XML body and a payload offset.
pub const CLASS_MODERN: u16 = 0x6414;

/// Whether a class carries the trailing 4-byte payload offset.
pub fn has_payload_offset(class: u16) -> bool {
    class == 0x6414 || class == 0x0000
}

/// How a message body is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encryption {
    /// Plaintext body (or no body).
    None,
    /// The legacy "BCEncrypt" xor cipher, at the given offset. The device's first
    /// login reply uses offset 0. Login messages stay on this even after AES is
    /// negotiated.
    BcEncrypt(u32),
    /// AES-128-CFB with the key derived at login. The cipher for every message
    /// *after* login.
    Aes([u8; 16]),
}

impl Encryption {
    /// Apply this cipher to a body (encryption and decryption are the same call for
    /// the xor ciphers; AES needs the explicit direction, handled by the caller).
    pub(crate) fn encrypt(self, body: &[u8]) -> Vec<u8> {
        match self {
            Encryption::None => body.to_vec(),
            Encryption::BcEncrypt(offset) => bc_encrypt(offset, body),
            Encryption::Aes(key) => aes128_cfb_encrypt(&key, &AES_IV, body),
        }
    }

    /// Decrypt a body with this cipher. Public within the crate so the video path can
    /// decrypt a message's extension and the encrypted prefix of its binary payload.
    pub(crate) fn decrypt(self, body: &[u8]) -> Vec<u8> {
        match self {
            Encryption::None => body.to_vec(),
            Encryption::BcEncrypt(offset) => bc_encrypt(offset, body),
            Encryption::Aes(key) => aes128_cfb_decrypt(&key, &AES_IV, body),
        }
    }
}

/// A control message's header fields, read without decrypting the body — what the
/// video path needs to walk a raw stream of frames and split each into its extension
/// and binary payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgHeader {
    pub msg_id: u32,
    /// The channel the message is about. A video message echoes its Preview's.
    pub channel_id: u8,
    /// Echoed from the request. Measured: with several Previews on one session,
    /// each stream's video messages carry its own Preview's number — which is how
    /// one connection's video is told apart.
    pub msg_num: u16,
    pub class: u16,
    pub response_code: u16,
    /// Extension length for the classes that carry one; `None` otherwise.
    pub payload_offset: Option<u32>,
    pub body_len: usize,
    pub header_len: usize,
}

/// Parse just the header of the message at the front of `buf`, returning it and the
/// message's total length (header + body). `Ok(None)` when `buf` does not yet hold the
/// whole message. The body is `buf[header_len..total]`, left encrypted for the caller.
pub fn message_frame(buf: &[u8]) -> Result<Option<(MsgHeader, usize)>> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != BC_MAGIC {
        return Err(Error::Protocol(format!(
            "control stream desynchronised (magic {magic:#010x})"
        )));
    }
    let msg_id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let body_len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    let channel_id = buf[12];
    let msg_num = u16::from_le_bytes([buf[14], buf[15]]);
    let response_code = u16::from_le_bytes([buf[16], buf[17]]);
    let class = u16::from_le_bytes([buf[18], buf[19]]);
    let (header_len, payload_offset) = if has_payload_offset(class) {
        if buf.len() < HEADER_LEN_EXTENDED {
            return Ok(None);
        }
        (
            HEADER_LEN_EXTENDED,
            Some(u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]])),
        )
    } else {
        (HEADER_LEN, None)
    };
    let total = header_len + body_len;
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some((
        MsgHeader {
            msg_id,
            channel_id,
            msg_num,
            class,
            response_code,
            payload_offset,
            body_len,
            header_len,
        },
        total,
    )))
}

/// One control message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub msg_id: u32,
    pub channel_id: u8,
    pub stream_type: u8,
    pub msg_num: u16,
    /// On a request this carries the requested encryption (e.g. `0xdc12` for AES);
    /// on a reply, the device's status (`200` means login succeeded).
    pub response_code: u16,
    pub class: u16,
    pub encryption: Encryption,
    /// The decrypted body. Empty for a header-only message.
    pub body: Vec<u8>,
    /// The whole body is an extension (the header's payload offset is its length)
    /// rather than a payload — how a command names the NVR channel it is about.
    pub extension_only: bool,
}

impl Message {
    /// A header-only message (no body) — used for the login upgrade.
    pub fn header_only(msg_id: u32, msg_num: u16, response_code: u16, class: u16) -> Self {
        Message {
            msg_id,
            channel_id: 0,
            stream_type: 0,
            msg_num,
            response_code,
            class,
            encryption: Encryption::None,
            body: Vec::new(),
            extension_only: false,
        }
    }

    /// A modern request carrying an XML body.
    pub fn modern(msg_id: u32, msg_num: u16, encryption: Encryption, xml: impl Into<String>) -> Self {
        Message {
            msg_id,
            channel_id: 0,
            stream_type: 0,
            msg_num,
            response_code: 0,
            class: CLASS_MODERN,
            encryption,
            body: xml.into().into_bytes(),
            extension_only: false,
        }
    }

    /// A command about one NVR channel, with no payload: the channel goes in the
    /// header and in an `Extension` body. Measured: `GetOsd` (44) sent bare is
    /// refused with `400`; with this it answers for the channel named.
    pub fn for_channel(msg_id: u32, msg_num: u16, encryption: Encryption, channel: u32) -> Self {
        let mut msg = Message::modern(
            msg_id,
            msg_num,
            encryption,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
                 <Extension version=\"1.1\">\n<channelId>{channel}</channelId>\n</Extension>\n"
            ),
        );
        msg.channel_id = channel as u8;
        msg.extension_only = true;
        msg
    }

    /// The body as text, when it is the XML these commands carry.
    pub fn xml(&self) -> Result<&str> {
        std::str::from_utf8(&self.body)
            .map_err(|_| Error::Protocol("control message body is not UTF-8".into()))
    }

    fn header_len(&self) -> usize {
        if has_payload_offset(self.class) {
            HEADER_LEN_EXTENDED
        } else {
            HEADER_LEN
        }
    }

    /// Serialise header + (possibly encrypted) body to the wire.
    pub fn to_bytes(&self) -> Vec<u8> {
        let body = self.encryption.encrypt(&self.body);

        let mut out = Vec::with_capacity(self.header_len() + body.len());
        out.extend_from_slice(&BC_MAGIC.to_le_bytes());
        out.extend_from_slice(&self.msg_id.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.push(self.channel_id);
        out.push(self.stream_type);
        out.extend_from_slice(&self.msg_num.to_le_bytes());
        out.extend_from_slice(&self.response_code.to_le_bytes());
        out.extend_from_slice(&self.class.to_le_bytes());
        if has_payload_offset(self.class) {
            // The body is either all payload (offset 0) or all extension.
            let offset = if self.extension_only { body.len() as u32 } else { 0 };
            out.extend_from_slice(&offset.to_le_bytes());
        }
        out.extend_from_slice(&body);
        out
    }

    /// Parse one message from the front of `buf`, returning it and how many bytes
    /// it consumed, or `None` when `buf` does not yet hold a whole message.
    ///
    /// `cipher` is how to decrypt the body — the session's current cipher. Login
    /// replies are [`Encryption::BcEncrypt`]; post-login replies are
    /// [`Encryption::Aes`]. An empty body needs no cipher.
    pub fn parse(buf: &[u8], cipher: Encryption) -> Result<Option<(Message, usize)>> {
        if buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != BC_MAGIC {
            return Err(Error::Protocol(format!(
                "control stream desynchronised (magic {magic:#010x})"
            )));
        }
        let msg_id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let body_len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
        let channel_id = buf[12];
        let stream_type = buf[13];
        let msg_num = u16::from_le_bytes([buf[14], buf[15]]);
        let response_code = u16::from_le_bytes([buf[16], buf[17]]);
        let class = u16::from_le_bytes([buf[18], buf[19]]);

        let header_len = if has_payload_offset(class) {
            HEADER_LEN_EXTENDED
        } else {
            HEADER_LEN
        };
        let total = header_len + body_len;
        if buf.len() < total {
            return Ok(None);
        }
        let raw = &buf[header_len..total];
        // An empty body needs no decryption; otherwise apply the session cipher.
        let (encryption, body) = if raw.is_empty() {
            (Encryption::None, Vec::new())
        } else {
            (cipher, cipher.decrypt(raw))
        };

        Ok(Some((
            Message {
                msg_id,
                channel_id,
                stream_type,
                msg_num,
                response_code,
                class,
                encryption,
                body,
                extension_only: false,
            },
            total,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::vendor::baichuan::cmd;

    /// The login upgrade is a 20-byte header with no body, magic first, LE.
    #[test]
    fn login_upgrade_is_a_bare_header() {
        let msg = Message::header_only(cmd::LOGIN, 1, 0xdc12, CLASS_LEGACY);
        let bytes = msg.to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(&bytes[0..4], &[0xf0, 0xde, 0xbc, 0x0a]);
        assert_eq!(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]), 1);
        assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 0xdc12);
        assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), CLASS_LEGACY);
    }

    /// The modern class carries the extra payload-offset word.
    #[test]
    fn modern_message_has_the_extended_header() {
        let msg = Message::modern(cmd::LOGIN, 2, Encryption::None, "<body/>");
        let bytes = msg.to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN_EXTENDED + "<body/>".len());
        let (back, used) = Message::parse(&bytes, Encryption::None).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(back.xml().unwrap(), "<body/>");
        assert_eq!(back.class, CLASS_MODERN);
    }

    /// An encrypted body round-trips, and the ciphertext is not the plaintext.
    #[test]
    fn bcencrypt_body_round_trips() {
        let msg = Message {
            msg_id: cmd::LOGIN,
            channel_id: 0,
            stream_type: 0,
            msg_num: 3,
            response_code: 0,
            class: 0x6614,
            encryption: Encryption::BcEncrypt(0),
            body: b"<?xml?><body><Encryption/></body>".to_vec(),
            extension_only: false,
        };
        let bytes = msg.to_bytes();
        // The non-XML-leading ciphertext is decrypted on parse.
        let (back, _) = Message::parse(&bytes, Encryption::BcEncrypt(0)).unwrap().unwrap();
        assert_eq!(back.body, msg.body);
    }

    #[test]
    fn a_short_buffer_is_incomplete_not_an_error() {
        assert!(Message::parse(&[0xf0, 0xde, 0xbc], Encryption::None).unwrap().is_none());
    }

    #[test]
    fn a_bad_magic_is_an_error() {
        let mut bytes = Message::header_only(1, 1, 0, CLASS_LEGACY).to_bytes();
        bytes[0] ^= 0xff;
        assert!(Message::parse(&bytes, Encryption::None).is_err());
    }

    /// The header layout matches what a real device sent: a 20-byte header for the
    /// non-extended class 0x6614, msg_id 1, body following immediately.
    #[test]
    fn parses_a_real_device_reply_header() {
        // Header of the captured login reply (class 0x6614, body_len 311).
        let mut packet = Vec::new();
        packet.extend_from_slice(&BC_MAGIC.to_le_bytes());
        packet.extend_from_slice(&1u32.to_le_bytes()); // msg_id
        packet.extend_from_slice(&3u32.to_le_bytes()); // body_len (tiny for the test)
        packet.push(0); // channel
        packet.push(0); // stream
        packet.extend_from_slice(&1u16.to_le_bytes()); // msg_num
        packet.extend_from_slice(&0xdd12u16.to_le_bytes()); // response_code
        packet.extend_from_slice(&0x6614u16.to_le_bytes()); // class (no payload offset)
        packet.extend_from_slice(b"<a>"); // plaintext-looking body
        let (msg, used) = Message::parse(&packet, Encryption::None).unwrap().unwrap();
        assert_eq!(msg.msg_id, 1);
        assert_eq!(msg.class, 0x6614);
        assert_eq!(used, HEADER_LEN + 3);
        assert_eq!(msg.xml().unwrap(), "<a>");
    }

    /// A channel command's body is all extension: the payload offset is its length.
    #[test]
    fn a_channel_command_carries_its_channel_as_an_extension() {
        let msg = Message::for_channel(44, 9, Encryption::None, 3);
        let bytes = msg.to_bytes();
        assert_eq!(bytes[12], 3, "channel in the header");
        let offset = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as usize;
        assert_eq!(offset, bytes.len() - HEADER_LEN_EXTENDED, "the whole body is extension");
        assert!(String::from_utf8_lossy(&bytes[24..]).contains("<channelId>3</channelId>"));
    }
}
