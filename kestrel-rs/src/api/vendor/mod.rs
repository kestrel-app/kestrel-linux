//! What every supported system has to provide.
//!
//! The client spoke Reolink everywhere. It now goes through this trait, with
//! Reolink as one implementation of a contract each system provides: connect,
//! enumerate cameras, hand back a stream and a still, and release the session.
//! Adding a vendor means writing that module and listing it in [`VENDORS`] —
//! nothing above this seam changes.
//!
//! **Capabilities are reported, not assumed.** PTZ, presets, floodlights,
//! recordings and detections are optional; the default implementations refuse
//! with [`Error::Unsupported`], and each [`Channel`] carries flags the UI reads
//! to decide what to offer. A system without PTZ shows no PTZ pane rather than
//! buttons that would only error.
//!
//! **A camera identifier is whatever the vendor calls it.** Reolink numbers
//! channels, Frigate names them, QNAP and UniFi use GUIDs. [`Channel::index`]
//! stays a position for the UI to key on, while [`Channel::key`] carries the
//! vendor's own identifier and is what URLs are built from.

pub mod frigate;
pub mod onvif;
pub mod qnap;
pub mod reolink;
pub mod unifi;
pub mod zoneminder;

use crate::api::error::{Error, Result};
use crate::api::http;
use crate::api::models::{Channel, DeviceInfo, Recording, StreamType};
use crate::api::settings::Block;
use crate::config::DeviceConfig;

/// A system Kestrel can talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
}

/// Every vendor, in the order they are offered.
pub const VENDORS: [VendorInfo; 6] = [
    VendorInfo {
        id: "reolink",
        label: "Reolink",
        detail: "NVR or standalone camera",
    },
    VendorInfo {
        id: "frigate",
        label: "Frigate",
        detail: "NVR with object detection",
    },
    VendorInfo {
        id: "zoneminder",
        label: "ZoneMinder",
        detail: "1.36 or newer",
    },
    VendorInfo {
        id: "qnap",
        label: "QNAP QVR",
        detail: "QVR Pro or QVR Elite",
    },
    VendorInfo {
        id: "unifi",
        label: "UniFi Protect",
        detail: "local account required",
    },
    // Last, and deliberately so. ONVIF is what a camera answers when it answers
    // nothing else, so it is the fallback rather than a peer: a Reolink NVR
    // speaks it too, and identifying one as a generic ONVIF device would trade
    // playback, floodlights and detections for a stream and nothing else.
    VendorInfo {
        id: "onvif",
        label: "ONVIF",
        detail: "most other cameras and NVRs",
    },
];

/// Reolink is the default, so a device saved before vendors existed lands on
/// the one it was actually configured for.
pub const DEFAULT_VENDOR: &str = "reolink";

pub fn label_for(id: &str) -> &'static str {
    VENDORS
        .iter()
        .find(|v| v.id == id)
        .map(|v| v.label)
        .unwrap_or("Reolink")
}

pub fn info_for(id: &str) -> VendorInfo {
    VENDORS
        .iter()
        .find(|v| v.id == id)
        .copied()
        .unwrap_or(VENDORS[0])
}

/// Where video comes from, and what is needed to fetch it.
///
/// `headers` exists for UniFi, whose streams sit behind the same session as its
/// API. ffmpeg is given them as its `headers` option.
#[derive(Debug, Clone, Default)]
pub struct StreamSource {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl StreamSource {
    pub fn new(url: impl Into<String>) -> Self {
        StreamSource {
            url: url.into(),
            headers: Vec::new(),
        }
    }

    /// The headers as ffmpeg wants them: CRLF-separated, or nothing at all.
    pub fn header_blob(&self) -> Option<String> {
        if self.headers.is_empty() {
            return None;
        }
        let mut blob = String::new();
        for (name, value) in &self.headers {
            blob.push_str(name);
            blob.push_str(": ");
            blob.push_str(value);
            blob.push_str("\r\n");
        }
        Some(blob)
    }
}

fn unsupported(what: &str, vendor: &str) -> Error {
    Error::Unsupported(format!(
        "{what} is not available on {}",
        label_for(vendor)
    ))
}

/// The operations a device supports.
///
/// Everything below the first group has a default that refuses, so a new vendor
/// starts out honest: it reports what it can do and nothing more.
pub trait Vendor: Send + Sync {
    /// Which vendor this is, for messages and for the refusals above.
    fn vendor_id(&self) -> &'static str;

