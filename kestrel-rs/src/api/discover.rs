//! Finding devices on the local network, so nobody has to know an address.
//!
//! Two strategies, because no one of them finds everything.
//!
//! **Ask, for anything that answers.** ONVIF devices join a multicast group and
//! answer a `Probe` with their own addresses. That is one packet and about two
//! seconds for every camera on the network, and it finds them whatever their
//! address is — including the vendors here that speak ONVIF as a second
//! language, which is most of them.
//!
//! **Look, for everything else.** Frigate, ZoneMinder, QNAP QVR and UniFi
//! Protect are software on a server. None of them announces itself, so the only
//! way to find one is to look where it would be: the networks this machine is
//! actually attached to, on the handful of ports these systems use. A camera
//! with ONVIF switched off — which is the factory setting on some — is found
//! the same way.
//!
//! Whatever finds a host, [`crate::api::vendor::detect`] decides what it *is*.
//! That matters: a Reolink NVR answers the ONVIF probe, and reporting it as a
//! generic ONVIF device would trade playback, floodlights and detections for a
//! bare stream.
//!
//! **The sweep is bounded on purpose.** Only directly-attached networks, never
//! anything larger than a `/22`, and only ports these systems serve. Kestrel
//! should find the NVR in the next room; it has no business enumerating a
//! corporate network, and a tool that walks off its own subnet is one nobody
//! should run at work.

use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::vendor::{self, Detected};

/// How a device came to light, so the list can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// It answered the ONVIF discovery probe.
    Announced,
    /// It was found by looking at the addresses on this network.
    Scanned,
}

impl Via {
    pub fn label(self) -> &'static str {
        match self {
            Via::Announced => "announced itself",
            Via::Scanned => "found on the network",
        }
    }
}

/// One device worth offering to add.
#[derive(Debug, Clone)]
pub struct Found {
    pub host: String,
    pub port: u16,
    pub https: bool,
    pub vendor: &'static str,
    pub detail: String,
    pub via: Via,
}

impl Found {
    /// The vendor's own name for itself, for a list the user reads.
    pub fn label(&self) -> &'static str {
        vendor::label_for(self.vendor)
    }
}

/// The ports these systems listen on, in the order most likely to answer.
///
/// Deliberately short. Every entry costs one connection attempt against every
/// address on the network, so a port that no supported system uses is pure
/// delay — and a long list is a port scanner rather than a discovery feature.
const PORTS: [(u16, bool); 6] = [
    (80, false),   // Reolink, ONVIF, ZoneMinder
    (443, true),   // UniFi Protect
    (8000, false), // ONVIF, where 80 is the web interface
    (5000, false), // Frigate
    (8080, false), // QNAP QVR
    (8443, true),  // UniFi Protect, older consoles
];

/// How long to wait for a TCP handshake from an address on the local network.
///
/// Local means milliseconds. This is long enough for a busy appliance and short
/// enough that a whole `/24` of empty addresses is a few seconds rather than
/// minutes — and an address with nothing on it usually refuses immediately
/// rather than timing out at all.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);

/// How wide a network this will sweep, as a prefix length.
///
/// A `/22` is a thousand addresses, which is about four seconds of scanning and
/// a reasonable ceiling for a home or small site. Anything larger is a network
/// with an administrator, who can type the address.
const WIDEST_PREFIX: u32 = 22;

// -------------------------------------------------------------- the networks

/// The IPv4 networks this machine is directly attached to.
///
/// Read from the kernel's routing table rather than guessed: a route with no
/// gateway is a network on the other end of a cable, which is exactly the set
/// worth looking at. Anything reached *through* a gateway is somewhere else and
/// is none of this feature's business.
pub fn local_networks() -> Vec<(Ipv4Addr, u32)> {
    match std::fs::read_to_string("/proc/net/route") {
        Ok(table) => parse_routes(&table),
        Err(err) => {
            log::warn!("discovery: cannot read the routing table ({err})");
            Vec::new()
        }
    }
}

