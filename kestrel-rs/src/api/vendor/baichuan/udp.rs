//! The UDP discovery layer: the packets that turn a UID into an address.
//!
//! Reolink's cloud does not resolve a UID to an IP over DNS. Instead the client
//! sends a small encrypted-XML datagram to a set of well-known registrars on
//! port 9999 and gets back the addresses of the register, relay and log servers
//! plus (when the device is reachable directly) the device itself. Everything
//! after that — the hole-punch, the reliable byte stream — rides the same three
//! packet shapes defined here.
//!
//! The three magics and the header layout were read from the official app's
//! `libBCSDKWrapper.so`; the payload cipher and CRC are in [`super::crypto`].
//!
//! This module is pure framing: it builds and parses bytes and does no I/O, so
//! all of it is exercised by the round-trip tests below without a network. The
//! transport that actually sends these lives in [`super::transport`].

use super::crypto::{bc_crc, xml_crypt};
use crate::api::error::{Error, Result};

/// The registrars a UID lookup is broadcast to, tried in parallel. The device's
/// own registrar is whichever one its UID was minted against, so all are asked.
pub const REGISTRARS: [&str; 12] = [
    "p2p.reolink.com",
    "p2p1.reolink.com",
    "p2p2.reolink.com",
    "p2p3.reolink.com",
    "p2p4.reolink.com",
    "p2p5.reolink.com",
    "p2p6.reolink.com",
    "p2p7.reolink.com",
    "p2p8.reolink.com",
    "p2p9.reolink.com",
    "p2p10.reolink.com",
    "p2p11.reolink.com",
];

/// The port every registrar answers UID lookups on.
pub const REGISTRAR_PORT: u16 = 9999;

/// Magic for a discovery/negotiation packet — the kind a UID lookup is.
pub const MAGIC_NEGO: u32 = 0x2a87_cf3a;
/// Magic for an acknowledgement packet in the reliable stream.
pub const MAGIC_ACK: u32 = 0x2a87_cf20;
/// Magic for a data packet in the reliable stream.
pub const MAGIC_DATA: u32 = 0x2a87_cf10;

/// A parsed discovery packet: its transmission id and its decrypted XML body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// Transmission id. Doubles as the payload cipher offset, so it has to
    /// survive parsing to decrypt the body and to key a reply.
    pub tid: u32,
    /// The decrypted XML, exactly as the device sent or expects it.
    pub xml: String,
}

impl Discovery {
    pub fn new(tid: u32, xml: impl Into<String>) -> Self {
        Discovery {
            tid,
            xml: xml.into(),
        }
    }

    /// Serialise to the wire: magic, payload length, a constant `1`, the tid, the
    /// CRC over the encrypted payload, then the encrypted payload.
    pub fn to_bytes(&self) -> Vec<u8> {
        let encrypted = xml_crypt(self.tid, self.xml.as_bytes());
        let crc = bc_crc(&encrypted);
        let mut out = Vec::with_capacity(20 + encrypted.len());
        out.extend_from_slice(&MAGIC_NEGO.to_le_bytes());
        out.extend_from_slice(&(encrypted.len() as u32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&self.tid.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&encrypted);
        out
    }

    /// Parse a datagram that is expected to be a discovery packet.
    ///
    /// Rejects anything whose magic is not [`MAGIC_NEGO`], whose length field
    /// overruns the buffer, or whose CRC does not match — a stray datagram or a
    /// decode against the wrong tid should fail here, not surface as mojibake XML.
    pub fn from_bytes(buf: &[u8]) -> Result<Discovery> {
        if buf.len() < 20 {
            return Err(Error::Protocol("discovery packet shorter than header".into()));
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC_NEGO {
            return Err(Error::Protocol(format!(
                "not a discovery packet (magic {magic:#010x})"
            )));
        }
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let tid = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let crc = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let payload = buf
            .get(20..20 + len)
            .ok_or_else(|| Error::Protocol("discovery length overruns packet".into()))?;
        if bc_crc(payload) != crc {
            return Err(Error::Protocol("discovery CRC mismatch".into()));
        }
        let decrypted = xml_crypt(tid, payload);
        let xml = String::from_utf8(decrypted)
            .map_err(|_| Error::Protocol("discovery payload is not UTF-8 XML".into()))?;
        Ok(Discovery { tid, xml })
    }
}

/// Build the UID-lookup request (`C2M_Q`) a registrar answers with the device's
/// server set.
///
/// `tid` can be anything; it only has to match what a reply would echo. The `<p>`
/// field is the client OS the app reports — the registrar does not appear to
/// branch on it, and `"WIN"` is what a desktop client sends.
pub fn uid_lookup_request(uid: &str, tid: u32) -> Discovery {
    let xml = format!("<P2P><C2M_Q><uid>{uid}</uid><p>WIN</p></C2M_Q></P2P>\n");
    Discovery::new(tid, xml)
}

/// One service address from a lookup reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub ip: String,
    pub port: u16,
}

/// The useful contents of a registrar's `M2C_Q_R` reply: where to find the
/// register and relay servers, and the device itself when it was given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LookupReply {
    pub register: Option<Endpoint>,
    pub relay: Option<Endpoint>,
    pub log: Option<Endpoint>,
    /// The device's own address, present when it is directly reachable.
    pub device: Option<Endpoint>,
}