    /// Log in and enumerate cameras. Called once, before the device is shared.
    fn connect(&mut self) -> Result<DeviceInfo>;

    /// Release the session. Reolink holds a token for its full lease whether or
    /// not the socket closed, so this matters more than it looks.
    fn logout(&self);

    fn channels(&self) -> &[Channel];

    /// Live video for one camera.
    fn stream(&self, channel: &Channel, stream: StreamType) -> Result<StreamSource>;

    /// A full-resolution still, fetched from the device rather than grabbed
    /// from the decoded picture.
    fn snapshot(&self, channel: &Channel) -> Result<Vec<u8>>;

    // ------------------------------------------------------------ detections

    /// Which cameras are detecting, and what. A system without detection
    /// returns an empty list rather than an error: it is not a failure, there
    /// is simply nothing to report.
    fn detections(&self, _channels: &[u32]) -> Result<Vec<(u32, Vec<(String, bool)>)>> {
        Ok(Vec::new())
    }

    // ------------------------------------------------------------------- PTZ

    fn ptz_move(&self, _channel: u32, _direction: &str, _speed: i64) -> Result<()> {
        Err(unsupported("PTZ", self.vendor_id()))
    }
    fn ptz_stop(&self, _channel: u32) -> Result<()> {
        Err(unsupported("PTZ", self.vendor_id()))
    }
    fn ptz_presets(&self, _channel: u32) -> Result<Vec<(i64, String)>> {
        Err(unsupported("Presets", self.vendor_id()))
    }
    fn ptz_goto_preset(&self, _channel: u32, _preset: i64, _speed: i64) -> Result<()> {
        Err(unsupported("Presets", self.vendor_id()))
    }
    fn ptz_go_home(&self, _channel: u32) -> Result<()> {
        Err(unsupported("PTZ", self.vendor_id()))
    }
    fn ptz_calibrate(&self, _channel: u32) -> Result<()> {
        Err(unsupported("Calibration", self.vendor_id()))
    }

    // ------------------------------------------------------------ floodlight

    fn white_led(&self, _channel: u32) -> Result<Block> {
        Err(unsupported("Floodlight control", self.vendor_id()))
    }
    fn set_floodlight(&self, _block: &Block, _field: &str, _value: i64) -> Result<Block> {
        Err(unsupported("Floodlight control", self.vendor_id()))
    }

    // -------------------------------------------------------------- playback

    fn search_recordings(
        &self,
        _channel: u32,
        _start: chrono::NaiveDateTime,
        _end: chrono::NaiveDateTime,
        _stream: StreamType,
    ) -> Result<Vec<Recording>> {
        Err(unsupported("Playback", self.vendor_id()))
    }
    fn recorded_days(
        &self,
        _channel: u32,
        _month_of: chrono::NaiveDate,
        _stream: StreamType,
    ) -> Result<Vec<u32>> {
        Err(unsupported("Playback", self.vendor_id()))
    }
    fn download_url(&self, _recording: &Recording) -> Result<Option<String>> {
        Err(unsupported("Playback", self.vendor_id()))
    }

