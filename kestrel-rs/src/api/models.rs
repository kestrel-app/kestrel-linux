//! Typed views over the JSON the Reolink CGI API returns.
//!
//! The device speaks loosely-typed JSON that varies between firmware versions
//! and between NVRs and standalone cameras: fields appear and vanish, numbers
//! arrive as strings, and the same key is sometimes an object and sometimes an
//! array. Everything crossing into the rest of the app is funnelled through
//! these types so no widget ever touches a raw value.

use chrono::{Datelike, NaiveDateTime, Timelike};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    Main,
    Sub,
}

impl StreamType {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamType::Main => "main",
            StreamType::Sub => "sub",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Camera,
    Nvr,
}

/// Which lens of a dual-lens camera a channel represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lens {
    None,
    Wide,
    Tele,
}

/// Models whose two channels are two lenses on one camera rather than two
/// separate cameras. Deliberately excludes the Duo series: those are twin *wide*
/// lenses stitched into a panorama, so there is no telephoto to zoom into.
const DUAL_LENS_MODEL_HINTS: &[&str] = &["trackmix"];

pub fn is_dual_lens_model(model: &str) -> bool {
    let flat: String = model
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    DUAL_LENS_MODEL_HINTS.iter().any(|hint| flat.contains(hint))
}

// ---------------------------------------------------------------- coercion

/// Coerce a device field to i64. Sizes arrive as strings on some firmware.
pub fn as_i64(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Bool(b)) => *b as i64,
        _ => 0,
    }
}

pub fn as_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Reolink reports times as {year, mon, day, hour, min, sec}.
pub fn parse_device_time(raw: &Value) -> Option<NaiveDateTime> {
    let get = |key: &str| -> Option<i64> {
        let v = raw.get(key)?;
        match v {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    };
    chrono::NaiveDate::from_ymd_opt(get("year")? as i32, get("mon")? as u32, get("day")? as u32)?
        .and_hms_opt(
            get("hour").unwrap_or(0) as u32,
            get("min").unwrap_or(0) as u32,
            get("sec").unwrap_or(0) as u32,
        )
}

pub fn to_device_time(when: &NaiveDateTime) -> Value {
    serde_json::json!({
        "year": when.year(),
        "mon": when.month(),
        "day": when.day(),
        "hour": when.hour(),
        "min": when.minute(),
        "sec": when.second(),
    })
}

// ---------------------------------------------------------------- device

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub name: String,
    pub model: String,
    pub firmware: String,
    pub serial: String,
    pub channel_count: usize,
    pub hdd_count: usize,
    pub build_day: String,
}

impl DeviceInfo {
    pub fn kind(&self) -> DeviceKind {
        if self.channel_count > 1 {
            DeviceKind::Nvr
        } else {
            DeviceKind::Camera
        }
    }

    pub fn parse(value: &Value) -> Self {
        let info = value.get("DevInfo").unwrap_or(value);
        let count = as_i64(info.get("channelNum")).max(1) as usize;
        DeviceInfo {
            name: as_string(info.get("name")),
            model: as_string(info.get("model")),
            firmware: as_string(info.get("firmVer")),
            serial: as_string(info.get("serial")),
            channel_count: count,
            hdd_count: as_i64(info.get("hddNum")).max(0) as usize,
            build_day: as_string(info.get("buildDay")),
        }
    }
}

// ---------------------------------------------------------------- channel

/// One cell's worth of camera: a device's channel, and which view of it.
///
/// `view` is `None` for the camera itself and `Some(id)` for one of the virtual
/// cameras cropped out of it. Streams, the warm pool and the event poller key
/// on [`SourceId::stream_key`] instead, because a virtual camera adds no
/// connection and no detection of its own — it is a different rectangle out of
/// the same picture, and everything that talks to the device is asking about
/// the picture.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId {
    pub device: String,
    pub channel: u32,
    pub view: Option<String>,
}

impl SourceId {
    /// The camera itself.
    pub fn camera(device: impl Into<String>, channel: u32) -> Self {
        SourceId { device: device.into(), channel, view: None }
    }

    /// One of that camera's virtual cameras.
    pub fn virtual_camera(device: impl Into<String>, channel: u32, view: impl Into<String>) -> Self {
        SourceId { device: device.into(), channel, view: Some(view.into()) }
    }

    /// What the connection behind this view is keyed by.
    ///
    /// A virtual camera and its parent share one, which is the whole point:
    /// one RTSP session and one decode however many crops are on the wall.
    pub fn stream_key(&self) -> (String, u32) {
        (self.device.clone(), self.channel)
    }

    /// The camera this is a view of, named the same way it is.
    pub fn stream_key_id(&self) -> SourceId {
        SourceId::camera(&self.device, self.channel)
    }