/// Parse `/proc/net/route`.
///
/// The columns are little-endian hex, which is the one detail worth being
/// careful about: `000200C0` is 192.0.2.0, not 0.2.0.192.
pub(crate) fn parse_routes(table: &str) -> Vec<(Ipv4Addr, u32)> {
    let mut out: Vec<(Ipv4Addr, u32)> = Vec::new();

    for line in table.lines().skip(1) {
        let mut columns = line.split_whitespace();
        let (Some(iface), Some(destination), Some(gateway)) =
            (columns.next(), columns.next(), columns.next())
        else {
            continue;
        };
        let Some(mask) = columns.nth(4) else { continue };

        if iface == "lo" {
            continue;
        }
        let (Ok(destination), Ok(gateway), Ok(mask)) = (
            u32::from_str_radix(destination, 16),
            u32::from_str_radix(gateway, 16),
            u32::from_str_radix(mask, 16),
        ) else {
            continue;
        };
        // A gateway means the network is somewhere else; a zero mask is the
        // default route, which is every address there is.
        if gateway != 0 || mask == 0 {
            continue;
        }

        let mask = mask.swap_bytes();
        // Contiguous masks only. A non-contiguous one is not something to guess
        // at, and `leading_ones` is only a prefix length if the rest are zeros.
        let prefix = mask.leading_ones();
        if mask.count_ones() != prefix {
            continue;
        }
        if prefix < WIDEST_PREFIX || prefix > 30 {
            continue;
        }

        let network = Ipv4Addr::from(destination.swap_bytes());
        if network.is_loopback() || network.is_link_local() {
            continue;
        }
        if !out.iter().any(|(net, len)| *net == network && *len == prefix) {
            out.push((network, prefix));
        }
    }
    out
}

/// Every address worth trying on a network, leaving out the ones that are not
/// hosts.
pub(crate) fn hosts_in(network: Ipv4Addr, prefix: u32) -> Vec<Ipv4Addr> {
    if prefix < WIDEST_PREFIX || prefix > 30 {
        return Vec::new();
    }
    let base = u32::from(network);
    let count = 1u32 << (32 - prefix);
    // The network address and the broadcast address are not hosts.
    (1..count.saturating_sub(1))
        .map(|offset| Ipv4Addr::from(base + offset))
        .collect()
}

// ------------------------------------------------------------- ws-discovery

const WS_DISCOVERY_PORT: u16 = 3702;
const WS_DISCOVERY_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

/// The ONVIF discovery probe.
///
/// `NetworkVideoTransmitter` is the ONVIF device type; a plain `Probe` with no
/// types would also catch printers and whatever else implements WS-Discovery on
/// the network, which is not what anyone pressed the button for.
pub(crate) fn probe_message(message_id: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope""#,
            r#" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing""#,
            r#" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery""#,
            r#" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">"#,
            r#"<e:Header><w:MessageID>uuid:{}</w:MessageID>"#,
            r#"<w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>"#,
            r#"<w:Action e:mustUnderstand="true">"#,
            r#"http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header>"#,
            r#"<e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body>"#,
            r#"</e:Envelope>"#,
        ),
        message_id
    )
}

/// The addresses a `ProbeMatches` reply points at.
///
/// A device lists every address it can be reached on in one space-separated
/// `XAddrs`, which for a camera with two interfaces means two URLs — and for a
/// camera that has ever had a different address sometimes means a stale one. All
/// of them are returned; whether any answers is settled by trying.
pub(crate) fn parse_probe_matches(body: &str) -> Vec<String> {
    let Ok(doc) = roxmltree::Document::parse(body) else {
        return Vec::new();
    };
    doc.root_element()
        .descendants()
        .filter(|n| n.tag_name().name() == "XAddrs")
        .filter_map(|n| n.text())
        .flat_map(|text| text.split_whitespace())
        .map(str::to_string)
        .collect()
}