    /// Whether this system can serve recordings at all, so the Playback tab can
    /// say so rather than presenting an empty calendar.
    fn supports_playback(&self) -> bool {
        false
    }
}

/// What a probe found at an address.
#[derive(Debug, Clone, PartialEq)]
pub struct Detected {
    pub vendor: &'static str,
    /// What to tell the user, including a version where one was offered.
    pub detail: String,
    /// Where it answered, which is not always where we were told to look.
    pub port: u16,
    pub https: bool,
}

/// The port each system listens on when nobody has changed it.
fn default_port(vendor: &str) -> (u16, bool) {
    match vendor {
        "frigate" => (5000, false),
        "unifi" => (443, true),
        "qnap" => (8080, false),
        // 80 is the common answer and the one the standard implies, but a large
        // minority of cameras - Hikvision's among them - put ONVIF on 8000 and
        // leave the web interface on 80.
        "onvif" => (80, false),
        _ => (80, false),
    }
}

/// The order the probes are tried in, which is not the order the vendors are
/// listed in.
///
/// Most specific first, most general last. The distinction that matters is the
/// final pair: a Reolink NVR with ONVIF switched on answers both probes, and
/// whichever runs first is what the device gets saved as. Identified as ONVIF it
/// would lose playback, floodlight control, presets and detections — every one
/// of which the Reolink module has and the ONVIF one does not.
///
/// So ONVIF is last, always. It is the answer to "nothing else recognised
/// this", never the answer to "this also speaks ONVIF".
const PROBE_ORDER: [&str; 6] = [
    "frigate",
    "unifi",
    "zoneminder",
    "qnap",
    "reolink",
    "onvif",
];

/// Work out what a host is running, so the user does not have to know.
///
/// Each system answers something distinctive *without credentials* — a version
/// string, an XML document root, a particular refusal — so this runs before
/// anything has been typed but an address. The probes are ordered cheapest and
/// most certain first, and each is given a short timeout, because a wrong guess
/// should cost a moment rather than the whole attempt.
///
/// Each vendor is tried at the configured port and, if that differs, at its own
/// default: someone who does not know what the system is will not know which
/// port it listens on either.
///
/// `None` is not a failure. It means nothing answered recognisably, so whatever
/// is already selected stands and the user chooses.
pub fn detect(
    host: &str,
    configured_port: u16,
    https: bool,
    allow_self_signed: bool,
) -> Option<Detected> {
    let agent = http::agent(4, allow_self_signed);

    for vendor in PROBE_ORDER {
        let (port, secure) = default_port(vendor);
        // The configured port first: it is what the user actually typed.
        let mut attempts = vec![(configured_port, https)];
        if port != configured_port {
            attempts.push((port, secure));
        }

        for (port, secure) in attempts {
            let base = http::base_url(host, port, secure);
            if let Some(detail) = probe(&agent, vendor, &base) {
                return Some(Detected {
                    vendor,
                    detail,
                    port,
                    https: secure,
                });
            }
        }
    }
    None
}

/// One vendor's identifying question. `Some(detail)` means it answered as
/// itself.
fn probe(agent: &ureq::Agent, vendor: &str, base: &str) -> Option<String> {
    let get = |path: &str| http::request(agent, "GET", &format!("{base}{path}"), None, &[]).ok();

    match vendor {
        // A bare version string on a path nothing else serves.
        "frigate" => {
            let response = get("/api/version")?;
            let body = response.body.trim();
            if response.ok() && !body.is_empty() && body.len() < 40 && !body.starts_with('<') {
                return Some(format!("Frigate {body}"));
            }
            None
        }
        // The bootstrap exists but refuses without a session. A 401 here is a
        // positive identification, not an error.
        "unifi" => {
            let response = get("/proxy/protect/api/bootstrap")?;
            (response.status == 401 || response.status == 403 || response.ok())
                .then(|| "UniFi Protect".to_string())
        }
        // Version is readable without logging in on a default install.
        "zoneminder" => {
            let response = get("/zm/api/host/getVersion.json")?;
            if !response.ok() {
                return None;
            }
            let parsed = response.json().ok()?;
            let version = parsed.get("version")?.as_str()?;
            Some(format!("ZoneMinder {version}"))
        }
        // authLogin.cgi answers with its XML document root even with no
        // credentials attached.
        "qnap" => {
            let response = get("/cgi-bin/authLogin.cgi")?;
            (response.ok() && response.body.contains("QDocRoot"))
                .then(|| "QNAP QVR".to_string())
        }
        // GetSystemDateAndTime is the one call ONVIF defines as needing no
        // credentials, which makes it the only probe here that asks the device
        // what it is rather than inferring it from a refusal. A device that
        // answers this is an ONVIF device.
        "onvif" => {
            let response = http::request(
                agent,
                "POST",
                &format!("{base}{}", onvif::DEVICE_SERVICE),
                Some(onvif::PROBE_ENVELOPE),
                &[("Content-Type".to_string(), onvif::SOAP_CONTENT_TYPE.to_string())],
            )
            .ok()?;
            response
                .body
                .contains("GetSystemDateAndTimeResponse")
                .then(|| "ONVIF device".to_string())
        }
        // The API rejects a request carrying no token, but it rejects it in
        // JSON with a numeric rspCode, which nothing else here does.
        _ => {
            let response = get("/cgi-bin/api.cgi?cmd=GetDevInfo")?;
            response
                .body
                .contains("rspCode")
                .then(|| "Reolink".to_string())
        }
    }
}

/// Build the client for a configured device. Nothing above this knows which
/// module it got.
pub fn build(config: &DeviceConfig) -> Box<dyn Vendor> {
    match config.vendor.as_str() {
        "frigate" => Box::new(frigate::Frigate::new(config)),
        "zoneminder" => Box::new(zoneminder::ZoneMinder::new(config)),
        "qnap" => Box::new(qnap::Qnap::new(config)),
        "unifi" => Box::new(unifi::Unifi::new(config)),
        "onvif" => Box::new(onvif::Onvif::new(config)),
        _ => Box::new(reolink::client_for(config)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vendor_has_a_distinct_id_and_a_label() {
        for (i, vendor) in VENDORS.iter().enumerate() {
            assert!(!vendor.id.is_empty() && !vendor.label.is_empty());
            for other in &VENDORS[i + 1..] {
                assert_ne!(vendor.id, other.id, "duplicate vendor id");
            }
        }
    }

    /// A config written before vendors existed has no vendor field, and must
    /// land on Reolink rather than on whatever happens to be first.
    #[test]
    fn an_unknown_vendor_falls_back_to_reolink() {
        assert_eq!(label_for(""), "Reolink");
        assert_eq!(label_for("nonsense"), "Reolink");
        assert_eq!(info_for("").id, DEFAULT_VENDOR);
    }

    /// The ordering is load-bearing and is only an array literal, so it is
    /// pinned here. See [`PROBE_ORDER`].
    #[test]
    fn onvif_is_probed_last_so_it_never_shadows_a_richer_vendor() {
        assert_eq!(
            PROBE_ORDER.last(),
            Some(&"onvif"),
            "a Reolink NVR answers the ONVIF probe too; whichever runs first wins"
        );
        assert!(
            PROBE_ORDER.iter().position(|v| *v == "reolink")
                < PROBE_ORDER.iter().position(|v| *v == "onvif"),
            "Reolink must be tried before the generic fallback"
        );
    }

    /// Every vendor that can be configured must also be reachable by the probe,
    /// or adding one leaves a system that can be picked by hand and never
    /// detected.
    #[test]
    fn every_vendor_is_probed_for() {
        for vendor in VENDORS {
            assert!(
                PROBE_ORDER.contains(&vendor.id),
                "{} is offered in the picker but never probed for",
                vendor.id
            );
        }
        assert_eq!(PROBE_ORDER.len(), VENDORS.len());
    }

    /// Against a real device. Ignored by default; run with the address:
    ///   KESTREL_TEST_HOST=192.0.2.242 cargo test -- --ignored identifies_a_real
    #[test]
    #[ignore]
    fn identifies_a_real_system() {
        let Ok(host) = std::env::var("KESTREL_TEST_HOST") else {
            eprintln!("KESTREL_TEST_HOST not set");
            return;
        };
        let found = detect(&host, 80, false, true);
        println!("{host} -> {found:?}");
        let found = found.expect("something should have answered");
        assert!(!found.detail.is_empty());
    }

    #[test]
    fn headers_are_formatted_for_ffmpeg() {
        let mut source = StreamSource::new("rtsps://host/x");
        assert_eq!(source.header_blob(), None, "no headers means no option");

        source.headers.push(("Cookie".into(), "TOKEN=abc".into()));
        source.headers.push(("X-CSRF-Token".into(), "xyz".into()));
        assert_eq!(
            source.header_blob().unwrap(),
            "Cookie: TOKEN=abc\r\nX-CSRF-Token: xyz\r\n"
        );
    }
}
