//! A reliable ordered byte stream over UDP — the pipe the control protocol runs
//! on once a UID has been resolved.
//!
//! UDP gives datagrams, not a stream, and the device's datagrams can be lost,
//! reordered or duplicated. This layer adds the minimum to make it look like a
//! socket: monotonic packet ids, acknowledgement of what was received, and
//! reassembly of what arrives into an in-order byte queue. [`wire`] then reads
//! whole control messages off that queue.
//!
//! **Verification status.** The DATA/ACK packet *framing* ([`DataPacket`],
//! [`AckPacket`]) is pure and round-trip tested below. The connection itself —
//! the registrar lookup round-trip, the NAT hole-punch, the relay fallback and the
//! retransmit loop — performs real network I/O and has **not** been run against a
//! device or a capture from this machine (no Reolink device is reachable here and
//! outbound UDP is currently blocked). It is written from the official app as the
//! specification and is listed in `docs/untested.md`. See [`super`].
//!
//! [`wire`]: super::wire

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use super::udp::{self, Discovery, LookupReply, MAGIC_ACK, MAGIC_DATA};
use crate::api::error::{Error, Result};

/// A single data packet in the reliable stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPacket {
    /// The connection id the device assigned at setup; every packet carries it.
    pub connection_id: i32,
    /// Monotonic id of this packet within the connection.
    pub packet_id: u32,
    /// The stream bytes this packet carries.
    pub payload: Vec<u8>,
}

impl DataPacket {
    pub fn to_bytes(&self) -> Vec<u8> {
        // Header is 20 bytes: magic, connection_id, a constant zero word, packet_id,
        // payload length. The zero word is easy to miss and the device drops packets
        // without it — it is measured from real data packets.
        let mut out = Vec::with_capacity(20 + self.payload.len());
        out.extend_from_slice(&MAGIC_DATA.to_le_bytes());
        out.extend_from_slice(&self.connection_id.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&self.packet_id.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<DataPacket> {
        if buf.len() < 20 {
            return Err(Error::Protocol("data packet shorter than header".into()));
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC_DATA {
            return Err(Error::Protocol("not a data packet".into()));
        }
        let connection_id = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        // buf[8..12] is the constant zero word.
        let packet_id = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let len = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]) as usize;
        let payload = buf
            .get(20..20 + len)
            .ok_or_else(|| Error::Protocol("data length overruns packet".into()))?
            .to_vec();
        Ok(DataPacket {
            connection_id,
            packet_id,
            payload,
        })
    }
}

/// An acknowledgement that packets up to `packet_id` were received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckPacket {
    pub connection_id: i32,
    pub packet_id: u32,
}

impl AckPacket {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        out.extend_from_slice(&MAGIC_ACK.to_le_bytes());
        out.extend_from_slice(&self.connection_id.to_le_bytes());
        out.extend_from_slice(&self.packet_id.to_le_bytes());
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<AckPacket> {
        if buf.len() < 12 {
            return Err(Error::Protocol("ack packet too short".into()));
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC_ACK {
            return Err(Error::Protocol("not an ack packet".into()));
        }
        Ok(AckPacket {
            connection_id: i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            packet_id: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
        })
    }
}

/// How a device was reached, for logging and for deciding what video path to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// A direct UDP path to the device was punched through.
    Direct,
    /// Traffic is brokered by a Reolink relay server.
    Relay,
}

