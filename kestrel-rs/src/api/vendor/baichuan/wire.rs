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

use super::crypto::bc_encrypt;
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
    /// login reply uses offset 0.
    BcEncrypt(u32),
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
        }
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
        let body = match self.encryption {
            Encryption::None => self.body.clone(),
            Encryption::BcEncrypt(offset) => bc_encrypt(offset, &self.body),
        };

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
            // The whole body is one payload; no separate extension.
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        out.extend_from_slice(&body);
        out
    }

    /// Parse one message from the front of `buf`, returning it and how many bytes
    /// it consumed, or `None` when `buf` does not yet hold a whole message.
    ///
    /// `decrypt_offset` is the BCEncrypt offset to try on an encrypted body; the
    /// device's login reply uses 0. A plaintext-looking body (starting with `<`) is
    /// taken as-is.
    pub fn parse(buf: &[u8], decrypt_offset: u32) -> Result<Option<(Message, usize)>> {
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

        // A body that already looks like XML is plaintext; otherwise decrypt it.
        let (encryption, body) = if raw.first() == Some(&b'<') || raw.is_empty() {
            (Encryption::None, raw.to_vec())
        } else {
            (Encryption::BcEncrypt(decrypt_offset), bc_encrypt(decrypt_offset, raw))
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
        let (back, used) = Message::parse(&bytes, 0).unwrap().unwrap();
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
        };
        let bytes = msg.to_bytes();
        // The non-XML-leading ciphertext is decrypted on parse.
        let (back, _) = Message::parse(&bytes, 0).unwrap().unwrap();
        assert_eq!(back.body, msg.body);
    }

    #[test]
    fn a_short_buffer_is_incomplete_not_an_error() {
        assert!(Message::parse(&[0xf0, 0xde, 0xbc], 0).unwrap().is_none());
    }

    #[test]
    fn a_bad_magic_is_an_error() {
        let mut bytes = Message::header_only(1, 1, 0, CLASS_LEGACY).to_bytes();
        bytes[0] ^= 0xff;
        assert!(Message::parse(&bytes, 0).is_err());
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
        let (msg, used) = Message::parse(&packet, 0).unwrap().unwrap();
        assert_eq!(msg.msg_id, 1);
        assert_eq!(msg.class, 0x6614);
        assert_eq!(used, HEADER_LEN + 3);
        assert_eq!(msg.xml().unwrap(), "<a>");
    }
}
