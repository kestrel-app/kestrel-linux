//! ONVIF Profile S — the standard everything else answers to.
//!
//! The other four vendors here each speak their own thing. ONVIF is the one
//! interface a camera from almost any manufacturer will answer, which makes it
//! the sensible last resort: a device nothing else recognises is very often
//! still an ONVIF device, and a wall that can show it is worth more than a
//! dialog explaining that it cannot.
//!
//! Three things about the protocol shape this module.
//!
//! **It is SOAP, and the namespace prefixes are not stable.** The same element
//! arrives as `tds:`, `trt:` or `tptz:` depending on which service answered, and
//! devices disagree about which prefix means what. Everything below matches on
//! the *local* name and ignores the prefix entirely.
//!
//! **Authentication is per request, not per session.** There is no token to
//! hold and nothing to log out of: every call carries a WS-Security
//! `UsernameToken` whose digest is `Base64(SHA1(nonce + created + password))`.
//! That makes the client stateless once connected, which is why nothing here
//! needs a mutex except the preset table.
//!
//! **The device's clock is part of the credential.** A digest whose `Created`
//! timestamp is too far from the device's own clock is refused, and cameras keep
//! famously bad time. So the first call is the one request ONVIF serves
//! *without* credentials — `GetSystemDateAndTime` — and every later timestamp is
//! written in the device's clock rather than ours. See [`Onvif::connect`].
//!
//! **Never run against a real ONVIF device.** See `docs/untested.md`.

use base64::Engine;
use roxmltree::{Document, Node};

use crate::api::error::{Error, Result};
use crate::api::http;
use crate::api::models::{Channel, DeviceInfo, StreamType};
use crate::config::DeviceConfig;

use super::{StreamSource, Vendor};

/// Where the device service lives when nobody has moved it.
///
/// The one path that is fixed. Every other service — media, PTZ, imaging —
/// announces its own address through `GetCapabilities`, and devices genuinely
/// put them in different places.
pub const DEVICE_SERVICE: &str = "/onvif/device_service";

// ------------------------------------------------------------------ the SOAP

const ENVELOPE_NS: &str = concat!(
    r#" xmlns:s="http://www.w3.org/2003/05/soap-envelope""#,
    r#" xmlns:tds="http://www.onvif.org/ver10/device/wsdl""#,
    r#" xmlns:trt="http://www.onvif.org/ver10/media/wsdl""#,
    r#" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl""#,
    r#" xmlns:tt="http://www.onvif.org/ver10/schema""#,
);

const WSSE_NS: &str =
    "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd";
const WSU_NS: &str =
    "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd";
const PASSWORD_DIGEST: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest";
const BASE64_BINARY: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary";

/// SOAP 1.2. Sending 1.1's `text/xml` gets a 415 from a conforming device.
pub const SOAP_CONTENT_TYPE: &str = "application/soap+xml; charset=utf-8";

/// The one request ONVIF defines as needing no credentials, ready to send.
///
/// Kept whole rather than built, because the probe in [`super::detect`] runs
/// before there is a client - or a password - to build it with.
pub const PROBE_ENVELOPE: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8"?>"#,
    r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope""#,
    r#" xmlns:tds="http://www.onvif.org/ver10/device/wsdl">"#,
    r#"<s:Body><tds:GetSystemDateAndTime/></s:Body></s:Envelope>"#,
);

/// XML text, with the five characters that would otherwise end the element.
///
/// This matters more than it looks: the username and password go into the
/// envelope as text, and a password containing an ampersand would otherwise
/// produce a document the device rejects as malformed rather than as wrong.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// `Base64(SHA1(nonce + created + password))`, which is the whole of ONVIF
/// authentication.
///
/// The nonce is raw bytes here and Base64 in the envelope — hashing the encoded
/// form instead is the classic way to get a digest every device refuses.
pub(crate) fn password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut buf = Vec::with_capacity(nonce.len() + created.len() + password.len());
    buf.extend_from_slice(nonce);
    buf.extend_from_slice(created.as_bytes());
    buf.extend_from_slice(password.as_bytes());
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &buf);
    base64::engine::general_purpose::STANDARD.encode(digest.as_ref())
}

/// The `<s:Header>` carrying one request's credentials, or nothing at all when
/// no username is configured.
///
/// An ONVIF device may be set to allow anonymous access, and some answer
/// `GetProfiles` without credentials. Sending an empty `UsernameToken` to one of
/// those is worse than sending none: it is a credential, and it fails.
fn security_header(username: &str, password: &str, created: &str, nonce: &[u8]) -> String {
    if username.is_empty() {
        return String::new();
    }
    let encoded_nonce = base64::engine::general_purpose::STANDARD.encode(nonce);
    format!(
        concat!(
            r#"<s:Header><wsse:Security s:mustUnderstand="1" xmlns:wsse="{}" xmlns:wsu="{}">"#,
            r#"<wsse:UsernameToken><wsse:Username>{}</wsse:Username>"#,
            r#"<wsse:Password Type="{}">{}</wsse:Password>"#,
            r#"<wsse:Nonce EncodingType="{}">{}</wsse:Nonce>"#,
            r#"<wsu:Created>{}</wsu:Created>"#,
            r#"</wsse:UsernameToken></wsse:Security></s:Header>"#,
        ),
        WSSE_NS,
        WSU_NS,
        escape(username),
        PASSWORD_DIGEST,
        password_digest(nonce, created, password),
        BASE64_BINARY,
        encoded_nonce,
        escape(created),
    )
}

