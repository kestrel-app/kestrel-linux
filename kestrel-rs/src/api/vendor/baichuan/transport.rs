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

use std::collections::BTreeMap;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use log::{debug, info};

use super::udp::{self, Discovery, Endpoint, LookupReply, RegisterReply, MAGIC_ACK, MAGIC_DATA};
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
    /// The round trip as we see it, in microseconds. The device paces its resends
    /// by it: measured over a ~1.7s satellite path, reporting 0 had it resend after
    /// ~0.7s (before our ack could arrive), and reporting the round trip moved that
    /// to ~3s and roughly doubled main-stream goodput.
    pub latency_us: u32,
}

/// The ack header is 28 bytes: magic, connection id, a zero word, a group id, the
/// packet id, a latency-ish word, and a payload size (then that many bytes of a
/// missing-packet map). Measured: acking packets 0 then 1, the device put the packet
/// id at offset 16 — the word at offset 8 is always zero.
const ACK_HEADER: usize = 28;

/// A group id of all ones means "nothing received yet": the packet id is not an ack.
const ACK_NOTHING_YET: u32 = 0xffff_ffff;

impl AckPacket {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ACK_HEADER);
        out.extend_from_slice(&MAGIC_ACK.to_le_bytes());
        out.extend_from_slice(&self.connection_id.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // always zero
        out.extend_from_slice(&0u32.to_le_bytes()); // group id: a normal ack
        out.extend_from_slice(&self.packet_id.to_le_bytes());
        out.extend_from_slice(&self.latency_us.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // no missing-packet map
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<AckPacket> {
        if buf.len() < ACK_HEADER {
            return Err(Error::Protocol("ack packet too short".into()));
        }
        let word = |at: usize| u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
        if word(0) != MAGIC_ACK {
            return Err(Error::Protocol("not an ack packet".into()));
        }
        if word(12) == ACK_NOTHING_YET {
            return Err(Error::Protocol("ack: device has received nothing yet".into()));
        }
        Ok(AckPacket {
            connection_id: word(4) as i32,
            packet_id: word(16),
            latency_us: word(20),
        })
    }
}

/// How a device was reached, for logging and for deciding what video path to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The device answered on the local network: no internet in the path.
    Local,
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
    lookup_on(&socket, uid)
}