/// The host, port and scheme out of a URL, without pulling in a URL parser for
/// the four fields that matter.
pub(crate) fn split_url(url: &str) -> Option<(String, u16, bool)> {
    let (scheme, rest) = url.split_once("://")?;
    let https = scheme.eq_ignore_ascii_case("https");
    let rest = rest.rsplit_once('@').map(|(_, tail)| tail).unwrap_or(rest);
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    if authority.is_empty() {
        return None;
    }

    // An IPv6 literal is bracketed, and splitting it on ':' would take it apart.
    if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
        let host = &authority[1..=end];
        let port = authority[end + 2..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(if https { 443 } else { 80 });
        return Some((host.to_string(), port, https));
    }

    match authority.split_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?, https)),
        None => Some((
            authority.to_string(),
            if https { 443 } else { 80 },
            https,
        )),
    }
}

/// Send the probe and gather what answers, for as long as `window` allows.
///
/// Replies arrive unicast to the port the probe went out from, so there is no
/// group to join — only a socket to keep open long enough. Devices deliberately
/// answer after a random delay to avoid all replying at once, which is why this
/// waits out the whole window instead of stopping at the first answer.
fn ws_discovery(window: Duration) -> Vec<String> {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        Ok(socket) => socket,
        Err(err) => {
            log::warn!("discovery: no UDP socket ({err})");
            return Vec::new();
        }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));
    let _ = socket.set_broadcast(true);

    let message = probe_message(&format!(
        "{:08x}-0000-4000-8000-{:012x}",
        std::process::id(),
        Instant::now().elapsed().as_nanos() as u64 & 0xffff_ffff_ffff,
    ));

    // Multicast is the standard; the broadcast address is a cheap second try for
    // devices whose multicast is broken or filtered, which is common enough on
    // consumer switches to be worth one extra packet.
    for target in [
        SocketAddr::from((WS_DISCOVERY_GROUP, WS_DISCOVERY_PORT)),
        SocketAddr::from((Ipv4Addr::BROADCAST, WS_DISCOVERY_PORT)),
    ] {
        if let Err(err) = socket.send_to(message.as_bytes(), target) {
            log::debug!("discovery: probe to {target} refused ({err})");
        }
    }

    let deadline = Instant::now() + window;
    let mut found = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    while Instant::now() < deadline {
        let Ok((len, from)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        let body = String::from_utf8_lossy(&buffer[..len]);
        for address in parse_probe_matches(&body) {
            log::debug!("discovery: {from} offers {address}");
            found.push(address);
        }
    }
    found
}

// --------------------------------------------------------------- the sweep

/// Whether anything is listening, without saying a word to it.
fn port_is_open(host: Ipv4Addr, port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from((host, port)), CONNECT_TIMEOUT).is_ok()
}

/// How many threads to sweep with.
///
/// The work is entirely waiting for sockets, so this is far above the core
/// count on purpose and is capped to stay a well-behaved neighbour: a hundred
/// simultaneous connections is enough to cross a `/24` in a couple of seconds
/// and not enough to bother a switch.
const SWEEP_THREADS: usize = 64;

/// Try every address on the given networks and report the ones with something
/// listening.
fn sweep(
    networks: &[(Ipv4Addr, u32)],
    cancel: &AtomicBool,
    done: &AtomicU32,
    total: &AtomicU32,
) -> Vec<(Ipv4Addr, u16, bool)> {
    let mut addresses: Vec<Ipv4Addr> = Vec::new();
    for (network, prefix) in networks {
        addresses.extend(hosts_in(*network, *prefix));
    }
    addresses.sort();
    addresses.dedup();
    total.store(addresses.len() as u32, Ordering::Relaxed);
    if addresses.is_empty() {
        return Vec::new();
    }

    let queue = Arc::new(std::sync::Mutex::new(addresses.into_iter()));
    let hits = Arc::new(std::sync::Mutex::new(Vec::new()));

    std::thread::scope(|scope| {
        for _ in 0..SWEEP_THREADS {
            let queue = Arc::clone(&queue);
            let hits = Arc::clone(&hits);
            scope.spawn(move || loop {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let Some(host) = queue.lock().unwrap().next() else {
                    return;
                };
                for (port, https) in PORTS {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    if port_is_open(host, port) {
                        hits.lock().unwrap().push((host, port, https));
                        // One open port is enough to hand this address to
                        // `detect`, which will try the others itself.
                        break;
                    }
                }
                done.fetch_add(1, Ordering::Relaxed);
            });
        }
    });

    let mut hits = Arc::try_unwrap(hits).unwrap().into_inner().unwrap();
    hits.sort();
    hits
}