impl LookupReply {
    /// Pull the service addresses out of a decrypted `M2C_Q_R` body.
    ///
    /// The reply is small, fixed XML, so it is read by locating each block rather
    /// than through a full parser — the same approach the QNAP vendor takes, and
    /// for the same reason: two fields per block, and a parser would be more
    /// machinery than the shape earns. An absent block is `None`, not an error;
    /// a registrar that does not know the UID answers with the blocks empty.
    pub fn parse(xml: &str) -> LookupReply {
        LookupReply {
            register: endpoint_in(xml, "reg"),
            relay: endpoint_in(xml, "relay"),
            log: endpoint_in(xml, "log"),
            device: endpoint_in(xml, "t"),
        }
    }

    /// Nothing usable came back — the registrar did not recognise the UID.
    pub fn is_empty(&self) -> bool {
        self.register.is_none()
            && self.relay.is_none()
            && self.log.is_none()
            && self.device.is_none()
    }
}

/// Read `<tag><ip>..</ip><port>..</port></tag>` out of the reply, if present.
fn endpoint_in(xml: &str, tag: &str) -> Option<Endpoint> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let block = &xml[start..end];
    let ip = text_between(block, "ip")?;
    let port = text_between(block, "port")?.parse().ok()?;
    Some(Endpoint { ip, port })
}

fn text_between(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(block[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lookup request survives a round trip through the wire format, and comes
    /// back as the XML we put in with the tid intact.
    #[test]
    fn lookup_request_round_trips_through_the_wire() {
        let req = uid_lookup_request("95270000ABCDEFGH", 0x1234);
        let bytes = req.to_bytes();
        let parsed = Discovery::from_bytes(&bytes).expect("round trip");
        assert_eq!(parsed.tid, 0x1234);
        assert!(parsed.xml.contains("<uid>95270000ABCDEFGH</uid>"));
        assert_eq!(parsed, req);
    }

    /// The header really is 20 bytes and the magic is first, little-endian.
    #[test]
    fn wire_header_layout_is_fixed() {
        let bytes = uid_lookup_request("X", 7).to_bytes();
        assert_eq!(&bytes[0..4], &MAGIC_NEGO.to_le_bytes());
        assert_eq!(&bytes[12..16], &7u32.to_le_bytes());
        let len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        assert_eq!(bytes.len(), 20 + len);
    }

    /// A corrupted payload is caught by the CRC rather than decrypted into
    /// garbage.
    #[test]
    fn a_flipped_payload_byte_fails_the_crc() {
        let mut bytes = uid_lookup_request("abc", 1).to_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(Discovery::from_bytes(&bytes).is_err());
    }

    #[test]
    fn a_foreign_datagram_is_rejected_by_magic() {
        let junk = [0u8; 40];
        assert!(Discovery::from_bytes(&junk).is_err());
    }

    /// The reply parser pulls each service address out of a representative
    /// `M2C_Q_R`, and leaves absent blocks as `None`.
    #[test]
    fn parses_a_representative_reply() {
        let xml = "<P2P><M2C_Q_R>\
            <reg><ip>18.162.200.47</ip><port>58200</port></reg>\
            <relay><ip>18.162.200.47</ip><port>58100</port></relay>\
            <log><ip>18.162.200.47</ip><port>57850</port></log>\
            <t><ip>203.0.113.9</ip><port>9996</port></t>\
            <rsp>0</rsp></M2C_Q_R></P2P>";
        let reply = LookupReply::parse(xml);
        assert_eq!(
            reply.register,
            Some(Endpoint {
                ip: "18.162.200.47".into(),
                port: 58200
            })
        );
        assert_eq!(reply.relay.as_ref().unwrap().port, 58100);
        assert_eq!(reply.device.as_ref().unwrap().ip, "203.0.113.9");
        assert!(!reply.is_empty());
    }

    #[test]
    fn an_unknown_uid_reply_is_empty() {
        let xml = "<P2P><M2C_Q_R><rsp>-1</rsp></M2C_Q_R></P2P>";
        assert!(LookupReply::parse(xml).is_empty());
    }
}