/// Resolve a UID using an existing socket, so the whole connection — lookup, register
/// and data channel — runs from one source port. The brokers tie a session to the
/// client's address, so reusing the socket is what makes the register that follows
/// actually complete (a fresh socket gets `R2C_T` but never `R2C_C_R`).
fn lookup_on(socket: &UdpSocket, uid: &str) -> Result<LookupReply> {
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

/// A reliable, ordered byte stream to a device reached by UID.
///
/// **Verification status.** The whole connect sequence and this reliable layer were
/// confirmed live against a real NVR (lookup → register → data channel → an
/// acknowledged login that returned `DeviceInfo`). The driver here is the in-Rust
/// port of that verified Python probe; its pure parts (the reassembler, the packet
/// framing) are unit-tested, while the socket I/O is exercised only against a real
/// device, since no device is reachable from CI and the Bash sandbox drops UDP. See
/// [`super`] and `docs/untested.md`.
#[derive(Debug)]
pub struct Transport {
    socket: UdpSocket,
    /// Where our data packets go (the device's mapped address, or a relay).
    peer: SocketAddr,
    route: Route,
    /// The id our *outbound* data packets carry — the device id (`did`).
    send_id: i32,
    /// The id the device's packets to us carry — our client id (`cid`).
    recv_id: i32,
    /// Next outbound packet id.
    next_out: u32,
    /// Next inbound packet id expected, for in-order reassembly.
    next_in: u32,
    /// Out-of-order inbound payloads held until their turn.
    pending: BTreeMap<u32, Vec<u8>>,
    /// Reassembled inbound bytes not yet drained by the reader.
    inbound: Vec<u8>,
    /// Highest acknowledged outbound packet id the device has reported.
    last_ack: Option<u32>,
    /// The data channel's round trip, reported in every ack we send.
    latency_us: u32,
}

impl Transport {
    /// Open a reliable stream to a device by UID: resolve it, register a session with
    /// the broker, and bring up the data channel (direct hole-punch, falling back to
    /// the relay).
    pub fn connect(uid: &str, timeout: Duration) -> Result<Transport> {
        // One socket for the whole flow — lookup, register and data channel — so the
        // broker sees a single client endpoint.
        let socket = UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| Error::connection(format!("transport: cannot open socket: {e}")))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|e| Error::connection(format!("transport: {e}")))?;

        let cid = rand_cid();

        // On the device's own network, talk to it there. Through the public
        // address instead, every packet can leave and come back over the uplink:
        // measured on an NVR behind satellite, that path carries ~35 KB/s, which
        // the sub stream fits and a 4K main stream never will.
        if let Ok((did, peer, rtt)) = run_local(&socket, uid, cid) {
            info!("Reolink P2P: found on the local network (did {did}, round trip {rtt:?})");
            return Ok(Transport::ready(socket, peer, Route::Local, did, cid).with_latency(rtt));
        }

        let reply = lookup_on(&socket, uid)?;
        let reg = reply
            .register
            .clone()
            .ok_or_else(|| Error::connection("lookup gave no register server"))?;
        let relay = reply
            .relay
            .clone()
            .ok_or_else(|| Error::connection("lookup gave no relay server"))?;

        let local = local_endpoint(&socket, &reg)?;

        // Register with the broker to get a session id and the device's addresses.
        let register = run_register(&socket, uid, &local, &reg, &relay, cid, timeout)?;
        let sid = register
            .sid
            .ok_or_else(|| Error::connection("register returned no session id"))?;

        // Bring up the data channel: prefer the direct mapped address, fall back to
        // the relay. The device confirms with a `did`.
        // The registrar also reports the device's own LAN address. Broadcasts do
        // not cross routers or VLANs, but a unicast to that address can — so try it
        // briefly before the public address.
        if let Some(dev) = register.dev.as_ref() {
            if let Ok((did, peer, rtt)) = run_datachannel(&socket, sid, "local", cid, dev, LOCAL_WAIT) {
                info!("Reolink P2P: reached at its LAN address (did {did}, round trip {rtt:?})");
                return Ok(Transport::ready(socket, peer, Route::Local, did, cid).with_latency(rtt));
            }
        }
        let direct = register.dmap.as_ref().or(register.dev.as_ref());
        if let Some(dmap) = direct {
            if let Ok((did, peer, rtt)) = run_datachannel(&socket, sid, "local", cid, dmap, timeout) {
                info!("Reolink P2P: direct data channel up (did {did}, round trip {rtt:?})");
                return Ok(Transport::ready(socket, peer, Route::Direct, did, cid).with_latency(rtt));
            }
            debug!("direct data channel failed; falling back to relay");
        }
        let relay_addr = register.relay.as_ref().unwrap_or(&relay);
        let (did, peer, rtt) = run_datachannel(&socket, sid, "relay", cid, relay_addr, timeout)?;
        info!("Reolink P2P: relayed data channel up (did {did}, round trip {rtt:?})");
        Ok(Transport::ready(socket, peer, Route::Relay, did, cid).with_latency(rtt))
    }

    fn ready(socket: UdpSocket, peer: SocketAddr, route: Route, send_id: i32, recv_id: i32) -> Self {
        Transport {
            socket,
            peer,
            route,
            send_id,
            recv_id,
            next_out: 0,
            next_in: 0,
            pending: BTreeMap::new(),
            inbound: Vec::new(),
            last_ack: None,
            latency_us: 0,
        }
    }

    fn with_latency(mut self, rtt: Duration) -> Self {
        self.latency_us = u32::try_from(rtt.as_micros()).unwrap_or(u32::MAX);
        self
    }

    pub fn route(&self) -> Route {
        self.route
    }

    /// Take an inbound datagram, reassemble its payload in order, and return the ACK
    /// to send back. Split out from the socket loop so the reliable-delivery logic is
    /// testable without a network. A datagram for another connection, or an ACK, is
    /// handled without producing an ACK of its own.
    pub fn accept_datagram(&mut self, buf: &[u8]) -> Result<Option<AckPacket>> {
        // An ACK for our outbound packets just advances `last_ack`. The device's
        // packets to us — data and acks alike — carry *our* client id (recv_id), not
        // the device id, so match on that. (Measured: a device acking our login packet
        // sent conn_id = cid.)
        if let Ok(ack) = AckPacket::from_bytes(buf) {
            if ack.connection_id == self.recv_id {
                self.last_ack = Some(self.last_ack.map_or(ack.packet_id, |p| p.max(ack.packet_id)));
            }
            return Ok(None);
        }
        let packet = DataPacket::from_bytes(buf)?;
        if packet.connection_id != self.recv_id {
            return Ok(None);
        }
        if packet.packet_id >= self.next_in {
            self.pending.entry(packet.packet_id).or_insert(packet.payload);
        }
        while let Some(payload) = self.pending.remove(&self.next_in) {
            self.inbound.extend_from_slice(&payload);
            self.next_in = self.next_in.wrapping_add(1);
        }
        // Acknowledge cumulatively: the ack's packet id means "I hold everything up
        // to here". Acking the id just received instead claims gaps are filled, so the
        // device never resends a lost packet and reassembly stalls — harmless at the
        // sub stream's bitrate, fatal for a main-stream keyframe spread over hundreds
        // of packets. Until packet 0 has arrived there is nothing contiguous to claim.
        if self.next_in == 0 {
            return Ok(None);
        }
        Ok(Some(AckPacket {
            connection_id: self.send_id,
            packet_id: self.next_in.wrapping_sub(1),
            latency_us: self.latency_us,
        }))
    }

    /// Frame outbound bytes as a data packet and advance the id.
    pub fn frame_outbound(&mut self, bytes: &[u8]) -> DataPacket {
        let packet = DataPacket {
            connection_id: self.send_id,
            packet_id: self.next_out,
            payload: bytes.to_vec(),
        };
        self.next_out = self.next_out.wrapping_add(1);
        packet
    }

    /// Drain whatever reassembled bytes are available, servicing the socket first so
    /// anything the device has sent is folded in.
    pub fn take_inbound(&mut self) -> Vec<u8> {
        let _ = self.pump(Duration::from_millis(200));
        std::mem::take(&mut self.inbound)
    }

    /// Service the socket for up to `budget`: reassemble inbound data (ACKing each
    /// packet) and record acknowledgements of our own sends.
    fn pump(&mut self, budget: Duration) -> Result<()> {
        let deadline = Instant::now() + budget;
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            match self.socket.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if let Ok(Some(ack)) = self.accept_datagram(&buf[..n]) {
                        let _ = self.socket.send_to(&ack.to_bytes(), self.peer);
                    }
                }
                Err(_) => break, // timed out: nothing more waiting
            }
        }
        Ok(())
    }

    /// Send a message reliably: fragment it, and resend until the device acknowledges
    /// the final fragment.
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        // The data-packet payload must fit the MTU; control messages are small, but
        // fragment defensively so a large one still goes through.
        const FRAGMENT: usize = 1024;
        let packets: Vec<DataPacket> = if bytes.is_empty() {
            vec![self.frame_outbound(bytes)]
        } else {
            bytes.chunks(FRAGMENT).map(|c| self.frame_outbound(c)).collect()
        };
        let last_id = packets.last().map(|p| p.packet_id).unwrap_or(0);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for packet in &packets {
                self.socket
                    .send_to(&packet.to_bytes(), self.peer)
                    .map_err(|e| Error::connection(format!("transport send: {e}")))?;
            }
            self.pump(Duration::from_millis(400))?;
            if self.last_ack.is_some_and(|a| a >= last_id) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::connection("transport send: no ack from device"));
            }
        }
    }
}