    pub fn is_virtual(&self) -> bool {
        self.view.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Channel {
    /// Position in this device's list, and how the UI keys a tile.
    pub index: u32,
    /// The identifier the vendor itself uses, kept exactly as given: a number
    /// for Reolink, a camera name for Frigate, a GUID for QNAP and UniFi.
    /// URLs are built from this; nothing treats it as a number.
    pub key: String,
    pub name: String,
    pub online: bool,
    pub model: String,
    /// Encoder type from GetEnc — decides the RTSP path (h264 vs h265).
    pub main_codec: String,
    pub sub_codec: String,
    pub ptz_supported: bool,
    pub ptz_presets_supported: bool,
    pub zoom_supported: bool,
    /// Whether the camera has a real zoom motor.
    ///
    /// Deliberately stricter than [`Channel::zoom_supported`], which accepts
    /// `ptzCtrl` as evidence. On an RLN36 every PTZ channel reports `ptzCtrl`
    /// while none reports `ptzZoom`: they pan and tilt only. Routing scroll on
    /// the looser flag would send zoom commands those cameras ignore, instead
    /// of falling back to a digital zoom that works everywhere.
    pub optical_zoom_supported: bool,
    /// A white-light floodlight, as opposed to the infrared illuminator.
    pub floodlight_supported: bool,
    pub ai_types: Vec<String>,
    pub lens: Lens,
    pub lens_partner: Option<u32>,
}

impl Channel {
    pub fn new(index: u32) -> Self {
        Channel {
            index,
            key: index.to_string(),
            name: String::new(),
            online: true,
            model: String::new(),
            main_codec: "h264".into(),
            sub_codec: "h264".into(),
            ptz_supported: false,
            ptz_presets_supported: false,
            zoom_supported: false,
            optical_zoom_supported: false,
            floodlight_supported: false,
            ai_types: Vec::new(),
            lens: Lens::None,
            lens_partner: None,
        }
    }

    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            format!("Channel {}", self.index + 1)
        } else {
            self.name.clone()
        }
    }

    pub fn is_dual_lens(&self) -> bool {
        self.lens_partner.is_some()
    }

    /// Map the abilityChn block onto the flags the UI cares about. Reolink
    /// encodes support as {"permit": N, "ver": N}, where ver > 0 means present.
    pub fn apply_abilities(&mut self, ability: &Value) {
        let has = |key: &str| -> bool {
            ability
                .get(key)
                .map(|entry| as_i64(entry.get("ver")) > 0)
                .unwrap_or(false)
        };

        self.ptz_supported = ["ptzCtrl", "ptzType", "ptzDirection"].iter().any(|k| has(k));
        self.ptz_presets_supported = has("ptzPreset");
        self.zoom_supported = has("ptzZoom") || has("ptzCtrl");
        self.optical_zoom_supported = has("ptzZoom");
        // Reported per channel, so an NVR can mix cameras that have a
        // floodlight with ones that do not.
        self.floodlight_supported = has("floodLight");

        self.ai_types = [
            ("supportAiPeople", "people"),
            ("supportAiVehicle", "vehicle"),
            ("supportAiDogCat", "dog_cat"),
            ("supportAiFace", "face"),
            ("supportAiPackage", "package"),
        ]
        .iter()
        .filter(|(key, _)| has(key))
        .map(|(_, name)| (*name).to_string())
        .collect();
    }
}

// ---------------------------------------------------------------- recordings

#[derive(Debug, Clone)]
pub struct Recording {
    pub channel: u32,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    /// Device-side path used as the Download source. Newer NVR firmware omits
    /// it entirely and indexes recordings by time, so it may be empty.
    pub name: String,
    pub size: i64,
    pub stream_type: StreamType,
    pub width: i64,
    pub height: i64,
    pub frame_rate: i64,
    /// Present on firmware that indexes playback by time rather than filename.
    pub playback_time: Option<NaiveDateTime>,
}

impl Recording {
    /// Whether this clip can actually be streamed or downloaded over HTTP.
    pub fn is_fetchable(&self) -> bool {
        !self.name.is_empty()
    }

    pub fn duration_seconds(&self) -> i64 {
        (self.end - self.start).num_seconds().max(0)
    }

    pub fn label(&self) -> String {
        let secs = self.duration_seconds();
        format!(
            "{} · {}m{:02}s",
            self.start.format("%H:%M:%S"),
            secs / 60,
            secs % 60
        )
    }

    /// Parse one entry of a Search result.
    ///
    /// Only the time span is genuinely required: `name` is absent on newer NVR
    /// firmware and numeric fields arrive as strings there, so neither may be
    /// treated as mandatory.
    pub fn parse(channel: u32, stream: StreamType, entry: &Value) -> Option<Self> {
        let start = parse_device_time(entry.get("StartTime")?)?;
        let end = parse_device_time(entry.get("EndTime")?)?;
        let playback_time = entry.get("PlaybackTime").and_then(parse_device_time);

        Some(Recording {
            channel,
            start,
            end,
            name: as_string(entry.get("name")),
            size: as_i64(entry.get("size")),
            stream_type: stream,
            width: as_i64(entry.get("width")),
            height: as_i64(entry.get("height")),
            frame_rate: as_i64(entry.get("frameRate")),
            playback_time,
        })
    }
}