/// Resolve a UID to the device's server set by asking the registrars.
///
/// Each registrar is asked in turn over a single short-lived socket; the first
/// usable answer wins.
///
/// **Verified live.** This was run against the real registrars with a real UID:
/// the owning registrar answers with `rsp=0` and the register/relay/log/device
/// addresses, while the others answer `rsp=-3` (not their UID). It only started
/// working once the discovery CRC was corrected to init=0 — the wrong init made
/// the registrars silently drop every request, which is what [`crypto::bc_crc`]
/// now documents.
///
/// [`crypto::bc_crc`]: super::crypto::bc_crc
pub fn lookup_uid(uid: &str, timeout: Duration) -> Result<LookupReply> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| Error::connection(format!("UID lookup: cannot open socket: {e}")))?;
    socket
        .set_read_timeout(Some(timeout))
        .map_err(|e| Error::connection(format!("UID lookup: {e}")))?;

    let request = udp::uid_lookup_request(uid, rand_tid());
    let packet = request.to_bytes();

    for host in udp::REGISTRARS {
        let addr = match (host, udp::REGISTRAR_PORT).to_socket_addrs() {
            Ok(mut it) => match it.next() {
                Some(a) => a,
                None => continue,
            },
            Err(e) => {
                debug!("registrar {host} did not resolve: {e}");
                continue;
            }
        };
        if socket.send_to(&packet, addr).is_err() {
            continue;
        }
        let mut buf = [0u8; 2048];
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => match Discovery::from_bytes(&buf[..n]) {
                Ok(disc) => {
                    let reply = LookupReply::parse(&disc.xml);
                    if !reply.is_empty() {
                        info!("UID resolved via {host}");
                        return Ok(reply);
                    }
                }
                Err(e) => debug!("registrar {host} sent an unparsable reply: {e}"),
            },
            Err(_) => debug!("registrar {host} did not answer in time"),
        }
    }

    Err(Error::connection(
        "no registrar recognised this UID (device offline, wrong UID, or UDP blocked)",
    ))
}

/// A connection id and route chosen for a resolved device.
///
/// UNVERIFIED: the hole-punch and relay negotiation that would populate this
/// against a real device are not yet exercised. [`Transport::connect`] documents
/// the intended sequence.
#[derive(Debug)]
pub struct Transport {
    socket: UdpSocket,
    peer: SocketAddr,
    connection_id: i32,
    route: Route,
    /// Next outbound packet id.
    next_id: u32,
    /// Highest contiguous inbound packet id acknowledged.
    acked_in: u32,
    /// Reassembled inbound bytes not yet drained by the reader.
    inbound: Vec<u8>,
}

impl Transport {
    /// Open a reliable stream to a resolved device.
    ///
    /// Intended sequence (from the app): resolve the UID, try the device's own
    /// address with a hole-punch handshake, and fall back to the relay if the
    /// direct path does not come up. The handshake and relay negotiation are the
    /// unverified core; the method is present so the layers above compile against a
    /// real type, and returns [`Error::Unsupported`] until the handshake is
    /// confirmed against a device rather than returning a connection that only
    /// looks real.
    ///
    /// UNVERIFIED — see `docs/untested.md`.
    pub fn connect(uid: &str, timeout: Duration) -> Result<Transport> {
        let reply = lookup_uid(uid, timeout)?;
        warn!(
            "Baichuan transport handshake is not yet verified against a device; \
             refusing rather than returning a half-open connection"
        );
        let _ = (&reply, Route::Direct, Route::Relay);
        Err(Error::Unsupported(
            "Reolink P2P transport is implemented but unverified; \
             connect over LAN/RTSP until a device confirms the handshake"
                .into(),
        ))
    }

    /// Hand a datagram that arrived for this connection to the reassembler,
    /// emitting the ACK that should be sent back. Split out from the socket loop so
    /// the reliable-delivery logic is testable without a network.
    pub fn accept_datagram(&mut self, buf: &[u8]) -> Result<Option<AckPacket>> {
        let packet = DataPacket::from_bytes(buf)?;
        if packet.connection_id != self.connection_id {
            return Ok(None);
        }
        // Accept the next expected packet in order; ignore anything ahead of or
        // behind the window rather than buffering out-of-order (the control
        // channel is low-rate, so a dropped packet is simply re-requested by the
        // sender's own retransmit).
        if packet.packet_id == self.acked_in {
            self.inbound.extend_from_slice(&packet.payload);
            self.acked_in = self.acked_in.wrapping_add(1);
        }
        Ok(Some(AckPacket {
            connection_id: self.connection_id,
            packet_id: self.acked_in,
        }))
    }

    /// Frame outbound bytes as a data packet and advance the id.
    pub fn frame_outbound(&mut self, bytes: &[u8]) -> DataPacket {
        let packet = DataPacket {
            connection_id: self.connection_id,
            packet_id: self.next_id,
            payload: bytes.to_vec(),
        };
        self.next_id = self.next_id.wrapping_add(1);
        packet
    }