/// Find the local address the kernel would use to reach `reg`, so the register
/// request can report it. Connecting a UDP socket only sets its default peer.
fn local_endpoint(socket: &UdpSocket, reg: &Endpoint) -> Result<Endpoint> {
    let probe = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect((reg.ip.as_str(), reg.port))?;
            s.local_addr()
        })
        .map_err(|e| Error::connection(format!("transport: local address: {e}")))?;
    Ok(Endpoint {
        ip: probe.ip().to_string(),
        port: socket
            .local_addr()
            .map(|a| a.port())
            .map_err(|e| Error::connection(format!("transport: {e}")))?,
    })
}

/// Register with the broker and return its reply (session id + device addresses).
fn run_register(
    socket: &UdpSocket,
    uid: &str,
    local: &Endpoint,
    reg: &Endpoint,
    relay: &Endpoint,
    cid: i32,
    timeout: Duration,
) -> Result<RegisterReply> {
    let addr: SocketAddr = (reg.ip.as_str(), reg.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut i| i.next())
        .ok_or_else(|| Error::connection("register address did not resolve"))?;
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    // What we saw back, to turn a timeout into a diagnosis rather than a shrug.
    let mut any_reply = false;
    let mut saw_other = false;
    while Instant::now() < deadline {
        let req = udp::register_request(uid, local, relay, cid, rand_tid());
        let _ = socket.send_to(&req.to_bytes(), addr);
        // Drain whatever comes back for a short window before resending: the broker
        // sends `R2C_T` before `R2C_C_R`, and we want the latter.
        let drain_until = Instant::now() + Duration::from_millis(800);
        while Instant::now() < drain_until {
            match socket.recv_from(&mut buf) {
                Ok((n, _)) => {
                    any_reply = true;
                    if let Ok(disc) = Discovery::from_bytes(&buf[..n]) {
                        if disc.xml.contains("R2C_C_R") {
                            let reply = RegisterReply::parse(&disc.xml);
                            if reply.is_ok() {
                                return Ok(reply);
                            }
                            debug!("register: R2C_C_R not usable: {}", disc.xml);
                        } else {
                            saw_other = true;
                            debug!("register: waiting, got {}", first_tag(&disc.xml));
                        }
                    }
                }
                Err(_) => break, // read timed out: resend
            }
        }
    }
    let why = if !any_reply {
        "no reply from the register server (UDP to it may be blocked)"
    } else if saw_other {
        "register server answered but never completed the session (R2C_C_R); \
         the client address it was given may be wrong"
    } else {
        "register server replied but not usably"
    };
    Err(Error::connection(format!("register timed out: {why}")))
}