fn envelope(header: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope{ENVELOPE_NS}>{header}<s:Body>{body}</s:Body></s:Envelope>"#
    )
}

// ------------------------------------------------------------------- reading

/// The first descendant with this local name, whatever namespace it came in.
fn find<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.descendants().find(|n| n.tag_name().name() == name)
}

/// Every descendant with this local name.
fn find_all<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Vec<Node<'a, 'i>> {
    node.descendants()
        .filter(|n| n.tag_name().name() == name)
        .collect()
}

/// The text of the first descendant with this local name, trimmed.
fn text(node: Node, name: &str) -> String {
    find(node, name)
        .and_then(|n| n.text())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A SOAP fault as something worth showing a person.
///
/// Devices put the useful half in different places — SOAP 1.2 uses
/// `Reason/Text`, 1.1 uses `faultstring`, and ONVIF adds its own `Subcode` on
/// top — so all three are tried before giving up.
fn fault(root: Node) -> Option<String> {
    let node = find(root, "Fault")?;
    let reason = text(node, "Text");
    let reason = if reason.is_empty() {
        text(node, "faultstring")
    } else {
        reason
    };
    let detail = find(node, "Subcode")
        .map(|sub| text(sub, "Value"))
        .unwrap_or_default();

    Some(match (reason.is_empty(), detail.is_empty()) {
        (true, true) => "the device refused the request".to_string(),
        (true, false) => detail,
        (false, true) => reason,
        // The subcode is the machine-readable half and often the only one that
        // says which of several things went wrong.
        (false, false) => format!("{reason} ({detail})"),
    })
}

// -------------------------------------------------------------------- shapes

/// One profile, which is ONVIF's word for a stream a camera is prepared to
/// serve.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Profile {
    pub token: String,
    pub name: String,
    /// The video *source* this profile encodes. Two profiles sharing one are two
    /// views of the same lens, which is how a camera's main and sub streams are
    /// told apart from two separate cameras on an NVR.
    pub source: String,
    pub codec: String,
    pub width: u64,
    pub height: u64,
    pub ptz: bool,
}

impl Profile {
    fn pixels(&self) -> u64 {
        self.width * self.height
    }
}

/// One physical camera: the profiles that share a video source, sorted.
struct Camera {
    main: Profile,
    main_uri: String,
    sub_uri: String,
    snapshot_uri: String,
}

/// Read `GetProfilesResponse` into the profiles it lists.
///
/// Everything is optional in practice. A profile with no encoder configuration
/// yet — which is a legitimate state, not a broken device — has no codec and no
/// resolution, and is kept: it still has a token, and a token is all
/// `GetStreamUri` needs.
pub(crate) fn parse_profiles(body: &str) -> Result<Vec<Profile>> {
    let doc = Document::parse(body)
        .map_err(|err| Error::Protocol(format!("profiles: malformed XML ({err})")))?;
    let root = doc.root_element();
    if let Some(reason) = fault(root) {
        return Err(Error::Protocol(format!("profiles: {reason}")));
    }

    Ok(find_all(root, "Profiles")
        .into_iter()
        .filter_map(|node| {
            let token = node.attribute("token").unwrap_or_default().trim();
            if token.is_empty() {
                return None;
            }
            // Scoped rather than searched for from the profile: a profile
            // carries several configurations and more than one of them has a
            // Resolution, so a loose search finds the source's bounds as often
            // as the encoder's picture size.
            let encoder = find(node, "VideoEncoderConfiguration");
            let (codec, width, height) = match encoder {
                Some(encoder) => {
                    let resolution = find(encoder, "Resolution");
                    (
                        text(encoder, "Encoding").to_ascii_lowercase(),
                        resolution
                            .map(|r| text(r, "Width"))
                            .unwrap_or_default()
                            .parse()
                            .unwrap_or(0),
                        resolution
                            .map(|r| text(r, "Height"))
                            .unwrap_or_default()
                            .parse()
                            .unwrap_or(0),
                    )
                }
                None => (String::new(), 0, 0),
            };

            Some(Profile {
                token: token.to_string(),
                name: text(node, "Name"),
                source: find(node, "VideoSourceConfiguration")
                    .map(|c| text(c, "SourceToken"))
                    .unwrap_or_default(),
                codec,
                width,
                height,
                ptz: find(node, "PTZConfiguration").is_some(),
            })
        })
        .collect())
}

/// Group profiles into cameras, biggest picture first.
///
/// ONVIF hands back a flat list and leaves the grouping to the client. Profiles
/// sharing a `SourceToken` are the same lens at different sizes — a camera's
/// main and sub streams — so they become one camera here, and the wall gets one
/// tile with two qualities rather than two tiles showing the same view.
///
/// Ordering is by pixel count rather than by name. `Name` is whatever somebody
/// typed into the camera years ago: "mainStream", "Profile_1", "" and
/// "MediaProfile000" are all real, and none of them can be sorted on.
///
/// A profile with no source token is its own camera. That is the honest reading
/// — nothing says it shares a lens with anything — and it keeps a device that
/// omits the field from collapsing all of its cameras into one tile.
pub(crate) fn group_profiles(profiles: Vec<Profile>) -> Vec<Vec<Profile>> {
    let mut groups: Vec<Vec<Profile>> = Vec::new();
    let mut order: Vec<String> = Vec::new();

    for profile in profiles {
        let key = if profile.source.is_empty() {
            format!("\0{}", profile.token)
        } else {
            profile.source.clone()
        };
        match order.iter().position(|k| *k == key) {
            Some(at) => groups[at].push(profile),
            None => {
                order.push(key);
                groups.push(vec![profile]);
            }
        }
    }

    for group in &mut groups {
        // Stable, so two profiles at the same size keep the order the device
        // listed them in rather than an arbitrary one.
        group.sort_by(|a, b| b.pixels().cmp(&a.pixels()));
    }
    groups
}

