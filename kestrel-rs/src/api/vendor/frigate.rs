//! Frigate.
//!
//! The simplest of the non-Reolink systems: no session, cameras named rather
//! than numbered, and a config endpoint that lists them all.
//!
//! Live video comes from Frigate's bundled go2rtc, which republishes every
//! camera as RTSP on port 8554 under the camera's own name. That is the default
//! layout; an install that has moved it needs the port changed on the device.
//!
//! **Never run against a real Frigate.** See `docs/untested.md`.

use crate::api::error::{Error, Result};
use crate::api::http;
use crate::api::models::{Channel, DeviceInfo, StreamType};
use crate::config::DeviceConfig;

use super::{StreamSource, Vendor};

/// go2rtc's RTSP port inside Frigate, which is where live streams are served.
const GO2RTC_RTSP_PORT: u16 = 8554;

pub struct Frigate {
    base: String,
    rtsp_host: String,
    rtsp_port: u16,
    agent: ureq::Agent,
    channels: Vec<Channel>,
    info: DeviceInfo,
}

impl Frigate {
    pub fn new(config: &DeviceConfig) -> Self {
        Frigate {
            base: http::base_url(&config.host, config.port, config.https),
            rtsp_host: config.host.clone(),
            // A device configured with the Reolink default of 554 means "not
            // set" here, so fall back to where Frigate actually serves.
            rtsp_port: if config.rtsp_port == 554 {
                GO2RTC_RTSP_PORT
            } else {
                config.rtsp_port
            },
            agent: http::agent(20, config.allow_self_signed),
            channels: Vec::new(),
            info: DeviceInfo::default(),
        }
    }

    fn get(&self, path: &str) -> Result<http::Response> {
        http::request(&self.agent, "GET", &format!("{}{path}", self.base), None, &[])
    }
}

/// Frigate labels objects with COCO class names; Kestrel groups them the way
/// its own detection types do, so alerts and follow-motion mean the same thing
/// whichever system a camera is on.
fn classify(label: &str) -> Option<&'static str> {
    match label.to_ascii_lowercase().as_str() {
        "person" | "people" => Some("people"),
        "face" => Some("face"),
        "car" | "truck" | "bus" | "motorcycle" | "bicycle" | "vehicle" => Some("vehicle"),
        "dog" | "cat" | "bird" | "horse" | "animal" => Some("dog_cat"),
        "package" => Some("package"),
        // Anything else Frigate was trained on still counts as movement.
        "" => None,
        _ => Some("motion"),
    }
}