/// Bring up the data channel and return the device id and the address it answered
/// from (which is where subsequent data packets go).
fn run_datachannel(
    socket: &UdpSocket,
    sid: u32,
    conn: &str,
    cid: i32,
    target: &Endpoint,
    timeout: Duration,
) -> Result<(i32, SocketAddr, Duration)> {
    let addr: SocketAddr = (target.ip.as_str(), target.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut i| i.next())
        .ok_or_else(|| Error::connection("data-channel address did not resolve"))?;
    let started = Instant::now();
    let deadline = started + timeout;
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        let req = udp::datachannel_request(sid, conn, cid, rand_tid());
        let _ = socket.send_to(&req.to_bytes(), addr);
        if let Ok((n, from)) = socket.recv_from(&mut buf) {
            if let Ok(disc) = Discovery::from_bytes(&buf[..n]) {
                if let Some(did) = udp::datachannel_confirmed_did(&disc.xml) {
                    // From the first request: on a path slower than the read
                    // timeout, the confirm answers that one, not the latest.
                    return Ok((did, from, started.elapsed()));
                }
            }
        }
    }
    Err(Error::connection(format!(
        "data channel ({conn}) was not confirmed"
    )))
}

/// How long to look for the device on the local network before going through the
/// registrars. A device on the LAN answers in milliseconds; this is the price every
/// remote connect pays for checking.
const LOCAL_WAIT: Duration = Duration::from_millis(1500);