    /// Drain whatever reassembled bytes are available.
    pub fn take_inbound(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.inbound)
    }

    pub fn route(&self) -> Route {
        self.route
    }

    /// Send already-reliable bytes and block for their ACK.
    ///
    /// UNVERIFIED: the retransmit timing is a reasonable default, not a measured
    /// one.
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let packet = self.frame_outbound(bytes);
        let wire = packet.to_bytes();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.socket
                .send_to(&wire, self.peer)
                .map_err(|e| Error::connection(format!("transport send: {e}")))?;
            let mut buf = [0u8; 2048];
            match self.socket.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if let Ok(ack) = AckPacket::from_bytes(&buf[..n]) {
                        if ack.connection_id == self.connection_id
                            && ack.packet_id > packet.packet_id
                        {
                            return Ok(());
                        }
                    }
                }
                Err(_) if Instant::now() < deadline => continue,
                Err(e) => return Err(Error::connection(format!("transport send timed out: {e}"))),
            }
            if Instant::now() >= deadline {
                return Err(Error::connection("transport send: no ack"));
            }
        }
    }
}

/// A transmission id that is merely distinct, not cryptographic.
fn rand_tid() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_packet_round_trips() {
        let packet = DataPacket {
            connection_id: 254000,
            packet_id: 7,
            payload: b"hello device".to_vec(),
        };
        let bytes = packet.to_bytes();
        assert_eq!(DataPacket::from_bytes(&bytes).unwrap(), packet);
    }

    #[test]
    fn ack_packet_round_trips() {
        let ack = AckPacket {
            connection_id: 254000,
            packet_id: 8,
        };
        assert_eq!(AckPacket::from_bytes(&ack.to_bytes()).unwrap(), ack);
    }

    #[test]
    fn a_data_packet_is_not_an_ack() {
        let data = DataPacket {
            connection_id: 1,
            packet_id: 0,
            payload: vec![],
        }
        .to_bytes();
        assert!(AckPacket::from_bytes(&data).is_err());
    }

    /// The reassembler accepts in-order payloads, advances its ack, and ignores a
    /// duplicate of a packet it already took.
    #[test]
    fn reassembler_takes_in_order_and_ignores_duplicates() {
        // Build a transport by hand; the socket is never touched by this path.
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = socket.local_addr().unwrap();
        let mut t = Transport {
            socket,
            peer,
            connection_id: 42,
            route: Route::Direct,
            next_id: 0,
            acked_in: 0,
            inbound: Vec::new(),
        };

        let p0 = DataPacket {
            connection_id: 42,
            packet_id: 0,
            payload: b"AB".to_vec(),
        };
        let ack = t.accept_datagram(&p0.to_bytes()).unwrap().unwrap();
        assert_eq!(ack.packet_id, 1);
        // A duplicate of packet 0 must not append again.
        let ack_dup = t.accept_datagram(&p0.to_bytes()).unwrap().unwrap();
        assert_eq!(ack_dup.packet_id, 1);

        let p1 = DataPacket {
            connection_id: 42,
            packet_id: 1,
            payload: b"CD".to_vec(),
        };
        t.accept_datagram(&p1.to_bytes()).unwrap();
        assert_eq!(t.take_inbound(), b"ABCD");
    }

    #[test]
    fn a_datagram_for_another_connection_is_dropped() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = socket.local_addr().unwrap();
        let mut t = Transport {
            socket,
            peer,
            connection_id: 42,
            route: Route::Direct,
            next_id: 0,
            acked_in: 0,
            inbound: Vec::new(),
        };
        let stray = DataPacket {
            connection_id: 99,
            packet_id: 0,
            payload: b"x".to_vec(),
        };
        assert!(t.accept_datagram(&stray.to_bytes()).unwrap().is_none());
        assert!(t.take_inbound().is_empty());
    }

    #[test]
    fn outbound_framing_advances_the_packet_id() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = socket.local_addr().unwrap();
        let mut t = Transport {
            socket,
            peer,
            connection_id: 1,
            route: Route::Direct,
            next_id: 0,
            acked_in: 0,
            inbound: Vec::new(),
        };
        assert_eq!(t.frame_outbound(b"a").packet_id, 0);
        assert_eq!(t.frame_outbound(b"b").packet_id, 1);
    }
}