// ---------------------------------------------------------------- the whole

/// How far along a scan is, for a dialog that has to show something.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU32,
    pub total: AtomicU32,
    pub cancel: AtomicBool,
}

impl Progress {
    /// A number between 0 and 1, or `None` before the size is known.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total.load(Ordering::Relaxed);
        (total > 0).then(|| {
            (self.done.load(Ordering::Relaxed) as f32 / total as f32).clamp(0.0, 1.0)
        })
    }

    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Find what is out there.
///
/// Runs the announcement and the sweep, then asks each candidate what it is.
/// Blocking and slow by nature — a few seconds at best — so it belongs on a
/// thread, which is how [`crate::ui::device_dialog`] calls it.
pub fn scan(progress: &Progress) -> Vec<Found> {
    let networks = local_networks();
    log::info!(
        "discovery: {} local network(s): {}",
        networks.len(),
        networks
            .iter()
            .map(|(net, len)| format!("{net}/{len}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Candidates as (host, port, https), announced ones first so a device that
    // both announces itself and is on a swept network is only probed once.
    let mut candidates: Vec<(String, u16, bool, Via)> = Vec::new();
    for address in ws_discovery(Duration::from_millis(2500)) {
        if let Some((host, port, https)) = split_url(&address) {
            candidates.push((host, port, https, Via::Announced));
        }
    }
    log::info!("discovery: {} announced address(es)", candidates.len());

    if !progress.cancel.load(Ordering::Relaxed) {
        for (host, port, https) in sweep(&networks, &progress.cancel, &progress.done, &progress.total)
        {
            candidates.push((host.to_string(), port, https, Via::Scanned));
        }
    }

    // One entry per host: the same box on two ports is one device to add.
    let mut seen: Vec<String> = Vec::new();
    let mut found = Vec::new();
    for (host, port, https, via) in candidates {
        if progress.cancel.load(Ordering::Relaxed) {
            break;
        }
        if seen.contains(&host) {
            continue;
        }
        seen.push(host.clone());

        // Self-signed certificates are the norm on this hardware, and this is a
        // probe rather than a session carrying a password. What the user
        // actually saves still defaults to verifying.
        let Some(Detected {
            vendor,
            detail,
            port,
            https,
        }) = vendor::detect(&host, port, https, true)
        else {
            log::debug!("discovery: {host}:{port} did not identify itself");
            continue;
        };
        // Where it actually answered, which is not always where it was tried:
        // `detect` falls back to each vendor's own default port.
        found.push(Found {
            host,
            port,
            https,
            vendor,
            detail,
            via,
        });
    }

    found.sort_by(|a, b| a.host.cmp(&b.host));
    log::info!("discovery: {} device(s) identified", found.len());
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real routing table, with the columns in the order the kernel writes
    /// them and the values in the endianness it writes them in.
    const ROUTES: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
eth0\t000200C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
lo\t0000007F\t00000000\t0001\t0\t0\t0\t000000FF\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
";

    #[test]
    fn the_attached_networks_are_read_from_the_routing_table() {
        let networks = parse_routes(ROUTES);
        // The /24 only. The default route has a gateway, loopback is skipped
        // by name, and docker0's /16 is a thousand times wider than anything
        // worth sweeping, so it is refused by the width limit.
        assert_eq!(networks, vec![(Ipv4Addr::new(192, 0, 2, 0), 24)], "{networks:?}");
        assert!(
            !networks.iter().any(|(net, _)| net.is_loopback()),
            "loopback is not somewhere to look for cameras"
        );
        // The default route is every address there is and must never be swept.
        assert!(
            !networks.contains(&(Ipv4Addr::UNSPECIFIED, 0)),
            "the default route was taken as a network to scan"
        );
    }

    /// The one detail that is easy to get backwards, and silently: the columns
    /// are little-endian.
    #[test]
    fn route_addresses_are_read_little_endian() {
        let table = format!(
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
             eth0\t000200C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\n"
        );
        assert_eq!(
            parse_routes(&table),
            vec![(Ipv4Addr::new(192, 0, 2, 0), 24)],
            "000200C0 is 192.0.2.0, not 0.2.0.192"
        );
    }

    /// Nothing wider than the ceiling, whatever the routing table says.
    #[test]
    fn a_network_too_wide_to_sweep_is_left_alone() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
             eth0\t0000000A\t00000000\t0001\t0\t0\t100\t000000FF\n";
        assert!(
            parse_routes(table).is_empty(),
            "a /8 is sixteen million addresses and must be refused"
        );
        assert!(hosts_in(Ipv4Addr::new(10, 0, 0, 0), 8).is_empty());
    }

    #[test]
    fn a_network_expands_to_its_hosts() {
        let hosts = hosts_in(Ipv4Addr::new(192, 0, 2, 0), 24);
        assert_eq!(hosts.len(), 254, "256 addresses, less the network and the broadcast");
        assert_eq!(hosts[0], Ipv4Addr::new(192, 0, 2, 1));
        assert_eq!(*hosts.last().unwrap(), Ipv4Addr::new(192, 0, 2, 254));
        assert!(
            !hosts.contains(&Ipv4Addr::new(192, 0, 2, 0)),
            "the network address is not a host"
        );
        assert!(
            !hosts.contains(&Ipv4Addr::new(192, 0, 2, 255)),
            "the broadcast address is not a host"
        );
    }

    #[test]
    fn the_probe_asks_for_cameras_rather_than_for_everything() {
        let message = probe_message("abc");
        assert!(message.contains("NetworkVideoTransmitter"), "{message}");
        assert!(message.contains("uuid:abc"), "{message}");
        assert!(message.contains("discovery/Probe"), "{message}");
    }

    /// A real ProbeMatches, with the prefixes a device actually uses and two
    /// addresses in one XAddrs.
    #[test]
    fn addresses_are_read_out_of_a_probe_reply() {
        let body = r#"<?xml version="1.0"?>
        <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope"
                           xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery">
          <SOAP-ENV:Body><d:ProbeMatches><d:ProbeMatch>
            <d:XAddrs>http://192.0.2.31/onvif/device_service http://[fe80::1]/onvif/device_service</d:XAddrs>
            <d:MetadataVersion>1</d:MetadataVersion>
          </d:ProbeMatch></d:ProbeMatches></SOAP-ENV:Body></SOAP-ENV:Envelope>"#;
        let addresses = parse_probe_matches(body);
        assert_eq!(addresses.len(), 2, "{addresses:?}");
        assert_eq!(addresses[0], "http://192.0.2.31/onvif/device_service");
    }

    #[test]
    fn a_reply_that_is_not_xml_finds_nothing_rather_than_panicking() {
        assert!(parse_probe_matches("").is_empty());
        assert!(parse_probe_matches("<d:ProbeMatches><d:XAdd").is_empty());
    }

    #[test]
    fn an_address_splits_into_host_port_and_scheme() {
        assert_eq!(
            split_url("http://192.0.2.31:8000/onvif/device_service"),
            Some(("192.0.2.31".into(), 8000, false))
        );
        // No port means the scheme's own.
        assert_eq!(
            split_url("http://192.0.2.31/onvif"),
            Some(("192.0.2.31".into(), 80, false))
        );
        assert_eq!(
            split_url("https://camera.lan/onvif"),
            Some(("camera.lan".into(), 443, true))
        );
        // An IPv6 literal must not be taken apart at its own colons.
        assert_eq!(
            split_url("http://[fe80::1]:8000/onvif"),
            Some(("fe80::1".into(), 8000, false))
        );
        assert_eq!(split_url("not a url"), None);
    }

    #[test]
    fn progress_is_a_fraction_only_once_the_size_is_known() {
        let progress = Progress::default();
        assert_eq!(progress.fraction(), None);
        progress.total.store(200, Ordering::Relaxed);
        progress.done.store(50, Ordering::Relaxed);
        assert_eq!(progress.fraction(), Some(0.25));
        // Never past the end, however the counters race.
        progress.done.store(500, Ordering::Relaxed);
        assert_eq!(progress.fraction(), Some(1.0));
    }
}