/// Broadcast `C2D_C` for the UID on the local network and wait for its `D2C_C_R`.
fn run_local(socket: &UdpSocket, uid: &str, cid: i32) -> Result<(i32, SocketAddr, Duration)> {
    socket
        .set_broadcast(true)
        .map_err(|e| Error::connection(format!("transport: broadcast: {e}")))?;
    let port = socket
        .local_addr()
        .map_err(|e| Error::connection(format!("transport: {e}")))?
        .port();
    let started = Instant::now();
    let mut buf = [0u8; 4096];
    while started.elapsed() < LOCAL_WAIT {
        let req = udp::local_connect_request(uid, port, cid, rand_tid()).to_bytes();
        for dest in [2015u16, 2018] {
            let _ = socket.send_to(&req, (std::net::Ipv4Addr::BROADCAST, dest));
        }
        // Read for a short slice, then broadcast again: a lost broadcast is common.
        let slice = Instant::now() + Duration::from_millis(300);
        while Instant::now() < slice {
            let Ok((n, from)) = socket.recv_from(&mut buf) else { break };
            if let Ok(disc) = Discovery::from_bytes(&buf[..n]) {
                if let Some(did) = udp::local_connect_did(&disc.xml, cid) {
                    return Ok((did, from, started.elapsed()));
                }
            }
        }
    }
    Err(Error::connection("not found on the local network"))
}