/// `front_door` reads better as `Front Door`.
fn label(name: &str) -> String {
    name.split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Vendor for Frigate {
    fn vendor_id(&self) -> &'static str {
        "frigate"
    }

    fn connect(&mut self) -> Result<DeviceInfo> {
        let version = self.get("/api/version")?;
        if !version.ok() {
            return Err(http::describe(&version, "Frigate"));
        }
        let firmware = version.body.trim().to_string();

        let config = self.get("/api/config")?;
        if !config.ok() {
            return Err(http::describe(&config, "config"));
        }
        let parsed = config.json()?;
        let cameras = parsed
            .get("cameras")
            .and_then(|c| c.as_object())
            .ok_or_else(|| Error::Protocol("config: no cameras in the response".into()))?;

        // Frigate hands back a map, whose order is not meaningful. Sorting by
        // name keeps the wall stable between restarts.
        let mut names: Vec<&String> = cameras.keys().collect();
        names.sort();

        self.channels = names
            .into_iter()
            .enumerate()
            .map(|(position, name)| {
                let camera = &cameras[name];
                let mut channel = Channel::new(position as u32);
                channel.key = name.clone();
                channel.name = label(name);
                channel.online = camera
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                // Frigate detects, but its detections arrive over MQTT rather
                // than the HTTP API, so nothing is claimed here.
                channel
            })
            .collect();

        self.info = DeviceInfo {
            name: "Frigate".into(),
            model: "Frigate".into(),
            firmware,
            channel_count: self.channels.len(),
            ..DeviceInfo::default()
        };
        Ok(self.info.clone())
    }

    fn logout(&self) {}

    fn channels(&self) -> &[Channel] {
        &self.channels
    }

    fn stream(&self, channel: &Channel, stream: StreamType) -> Result<StreamSource> {
        // go2rtc publishes the camera under its own name; Frigate additionally
        // exposes a `_sub` restream for the detect resolution when one is
        // configured.
        let name = match stream {
            StreamType::Main => channel.key.clone(),
            StreamType::Sub => format!("{}_sub", channel.key),
        };
        Ok(StreamSource::new(format!(
            "rtsp://{}:{}/{}",
            self.rtsp_host,
            self.rtsp_port,
            http::encode(&name)
        )))
    }

    fn detections(&self, channels: &[u32]) -> Result<Vec<(u32, Vec<(String, bool)>)>> {
        // Frigate keeps an event open for as long as the object is in view, so
        // "in progress" is exactly the set of cameras detecting right now.
        let response = self.get("/api/events?in_progress=1&limit=50")?;
        if !response.ok() {
            return Err(http::describe(&response, "events"));
        }
        let events = response.json()?;
        let events = events.as_array().cloned().unwrap_or_default();

        Ok(channels
            .iter()
            .filter_map(|&index| {
                let camera = self.channels.iter().find(|c| c.index == index)?;
                let mut flags: Vec<(String, bool)> = Vec::new();
                for event in &events {
                    if event.get("camera").and_then(|c| c.as_str()) != Some(camera.key.as_str()) {
                        continue;
                    }
                    let label = event.get("label").and_then(|l| l.as_str()).unwrap_or("");
                    if let Some(kind) = classify(label) {
                        if !flags.iter().any(|(k, _)| k == kind) {
                            flags.push((kind.to_string(), true));
                        }
                    }
                }
                Some((index, flags))
            })
            .collect())
    }

    fn snapshot(&self, channel: &Channel) -> Result<Vec<u8>> {
        let url = format!(
            "{}/api/{}/latest.jpg?h=1080",
            self.base,
            http::encode(&channel.key)
        );
        let response = self
            .agent
            .get(&url)
            .call()
            .map_err(|err| Error::connection(err.to_string()))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|err| Error::connection(err.to_string()))?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frigate_labels_map_onto_kestrels_types() {
        assert_eq!(classify("person"), Some("people"));
        assert_eq!(classify("car"), Some("vehicle"));
        assert_eq!(classify("truck"), Some("vehicle"));
        assert_eq!(classify("dog"), Some("dog_cat"));
        assert_eq!(classify("package"), Some("package"));
        assert_eq!(classify("Person"), Some("people"), "case should not matter");
        // An object Frigate knows and Kestrel does not is still movement.
        assert_eq!(classify("umbrella"), Some("motion"));
        assert_eq!(classify(""), None);
    }

    #[test]
    fn camera_names_become_labels() {
        assert_eq!(label("front_door"), "Front Door");
        assert_eq!(label("driveway"), "Driveway");
        assert_eq!(label("back-yard_north"), "Back Yard North");
        assert_eq!(label(""), "");
    }

    #[test]
    fn substream_uses_frigates_restream_naming() {
        let config = DeviceConfig {
            host: "nvr".into(),
            port: 5000,
            rtsp_port: 554,
            ..DeviceConfig::default()
        };
        let mut frigate = Frigate::new(&config);
        let mut channel = Channel::new(0);
        channel.key = "front_door".into();
        frigate.channels.push(channel.clone());

        let main = frigate.stream(&channel, StreamType::Main).unwrap();
        let sub = frigate.stream(&channel, StreamType::Sub).unwrap();
        // The Reolink default of 554 is not where Frigate serves.
        assert_eq!(main.url, "rtsp://nvr:8554/front%5Fdoor");
        assert_eq!(sub.url, "rtsp://nvr:8554/front%5Fdoor%5Fsub");
    }

    #[test]
    fn an_explicit_rtsp_port_is_respected() {
        let config = DeviceConfig {
            host: "nvr".into(),
            rtsp_port: 9554,
            ..DeviceConfig::default()
        };
        let frigate = Frigate::new(&config);
        assert_eq!(frigate.rtsp_port, 9554);
    }
}