// ---------------------------------------------------------------- events

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Motion,
    Person,
    Vehicle,
    Pet,
    Face,
    /// A parcel left in view. Reported by doorbells and a few of the newer
    /// cameras; others advertise it with support 0 and never raise it.
    Package,
}

impl EventKind {
    /// Every kind, in the order they are offered to the user.
    pub const ALL: [EventKind; 6] = [
        EventKind::Motion,
        EventKind::Person,
        EventKind::Vehicle,
        EventKind::Pet,
        EventKind::Face,
        EventKind::Package,
    ];

    /// The name the device uses for this detection.
    pub fn device_key(self) -> &'static str {
        match self {
            EventKind::Motion => "motion",
            EventKind::Person => "people",
            EventKind::Vehicle => "vehicle",
            EventKind::Pet => "dog_cat",
            EventKind::Face => "face",
            EventKind::Package => "package",
        }
    }

    pub fn from_device(key: &str) -> Option<Self> {
        Some(match key {
            "motion" => EventKind::Motion,
            "people" => EventKind::Person,
            "vehicle" => EventKind::Vehicle,
            "dog_cat" => EventKind::Pet,
            "face" => EventKind::Face,
            "package" => EventKind::Package,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            EventKind::Motion => "Motion",
            EventKind::Person => "Person",
            EventKind::Vehicle => "Vehicle",
            EventKind::Pet => "Pet",
            EventKind::Face => "Face",
            EventKind::Package => "Package",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dual_lens_detection_excludes_duo() {
        for model in ["TrackMix PoE", "Trackmix WiFi", "Reolink TrackMix"] {
            assert!(is_dual_lens_model(model), "{model}");
        }
        for model in ["RLC-810A", "Duo 2 POE", "RLN8-410", "RLN36", ""] {
            assert!(!is_dual_lens_model(model), "{model}");
        }
    }

    #[test]
    fn coerces_string_numbers() {
        assert_eq!(as_i64(Some(&json!("1186988032"))), 1186988032);
        assert_eq!(as_i64(Some(&json!(42))), 42);
        assert_eq!(as_i64(Some(&json!("not a number"))), 0);
        assert_eq!(as_i64(None), 0);
    }

    /// The exact payload an RLN36 on v3.5.0.329 returns: no `name`, and `size`
    /// as a string. The old parser discarded these outright.
    #[test]
    fn parses_a_nameless_nvr_recording() {
        let entry = json!({
            "EndTime":      {"day":30,"hour":17,"min":0,"mon":7,"sec":1,"year":2026},
            "PlaybackTime": {"day":30,"hour":20,"min":37,"mon":7,"sec":32,"year":2026},
            "StartTime":    {"day":30,"hour":16,"min":37,"mon":7,"sec":32,"year":2026},
            "frameRate": 0, "height": 0, "size": "1186988032", "type": "main", "width": 0
        });
        let rec = Recording::parse(0, StreamType::Main, &entry).expect("should parse");
        assert_eq!(rec.size, 1_186_988_032);
        assert!(rec.name.is_empty());
        assert!(!rec.is_fetchable());
        assert_eq!(rec.playback_time.unwrap().hour(), 20);
        assert_eq!(rec.duration_seconds(), 1349);
    }

    #[test]
    fn parses_a_named_recording_and_rejects_one_with_no_times() {
        let named = json!({
            "name": "Mp4Record/2026-07-30/RecM01_x.mp4",
            "StartTime": {"day":30,"hour":9,"min":0,"mon":7,"sec":0,"year":2026},
            "EndTime":   {"day":30,"hour":9,"min":5,"mon":7,"sec":0,"year":2026},
            "size": 5242880, "width": 2560, "height": 1920, "frameRate": 15
        });
        let rec = Recording::parse(0, StreamType::Main, &named).unwrap();
        assert!(rec.is_fetchable());
        assert_eq!(rec.width, 2560);

        assert!(Recording::parse(0, StreamType::Main, &json!({"size": 10})).is_none());
    }

    #[test]
    fn ability_flags_follow_ver() {
        let mut ch = Channel::new(0);
        ch.apply_abilities(&json!({
            "ptzCtrl": {"permit": 7, "ver": 1},
            "ptzPreset": {"permit": 7, "ver": 1},
            "supportAiPeople": {"permit": 6, "ver": 1},
            "supportAiVehicle": {"permit": 6, "ver": 0},
        }));
        assert!(ch.ptz_supported && ch.ptz_presets_supported);
        // ptzCtrl alone is pan/tilt: it must not be read as a zoom motor.
        assert!(!ch.optical_zoom_supported);
        assert_eq!(ch.ai_types, vec!["people".to_string()]);
    }

    #[test]
    fn device_info_kind_from_channel_count() {
        let nvr = DeviceInfo::parse(&json!({"DevInfo": {"model": "RLN36", "channelNum": 36}}));
        assert_eq!(nvr.kind(), DeviceKind::Nvr);
        let cam = DeviceInfo::parse(&json!({"DevInfo": {"model": "D340"}}));
        assert_eq!(cam.kind(), DeviceKind::Camera);
        assert_eq!(cam.channel_count, 1);
    }
}