/// The name of the first inner element of a P2P XML body (e.g. `R2C_T`), for logs.
fn first_tag(xml: &str) -> &str {
    xml.split('<')
        .map(|s| s.trim_start_matches('/'))
        .find(|s| !s.is_empty() && *s != "P2P" && !s.starts_with('?'))
        .and_then(|s| s.split(['>', ' ', '/']).next())
        .unwrap_or("?")
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

/// A client id for a session: distinct per connection, in the range the app uses.
fn rand_cid() -> i32 {
    200_000 + (rand_tid() % 700_000) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a transport without connecting, for exercising the reassembler. The
    /// socket gets a short read timeout so `take_inbound`'s pump returns promptly
    /// (nothing is ever sent to it in these tests).
    fn test_transport(send_id: i32, recv_id: i32) -> Transport {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        let peer = socket.local_addr().unwrap();
        Transport::ready(socket, peer, Route::Direct, send_id, recv_id)
    }

    #[test]
    fn data_packet_round_trips() {
        let packet = DataPacket {
            connection_id: 254000,
            packet_id: 7,
            payload: b"hello device".to_vec(),
        };
        assert_eq!(DataPacket::from_bytes(&packet.to_bytes()).unwrap(), packet);
    }

    #[test]
    fn ack_packet_round_trips() {
        let ack = AckPacket {
            connection_id: 254000,
            packet_id: 8,
            latency_us: 1_700_000,
        };
        assert_eq!(AckPacket::from_bytes(&ack.to_bytes()).unwrap(), ack);
    }

    /// The layout a real device sent when acking packet 1: magic, conn id (our client
    /// id), zero, group 0, packet id 1, latency 0, payload size 0. The packet id lives
    /// at offset 16 — reading it from offset 8 (always zero) was the "no ack" bug.
    #[test]
    fn parses_a_captured_device_ack() {
        let mut wire = Vec::new();
        for w in [0x2a87_cf20u32, 297_136, 0, 0, 1, 0, 0] {
            wire.extend_from_slice(&w.to_le_bytes());
        }
        let ack = AckPacket::from_bytes(&wire).unwrap();
        assert_eq!(ack.connection_id, 297_136);
        assert_eq!(ack.packet_id, 1);
        assert_eq!(wire, ack.to_bytes(), "we emit the same 28-byte shape");
    }

    #[test]
    fn a_nothing_received_yet_ack_is_not_an_ack() {
        let mut wire = Vec::new();
        for w in [0x2a87_cf20u32, 5, 0, 0xffff_ffff, 0, 0, 0] {
            wire.extend_from_slice(&w.to_le_bytes());
        }
        assert!(AckPacket::from_bytes(&wire).is_err());
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

    /// Inbound device packets (tagged with our client id) reassemble in order; a
    /// duplicate is ignored, and the ACK covers what is now contiguous.
    #[test]
    fn reassembles_device_packets_in_order() {
        let mut t = test_transport(/*did*/ 42, /*cid*/ 99).with_latency(Duration::from_millis(1700));
        let p0 = DataPacket { connection_id: 99, packet_id: 0, payload: b"AB".to_vec() };
        let ack = t.accept_datagram(&p0.to_bytes()).unwrap().unwrap();
        assert_eq!(ack.connection_id, 42, "ack carries the device id");
        assert_eq!(ack.packet_id, 0, "ack covers packet 0");
        assert_eq!(ack.latency_us, 1_700_000, "every ack reports the round trip");
        // Duplicate must not append again.
        t.accept_datagram(&p0.to_bytes()).unwrap();
        let p1 = DataPacket { connection_id: 99, packet_id: 1, payload: b"CD".to_vec() };
        t.accept_datagram(&p1.to_bytes()).unwrap();
        assert_eq!(t.take_inbound(), b"ABCD");
    }

    /// An out-of-order packet is held until the gap is filled.
    #[test]
    fn out_of_order_packets_are_buffered() {
        let mut t = test_transport(42, 99);
        let p1 = DataPacket { connection_id: 99, packet_id: 1, payload: b"CD".to_vec() };
        // Packet 0 is missing, so nothing is contiguous yet: no ack, or the device would
        // take the gap as filled and never resend it.
        assert!(t.accept_datagram(&p1.to_bytes()).unwrap().is_none());
        assert!(t.take_inbound().is_empty(), "nothing until packet 0 arrives");
        let p0 = DataPacket { connection_id: 99, packet_id: 0, payload: b"AB".to_vec() };
        let ack = t.accept_datagram(&p0.to_bytes()).unwrap().unwrap();
        assert_eq!(ack.packet_id, 1, "both held now: ack is cumulative up to 1");
        assert_eq!(t.take_inbound(), b"ABCD");
    }

    /// After a gap, acks keep reporting the last contiguous packet, not the latest one.
    #[test]
    fn ack_stays_at_the_gap_until_it_is_filled() {
        let mut t = test_transport(42, 99);
        let pkt = |id| DataPacket { connection_id: 99, packet_id: id, payload: vec![id as u8] };
        assert_eq!(t.accept_datagram(&pkt(0).to_bytes()).unwrap().unwrap().packet_id, 0);
        // Packet 1 lost; 2 and 3 arrive.
        assert_eq!(t.accept_datagram(&pkt(2).to_bytes()).unwrap().unwrap().packet_id, 0);
        assert_eq!(t.accept_datagram(&pkt(3).to_bytes()).unwrap().unwrap().packet_id, 0);
        // The resend of 1 fills the gap and the ack jumps to 3.
        assert_eq!(t.accept_datagram(&pkt(1).to_bytes()).unwrap().unwrap().packet_id, 3);
        assert_eq!(t.take_inbound(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_datagram_for_another_connection_is_dropped() {
        let mut t = test_transport(42, 99);
        let stray = DataPacket { connection_id: 7, packet_id: 0, payload: b"x".to_vec() };
        assert!(t.accept_datagram(&stray.to_bytes()).unwrap().is_none());
        assert!(t.take_inbound().is_empty());
    }

    #[test]
    fn an_ack_advances_last_ack_without_replying() {
        // The device acks with our client id (recv_id = 99), not the device id.
        let mut t = test_transport(42, 99);
        let ack = AckPacket { connection_id: 99, packet_id: 3, latency_us: 0 };
        assert!(t.accept_datagram(&ack.to_bytes()).unwrap().is_none());
        assert_eq!(t.last_ack, Some(3));
        // An ack carrying the device id (not what the device sends) is ignored.
        let wrong = AckPacket { connection_id: 42, packet_id: 9, latency_us: 0 };
        assert!(t.accept_datagram(&wrong.to_bytes()).unwrap().is_none());
        assert_eq!(t.last_ack, Some(3), "unchanged");
    }

    #[test]
    fn outbound_framing_advances_the_packet_id() {
        let mut t = test_transport(1, 2);
        assert_eq!(t.frame_outbound(b"a").packet_id, 0);
        assert_eq!(t.frame_outbound(b"b").packet_id, 1);
    }
}