/// Rewrite a URI the device handed back to point at the address that is known
/// to work.
///
/// A device reports the address *it* believes it has, which is right on a flat
/// network and wrong through a port forward, a VLAN, or any NAT — and it is
/// wrong in the one direction that matters, because the user reached it at the
/// address they typed. So the host and port come from the configuration and the
/// path comes from the device, which is the half only the device knows.
///
/// Credentials are injected because ffmpeg has nowhere else to take them from;
/// any the device put there itself are replaced rather than appended.
pub(crate) fn retarget(uri: &str, host: &str, port: u16, username: &str, password: &str) -> String {
    let (scheme, rest) = match uri.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("rtsp", uri),
    };
    // Anything before an @ is the device's own idea of the credentials.
    let rest = rest.rsplit_once('@').map(|(_, tail)| tail).unwrap_or(rest);
    let path = match rest.find(['/', '?']) {
        Some(at) => &rest[at..],
        None => "",
    };

    let credentials = if username.is_empty() {
        String::new()
    } else {
        format!(
            "{}:{}@",
            http::encode(username),
            http::encode(password)
        )
    };
    format!("{scheme}://{credentials}{host}:{port}{path}")
}

// -------------------------------------------------------------------- client

pub struct Onvif {
    base: String,
    host: String,
    rtsp_port: u16,
    username: String,
    password: String,
    agent: ureq::Agent,
    /// Seconds to add to this machine's clock to land on the device's.
    skew: i64,
    media_url: String,
    ptz_url: String,
    channels: Vec<Channel>,
    cameras: Vec<Camera>,
    info: DeviceInfo,
    /// The ONVIF preset token behind each number handed to the UI.
    ///
    /// Presets are identified by string here and by `i64` in the trait, and the
    /// strings are not always numbers — "Preset001" and a GUID are both real. So
    /// the numbers given out are remembered against the tokens they stand for.
    /// The one piece of state that outlives a call, hence the one mutex.
    presets: std::sync::Mutex<std::collections::HashMap<(u32, i64), String>>,
}

impl Onvif {
    pub fn new(config: &DeviceConfig) -> Self {
        Onvif {
            base: http::base_url(&config.host, config.port, config.https),
            host: config.host.clone(),
            rtsp_port: config.rtsp_port,
            username: config.username.clone(),
            password: config.password.clone(),
            agent: http::agent(20, config.allow_self_signed),
            skew: 0,
            media_url: String::new(),
            ptz_url: String::new(),
            channels: Vec::new(),
            cameras: Vec::new(),
            info: DeviceInfo::default(),
            presets: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Now, in the device's clock, as ONVIF wants it written.
    fn created(&self) -> String {
        let now = chrono::Utc::now() + chrono::Duration::seconds(self.skew);
        now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }

    /// One SOAP call. `authenticated` is false only for the clock read that has
    /// to happen before a credential can be built.
    fn call(&self, url: &str, what: &str, body: &str, authenticated: bool) -> Result<String> {
        let header = if authenticated {
            let mut nonce = [0u8; 16];
            // A predictable nonce is a replayable credential. Failing closed
            // here rather than falling back to something weaker.
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
                .map_err(|_| Error::connection("no source of randomness for the credential"))?;
            security_header(&self.username, &self.password, &self.created(), &nonce)
        } else {
            String::new()
        };

        let response = http::request(
            &self.agent,
            "POST",
            url,
            Some(&envelope(&header, body)),
            &[("Content-Type".to_string(), SOAP_CONTENT_TYPE.to_string())],
        )?;

        // A fault arrives with a 400 or a 500 as often as a 200, and the body is
        // the half worth reading either way, so it is parsed before the status
        // is judged.
        if let Ok(doc) = Document::parse(&response.body) {
            if let Some(reason) = fault(doc.root_element()) {
                let lowered = reason.to_ascii_lowercase();
                if lowered.contains("auth")
                    || lowered.contains("password")
                    || lowered.contains("credential")
                    || lowered.contains("subscriber")
                {
                    return Err(Error::auth(format!("{what}: {reason}")));
                }
                return Err(Error::Protocol(format!("{what}: {reason}")));
            }
        }
        if !response.ok() {
            return Err(http::describe(&response, what));
        }
        Ok(response.body)
    }

    fn device_url(&self) -> String {
        format!("{}{}", self.base, DEVICE_SERVICE)
    }

    /// Read the device's clock, which is the only call that needs no
    /// credentials, and keep the difference from ours.
    ///
    /// Not an error when it fails. A device that will not say what time it is
    /// may still accept a digest written in ours, and refusing to connect over
    /// it would turn a device that mostly works into one that does not work at
    /// all.
    fn learn_the_clock(&mut self) {
        let Ok(body) = self.call(
            &self.device_url(),
            "system time",
            "<tds:GetSystemDateAndTime/>",
            false,
        ) else {
            return;
        };
        let Ok(doc) = Document::parse(&body) else { return };
        let root = doc.root_element();
        // UTCDateTime is the one that is defined to be UTC; LocalDateTime is the
        // same instant in the device's timezone and is not usable without also
        // reading the zone, which devices report inconsistently.
        let Some(utc) = find(root, "UTCDateTime") else { return };
        let (Some(date), Some(time)) = (find(utc, "Date"), find(utc, "Time")) else {
            return;
        };

        let number = |node: Node, name: &str| text(node, name).parse::<u32>().ok();
        let (Some(year), Some(month), Some(day)) = (
            text(date, "Year").parse::<i32>().ok(),
            number(date, "Month"),
            number(date, "Day"),
        ) else {
            return;
        };
        let Some(stamp) = chrono::NaiveDate::from_ymd_opt(year, month, day).and_then(|d| {
            d.and_hms_opt(
                number(time, "Hour").unwrap_or(0),
                number(time, "Minute").unwrap_or(0),
                number(time, "Second").unwrap_or(0),
            )
        }) else {
            return;
        };

        self.skew = stamp.and_utc().timestamp() - chrono::Utc::now().timestamp();
        if self.skew.abs() > 60 {
            log::info!(
                "onvif: device clock is {}s from this one; timestamps will follow the device",
                self.skew
            );
        }
    }

    /// Where the media and PTZ services actually live.
    ///
    /// Falls back to the conventional paths rather than failing: plenty of
    /// devices serve every service from one endpoint, and some report
    /// capabilities badly while answering the calls perfectly well.
    fn learn_the_services(&mut self) {
        let body = self.call(
            &self.device_url(),
            "capabilities",
            "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>",
            true,
        );
        let mut media = String::new();
        let mut ptz = String::new();
        if let Ok(body) = &body {
            if let Ok(doc) = Document::parse(body) {
                let root = doc.root_element();
                media = find(root, "Media").map(|n| text(n, "XAddr")).unwrap_or_default();
                ptz = find(root, "PTZ").map(|n| text(n, "XAddr")).unwrap_or_default();
            }
        }

        self.media_url = if media.is_empty() {
            format!("{}/onvif/media_service", self.base)
        } else {
            self.same_host(&media)
        };
        self.ptz_url = if ptz.is_empty() {
            String::new()
        } else {
            self.same_host(&ptz)
        };
    }

    /// A service address the device gave us, pointed back at the address we
    /// reached it on. Same reasoning as [`retarget`], without the credentials.
    fn same_host(&self, url: &str) -> String {
        let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
        let path = match rest.find(['/', '?']) {
            Some(at) => &rest[at..],
            None => "",
        };
        format!("{}{}", self.base, path)
    }

    fn stream_uri(&self, token: &str) -> Result<String> {
        let body = format!(
            concat!(
                "<trt:GetStreamUri><trt:StreamSetup>",
                "<tt:Stream>RTP-Unicast</tt:Stream>",
                "<tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport>",
                "</trt:StreamSetup><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>",
            ),
            escape(token)
        );
        let body = self.call(&self.media_url, "stream address", &body, true)?;
        let doc = Document::parse(&body)
            .map_err(|err| Error::Protocol(format!("stream address: malformed XML ({err})")))?;
        let uri = text(doc.root_element(), "Uri");
        if uri.is_empty() {
            return Err(Error::Protocol(
                "stream address: the device returned no URI".into(),
            ));
        }
        Ok(uri)
    }

    /// The snapshot address, which many devices simply do not have.
    ///
    /// Absence is not an error: `snapshot` reports it when asked, and the rest
    /// of the wall is unaffected.
    fn snapshot_uri(&self, token: &str) -> String {
        let body = format!(
            "<trt:GetSnapshotUri><trt:ProfileToken>{}</trt:ProfileToken></trt:GetSnapshotUri>",
            escape(token)
        );
        match self.call(&self.media_url, "snapshot address", &body, true) {
            Ok(body) => Document::parse(&body)
                .map(|doc| text(doc.root_element(), "Uri"))
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    fn camera(&self, channel: &Channel) -> Result<&Camera> {
        self.cameras.get(channel.index as usize).ok_or_else(|| {
            Error::Protocol(format!("no such camera: channel {}", channel.index))
        })
    }

    /// The profile token PTZ commands for a channel are addressed to.
    fn ptz_token(&self, channel: u32) -> Result<&str> {
        if self.ptz_url.is_empty() {
            return Err(Error::Unsupported(
                "this device reports no PTZ service".into(),
            ));
        }
        let camera = self
            .cameras
            .get(channel as usize)
            .ok_or_else(|| Error::Protocol(format!("no such camera: channel {channel}")))?;
        if !camera.main.ptz {
            return Err(Error::Unsupported(
                "this camera has no PTZ configuration".into(),
            ));
        }
        Ok(&camera.main.token)
    }
}

/// A UI speed (1..=64) as an ONVIF velocity (0.0..=1.0).
pub(crate) fn velocity(speed: i64) -> f32 {
    (speed.clamp(1, 64) as f32 / 64.0).clamp(0.05, 1.0)
}

/// Which way a direction pushes pan and tilt, as ONVIF's normalised axes.
///
/// `None` is a direction ONVIF's PTZ service has no word for. Focus is the
/// notable one: it belongs to the imaging service, which is a different
/// endpoint and a different call, so it is refused rather than silently
/// swallowed.
pub(crate) fn axes(direction: &str) -> Option<(f32, f32, f32)> {
    Some(match direction {
        "up" => (0.0, 1.0, 0.0),
        "down" => (0.0, -1.0, 0.0),
        "left" => (-1.0, 0.0, 0.0),
        "right" => (1.0, 0.0, 0.0),
        "leftup" => (-1.0, 1.0, 0.0),
        "leftdown" => (-1.0, -1.0, 0.0),
        "rightup" => (1.0, 1.0, 0.0),
        "rightdown" => (1.0, -1.0, 0.0),
        "zoom_in" => (0.0, 0.0, 1.0),
        "zoom_out" => (0.0, 0.0, -1.0),
        _ => return None,
    })
}

impl Vendor for Onvif {
    fn vendor_id(&self) -> &'static str {
        "onvif"
    }

    fn connect(&mut self) -> Result<DeviceInfo> {
        // Before anything that needs a credential: the digest is only valid
        // against the device's clock.
        self.learn_the_clock();

        let body = self.call(
            &self.device_url(),
            "device information",
            "<tds:GetDeviceInformation/>",
            true,
        )?;
        let doc = Document::parse(&body)
            .map_err(|err| Error::Protocol(format!("device information: malformed XML ({err})")))?;
        let root = doc.root_element();
        let manufacturer = text(root, "Manufacturer");
        let model = text(root, "Model");

        self.learn_the_services();

        let profiles = parse_profiles(&self.call(
            &self.media_url,
            "profiles",
            "<trt:GetProfiles/>",
            true,
        )?)?;
        if profiles.is_empty() {
            return Err(Error::Protocol(
                "the device lists no media profiles, so there is nothing to show".into(),
            ));
        }

        for (index, group) in group_profiles(profiles).into_iter().enumerate() {
            let mut group = group.into_iter();
            let Some(main) = group.next() else { continue };
            let sub = group.next();

            // One round trip per stream, done once here rather than on every
            // tile: the URI is stable for the life of the profile, and
            // `stream` has to answer without blocking the wall.
            let main_uri = self.stream_uri(&main.token)?;
            let sub_uri = match &sub {
                Some(sub) => self.stream_uri(&sub.token).unwrap_or_default(),
                None => String::new(),
            };
            let snapshot_uri = self.snapshot_uri(&main.token);

            let mut channel = Channel::new(index as u32);
            channel.key = main.token.clone();
            channel.name = if main.name.is_empty() {
                format!("Camera {}", index + 1)
            } else {
                main.name.clone()
            };
            channel.model = model.clone();
            if !main.codec.is_empty() {
                channel.main_codec = main.codec.clone();
            }
            if let Some(sub) = &sub {
                if !sub.codec.is_empty() {
                    channel.sub_codec = sub.codec.clone();
                }
            }
            // Reported, never assumed — the house rule for this trait. A device
            // with no PTZ service gets no PTZ pane at all.
            channel.ptz_supported = main.ptz && !self.ptz_url.is_empty();
            channel.ptz_presets_supported = channel.ptz_supported;
            channel.zoom_supported = channel.ptz_supported;
            channel.optical_zoom_supported = channel.ptz_supported;

            self.channels.push(channel);
            self.cameras.push(Camera {
                main,
                main_uri,
                sub_uri,
                snapshot_uri,
            });
        }

        let name = match (manufacturer.is_empty(), model.is_empty()) {
            (true, true) => "ONVIF device".to_string(),
            (true, false) => model.clone(),
            (false, true) => manufacturer.clone(),
            (false, false) => format!("{manufacturer} {model}"),
        };
        self.info = DeviceInfo {
            name,
            model,
            firmware: text(root, "FirmwareVersion"),
            serial: text(root, "SerialNumber"),
            channel_count: self.channels.len(),
            hdd_count: 0,
            build_day: String::new(),
        };
        Ok(self.info.clone())
    }

    /// Nothing to release. Every request carried its own credential, so there
    /// is no session on the device to end.
    fn logout(&self) {}

    fn channels(&self) -> &[Channel] {
        &self.channels
    }

    fn stream(&self, channel: &Channel, stream: StreamType) -> Result<StreamSource> {
        let camera = self.camera(channel)?;
        // A camera with one profile has no sub stream. Falling back to the main
        // one beats refusing: the wall asked for a picture, and a bigger picture
        // is still a picture.
        let uri = match stream {
            StreamType::Sub if !camera.sub_uri.is_empty() => &camera.sub_uri,
            _ => &camera.main_uri,
        };
        Ok(StreamSource::new(retarget(
            uri,
            &self.host,
            self.rtsp_port,
            &self.username,
            &self.password,
        )))
    }

    fn snapshot(&self, channel: &Channel) -> Result<Vec<u8>> {
        let camera = self.camera(channel)?;
        if camera.snapshot_uri.is_empty() {
            return Err(Error::Unsupported(
                "this camera does not offer a snapshot address".into(),
            ));
        }
        let url = self.same_host(&camera.snapshot_uri);

        // Basic, not digest. The snapshot is plain HTTP rather than SOAP, so
        // WS-Security does not apply to it, and a device that insists on digest
        // will refuse this - which is said plainly below rather than returned as
        // an empty picture. See docs/untested.md.
        let mut headers = Vec::new();
        if !self.username.is_empty() {
            let credentials = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", self.username, self.password));
            headers.push(("Authorization".to_string(), format!("Basic {credentials}")));
        }

        let (status, bytes) = http::request_bytes(&self.agent, &url, &headers, 16 * 1024 * 1024)?;
        if !(200..300).contains(&status) {
            return Err(match status {
                401 | 403 => Error::auth(
                    "snapshot: the camera refused HTTP Basic — it may require digest authentication"
                        .to_string(),
                ),
                status => Error::Protocol(format!("snapshot: HTTP {status}")),
            });
        }
        Ok(bytes)
    }

    // ------------------------------------------------------------------- PTZ

    fn ptz_move(&self, channel: u32, direction: &str, speed: i64) -> Result<()> {
        let token = self.ptz_token(channel)?;
        let Some((x, y, zoom)) = axes(direction) else {
            return Err(Error::Unsupported(format!(
                "{direction} is not something ONVIF's PTZ service does"
            )));
        };
        let v = velocity(speed);
        let body = format!(
            concat!(
                "<tptz:ContinuousMove><tptz:ProfileToken>{}</tptz:ProfileToken>",
                r#"<tptz:Velocity><tt:PanTilt x="{}" y="{}"/><tt:Zoom x="{}"/></tptz:Velocity>"#,
                "</tptz:ContinuousMove>",
            ),
            escape(token),
            x * v,
            y * v,
            zoom * v,
        );
        self.call(&self.ptz_url, "PTZ", &body, true)?;
        Ok(())
    }

    fn ptz_stop(&self, channel: u32) -> Result<()> {
        let token = self.ptz_token(channel)?;
        let body = format!(
            concat!(
                "<tptz:Stop><tptz:ProfileToken>{}</tptz:ProfileToken>",
                "<tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>true</tptz:Zoom></tptz:Stop>",
            ),
            escape(token)
        );
        self.call(&self.ptz_url, "PTZ stop", &body, true)?;
        Ok(())
    }

    fn ptz_presets(&self, channel: u32) -> Result<Vec<(i64, String)>> {
        let token = self.ptz_token(channel)?;
        let body = format!(
            "<tptz:GetPresets><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetPresets>",
            escape(token)
        );
        let body = self.call(&self.ptz_url, "presets", &body, true)?;
        let doc = Document::parse(&body)
            .map_err(|err| Error::Protocol(format!("presets: malformed XML ({err})")))?;

        let mut out = Vec::new();
        let mut remembered = self.presets.lock().unwrap();
        for (position, node) in find_all(doc.root_element(), "Preset").into_iter().enumerate() {
            let Some(preset) = node.attribute("token") else { continue };
            // A numeric token is used as its own number so the UI shows what the
            // camera calls it. Anything else gets a position, and the mapping is
            // kept because `ptz_goto_preset` only receives the number back.
            let id = preset
                .parse::<i64>()
                .unwrap_or_else(|_| position as i64 + 1);
            let name = text(node, "Name");
            let name = if name.is_empty() {
                format!("Preset {id}")
            } else {
                name
            };
            remembered.insert((channel, id), preset.to_string());
            out.push((id, name));
        }
        Ok(out)
    }

    fn ptz_goto_preset(&self, channel: u32, preset: i64, speed: i64) -> Result<()> {
        let token = self.ptz_token(channel)?;
        // The token this number stood for, or the number itself for a camera
        // whose tokens were numeric all along.
        let target = self
            .presets
            .lock()
            .unwrap()
            .get(&(channel, preset))
            .cloned()
            .unwrap_or_else(|| preset.to_string());
        let v = velocity(speed);
        let body = format!(
            concat!(
                "<tptz:GotoPreset><tptz:ProfileToken>{}</tptz:ProfileToken>",
                "<tptz:PresetToken>{}</tptz:PresetToken>",
                r#"<tptz:Speed><tt:PanTilt x="{}" y="{}"/><tt:Zoom x="{}"/></tptz:Speed>"#,
                "</tptz:GotoPreset>",
            ),
            escape(token),
            escape(&target),
            v,
            v,
            v,
        );
        self.call(&self.ptz_url, "preset", &body, true)?;
        Ok(())
    }

    fn ptz_go_home(&self, channel: u32) -> Result<()> {
        let token = self.ptz_token(channel)?;
        let v = velocity(32);
        let body = format!(
            concat!(
                "<tptz:GotoHomePosition><tptz:ProfileToken>{}</tptz:ProfileToken>",
                r#"<tptz:Speed><tt:PanTilt x="{}" y="{}"/><tt:Zoom x="{}"/></tptz:Speed>"#,
                "</tptz:GotoHomePosition>",
            ),
            escape(token),
            v,
            v,
            v,
        );
        self.call(&self.ptz_url, "home", &body, true)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole of ONVIF authentication, checked against a digest computed
    /// outside this code entirely:
    ///
    /// ```text
    /// python3 -c "import hashlib,base64; print(base64.b64encode(
    ///   hashlib.sha1(bytes(range(16))
    ///     + b'2026-08-31T03:00:00.000Z' + b's3cr3t').digest()).decode())"
    /// ```
    ///
    /// Worth pinning rather than trusting: the two ways to get this wrong -
    /// hashing the Base64 nonce instead of its bytes, or ordering the three
    /// parts differently - both produce a perfectly well-formed digest that
    /// every device refuses, and the failure looks exactly like a wrong
    /// password.
    #[test]
    fn the_password_digest_matches_an_independent_one() {
        let nonce: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            password_digest(&nonce, "2026-08-31T03:00:00.000Z", "s3cr3t"),
            "c2xF3/jE6HZECy3Fa8dqn2yVeCA="
        );
    }

    /// A different nonce has to give a different digest, or the nonce is not
    /// reaching the hash at all — which the test above would not catch.
    #[test]
    fn the_nonce_reaches_the_digest() {
        let created = "2026-08-31T03:00:00.000Z";
        let one = password_digest(&[1, 2, 3], created, "s3cr3t");
        let two = password_digest(&[3, 2, 1], created, "s3cr3t");
        assert_ne!(one, two);
        assert_ne!(one, password_digest(&[1, 2, 3], created, "other"));
        assert_ne!(one, password_digest(&[1, 2, 3], "2026-08-31T04:00:00Z", "s3cr3t"));
    }

    /// No username means anonymous access, which is a real ONVIF configuration.
    /// An empty credential is not the same thing and would be refused.
    #[test]
    fn no_username_sends_no_credential() {
        assert_eq!(security_header("", "", "now", &[1, 2, 3]), "");
        assert!(security_header("admin", "p", "now", &[1, 2, 3]).contains("UsernameToken"));
    }

    /// A password is user input that goes straight into an XML document.
    #[test]
    fn credentials_are_escaped_into_the_envelope() {
        let header = security_header("ad&min", "p<>w", "now", &[1]);
        assert!(header.contains("ad&amp;min"), "{header}");
        assert!(!header.contains("ad&min"), "raw ampersand reached the document");
        assert_eq!(escape(r#"a&b<c>d"e'f"#), "a&amp;b&lt;c&gt;d&quot;e&apos;f");
    }

    fn profiles_response() -> &'static str {
        // Shaped like a real two-camera NVR reply: prefixes that differ from the
        // request's, a main and a sub profile per source, one PTZ camera, and a
        // Bounds element carrying a Resolution-shaped decoy.
        r#"<?xml version="1.0"?>
        <env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"
                      xmlns:trt="http://www.onvif.org/ver10/media/wsdl"
                      xmlns:tt="http://www.onvif.org/ver10/schema">
          <env:Body><trt:GetProfilesResponse>
            <trt:Profiles token="Profile_1" fixed="true">
              <tt:Name>Front Door</tt:Name>
              <tt:VideoSourceConfiguration token="VSC_1">
                <tt:SourceToken>VideoSource_1</tt:SourceToken>
                <tt:Bounds x="0" y="0" width="3840" height="2160"/>
              </tt:VideoSourceConfiguration>
              <tt:VideoEncoderConfiguration token="VEC_1">
                <tt:Encoding>H265</tt:Encoding>
                <tt:Resolution><tt:Width>2560</tt:Width><tt:Height>1440</tt:Height></tt:Resolution>
              </tt:VideoEncoderConfiguration>
              <tt:PTZConfiguration token="PTZ_1"><tt:Name>ptz</tt:Name></tt:PTZConfiguration>
            </trt:Profiles>
            <trt:Profiles token="Profile_1_sub">
              <tt:Name>Front Door Sub</tt:Name>
              <tt:VideoSourceConfiguration token="VSC_1">
                <tt:SourceToken>VideoSource_1</tt:SourceToken>
              </tt:VideoSourceConfiguration>
              <tt:VideoEncoderConfiguration token="VEC_2">
                <tt:Encoding>H264</tt:Encoding>
                <tt:Resolution><tt:Width>640</tt:Width><tt:Height>360</tt:Height></tt:Resolution>
              </tt:VideoEncoderConfiguration>
            </trt:Profiles>
            <trt:Profiles token="Profile_2">
              <tt:Name>Drive</tt:Name>
              <tt:VideoSourceConfiguration token="VSC_2">
                <tt:SourceToken>VideoSource_2</tt:SourceToken>
              </tt:VideoSourceConfiguration>
              <tt:VideoEncoderConfiguration token="VEC_3">
                <tt:Encoding>H264</tt:Encoding>
                <tt:Resolution><tt:Width>1920</tt:Width><tt:Height>1080</tt:Height></tt:Resolution>
              </tt:VideoEncoderConfiguration>
            </trt:Profiles>
          </trt:GetProfilesResponse></env:Body>
        </env:Envelope>"#
    }

    #[test]
    fn profiles_are_read_whatever_the_prefixes_are() {
        let profiles = parse_profiles(profiles_response()).unwrap();
        assert_eq!(profiles.len(), 3);

        let front = &profiles[0];
        assert_eq!(front.token, "Profile_1");
        assert_eq!(front.name, "Front Door");
        assert_eq!(front.source, "VideoSource_1");
        assert_eq!(front.codec, "h265");
        assert!(front.ptz, "the PTZ configuration should have been noticed");

        // The picture size, not the sensor's bounds: Bounds says 3840x2160 and
        // is the first Resolution-shaped thing in the profile.
        assert_eq!((front.width, front.height), (2560, 1440));
        assert!(!profiles[1].ptz, "the sub profile has no PTZ configuration");
    }

    /// Two profiles on one lens are one camera; a second lens is a second one.
    #[test]
    fn profiles_sharing_a_lens_become_one_camera() {
        let groups = group_profiles(parse_profiles(profiles_response()).unwrap());
        assert_eq!(groups.len(), 2, "two video sources, so two cameras");

        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[0][0].token, "Profile_1", "the bigger picture leads");
        assert_eq!(groups[0][1].token, "Profile_1_sub");

        assert_eq!(groups[1].len(), 1, "the second camera has no sub stream");
        assert_eq!(groups[1][0].token, "Profile_2");
    }

    /// Sorted by pixel count, not by the order the device listed them or by a
    /// name that cannot be relied on.
    #[test]
    fn the_main_stream_is_the_biggest_one() {
        let small = Profile {
            token: "small".into(),
            source: "one".into(),
            width: 640,
            height: 360,
            ..Profile::default()
        };
        let big = Profile {
            token: "big".into(),
            source: "one".into(),
            width: 1920,
            height: 1080,
            ..Profile::default()
        };
        // Listed smallest first, which is how plenty of devices order them.
        let groups = group_profiles(vec![small, big]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0][0].token, "big");
    }

    /// A device that omits SourceToken must not collapse into a single tile
    /// showing one camera where there were six.
    #[test]
    fn profiles_without_a_source_stay_separate() {
        let bare = |token: &str| Profile {
            token: token.into(),
            ..Profile::default()
        };
        let groups = group_profiles(vec![bare("a"), bare("b"), bare("c")]);
        assert_eq!(groups.len(), 3, "no shared lens is not the same as one lens");
    }

    #[test]
    fn a_fault_is_read_out_of_the_reply() {
        let body = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
          <s:Body><s:Fault>
            <s:Code><s:Value>s:Sender</s:Value>
              <s:Subcode><s:Value>ter:NotAuthorized</s:Value></s:Subcode></s:Code>
            <s:Reason><s:Text xml:lang="en">Sender not Authorized</s:Text></s:Reason>
          </s:Fault></s:Body></s:Envelope>"#;
        let doc = Document::parse(body).unwrap();
        let reason = fault(doc.root_element()).unwrap();
        assert!(reason.contains("Sender not Authorized"), "{reason}");
        assert!(reason.contains("ter:NotAuthorized"), "{reason}");

        let fine = Document::parse("<a><b/></a>").unwrap();
        assert_eq!(fault(fine.root_element()), None);
    }

    /// The device's own address is not necessarily one that works from here.
    #[test]
    fn a_stream_address_is_pointed_back_at_the_configured_host() {
        // The device answers with the LAN address it believes it has.
        assert_eq!(
            retarget(
                "rtsp://192.0.2.9:554/Streaming/Channels/101",
                "camera.lan",
                8554,
                "admin",
                "p@ss"
            ),
            "rtsp://admin:p%40ss@camera.lan:8554/Streaming/Channels/101"
        );
        // Credentials the device put there itself are replaced, not stacked.
        assert_eq!(
            retarget("rtsp://old:creds@192.0.2.9:554/live", "host", 554, "admin", "x"),
            "rtsp://admin:x@host:554/live"
        );
        // No credentials configured means none in the URL.
        assert_eq!(
            retarget("rtsp://192.0.2.9:554/live?x=1", "host", 554, "", ""),
            "rtsp://host:554/live?x=1"
        );
        // A path-less URI is still a URI.
        assert_eq!(
            retarget("rtsp://192.0.2.9:554", "host", 554, "", ""),
            "rtsp://host:554"
        );
    }

    #[test]
    fn ptz_directions_map_onto_onvif_axes() {
        assert_eq!(axes("left"), Some((-1.0, 0.0, 0.0)));
        assert_eq!(axes("rightup"), Some((1.0, 1.0, 0.0)));
        assert_eq!(axes("zoom_out"), Some((0.0, 0.0, -1.0)));
        // Focus is the imaging service, not the PTZ one, and is refused rather
        // than quietly dropped.
        assert_eq!(axes("focus_near"), None);
        assert_eq!(axes("nonsense"), None);
    }

    #[test]
    fn ptz_speed_becomes_a_normalised_velocity() {
        assert!((velocity(64) - 1.0).abs() < f32::EPSILON);
        assert!((velocity(32) - 0.5).abs() < f32::EPSILON);
        // Never zero: a move at velocity 0 is a move that does nothing, and the
        // slider goes down to 1.
        assert!(velocity(1) >= 0.05);
        assert!(velocity(-5) >= 0.05, "out of range must not invert the axis");
        assert!((velocity(9999) - 1.0).abs() < f32::EPSILON);
    }

    /// Against a real device, or the stub in `tools/`. Ignored by default:
    ///   KESTREL_TEST_ONVIF=192.0.2.77:80 KESTREL_TEST_ONVIF_USER=admin \
    ///   KESTREL_TEST_ONVIF_PASS=s3cr3t cargo test -- --ignored talks_to_a_real
    #[test]
    #[ignore]
    fn talks_to_a_real_onvif_device() {
        let Ok(target) = std::env::var("KESTREL_TEST_ONVIF") else {
            eprintln!("KESTREL_TEST_ONVIF not set");
            return;
        };
        let (host, port) = target
            .split_once(':')
            .map(|(h, p)| (h.to_string(), p.parse().unwrap_or(80)))
            .unwrap_or((target.clone(), 80));

        let config = DeviceConfig {
            vendor: "onvif".into(),
            host,
            port,
            rtsp_port: 554,
            username: std::env::var("KESTREL_TEST_ONVIF_USER").unwrap_or_else(|_| "admin".into()),
            password: std::env::var("KESTREL_TEST_ONVIF_PASS").unwrap_or_default(),
            ..DeviceConfig::default()
        };

        let mut onvif = Onvif::new(&config);
        let info = onvif.connect().expect("the device should answer");
        println!("  {} / {} fw {}", info.name, info.model, info.firmware);
        println!("  clock skew: {}s", onvif.skew);
        assert!(info.channel_count > 0, "a device with no cameras is no use");

        for channel in onvif.channels() {
            let main = onvif.stream(channel, StreamType::Main).unwrap();
            let sub = onvif.stream(channel, StreamType::Sub).unwrap();
            println!(
                "  [{}] {:20} {:>4} ptz={} \n        main {}\n        sub  {}",
                channel.index, channel.name, channel.main_codec, channel.ptz_supported,
                crate::api::error::redact_rtsp(&main.url),
                crate::api::error::redact_rtsp(&sub.url),
            );
            assert!(main.url.starts_with("rtsp://"), "{}", main.url);
        }
    }

    /// Malformed XML is a protocol error rather than a panic. Devices under
    /// load do truncate replies.
    #[test]
    fn a_truncated_reply_is_an_error_not_a_panic() {
        assert!(parse_profiles("<trt:GetProfilesResponse><trt:Profiles tok").is_err());
        assert!(parse_profiles("").is_err());
    }
}
