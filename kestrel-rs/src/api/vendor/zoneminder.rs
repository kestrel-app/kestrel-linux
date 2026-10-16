//! ZoneMinder 1.36 and newer.
//!
//! Token authentication through `/api/host/login.json`, monitors listed from
//! `/api/monitors.json`, and video from `nph-zms` — which serves MJPEG rather
//! than RTSP. ffmpeg reads that happily, so live view works; there is simply no
//! keyframe concept, which makes warm streams pointless but harmless.
//!
//! The `/zm` path prefix is assumed, as it is in the default packaging. An
//! install at the web root needs it removed.
//!
//! **Never run against a real ZoneMinder.** See `docs/untested.md`.

use crate::api::error::{Error, Result};
use crate::api::http;
use crate::api::models::{Channel, DeviceInfo, StreamType};
use crate::config::DeviceConfig;

use super::{StreamSource, Vendor};

/// Where ZoneMinder sits under the web root in the usual packaging.
const PREFIX: &str = "/zm";

pub struct ZoneMinder {
    base: String,
    username: String,
    password: String,
    agent: ureq::Agent,
    token: std::sync::Mutex<String>,
    channels: Vec<Channel>,
    info: DeviceInfo,
}

impl ZoneMinder {
    pub fn new(config: &DeviceConfig) -> Self {
        ZoneMinder {
            base: http::base_url(&config.host, config.port, config.https),
            username: config.username.clone(),
            password: config.password.clone(),
            agent: http::agent(20, config.allow_self_signed),
            token: std::sync::Mutex::new(String::new()),
            channels: Vec::new(),
            info: DeviceInfo::default(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{PREFIX}{path}", self.base)
    }

    fn token(&self) -> String {
        self.token.lock().unwrap().clone()
    }
}

/// ZoneMinder reports booleans as "1"/"0" strings as often as numbers.
fn truthy(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::String(s)) => s != "0" && !s.is_empty(),
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(serde_json::Value::Bool(b)) => *b,
        _ => true,
    }
}

impl Vendor for ZoneMinder {
    fn vendor_id(&self) -> &'static str {
        "zoneminder"
    }

    fn connect(&mut self) -> Result<DeviceInfo> {
        let body = format!(
            "user={}&pass={}",
            http::encode(&self.username),
            http::encode(&self.password)
        );
        let login = http::request(
            &self.agent,
            "POST",
            &self.url("/api/host/login.json"),
            Some(&body),
            &[(
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            )],
        )?;
        if !login.ok() {
            return Err(http::describe(&login, "login"));
        }
        let parsed = login.json()?;
        let token = parsed
            .get("access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| Error::auth("login rejected — check the username and password"))?;
        *self.token.lock().unwrap() = token.to_string();

        let monitors = http::request(
            &self.agent,
            "GET",
            &format!(
                "{}?token={}",
                self.url("/api/monitors.json"),
                http::encode(token)
            ),
            None,
            &[],
        )?;
        if !monitors.ok() {
            return Err(http::describe(&monitors, "monitors"));
        }
        let parsed = monitors.json()?;
        let list = parsed
            .get("monitors")
            .and_then(|m| m.as_array())
            .ok_or_else(|| Error::Protocol("monitors: no list in the response".into()))?;

        self.channels = list
            .iter()
            .enumerate()
            .filter_map(|(position, entry)| {
                // The API nests each monitor under a "Monitor" key; some
                // versions return it flat.
                let monitor = entry.get("Monitor").unwrap_or(entry);
                let id = monitor.get("Id")?;
                let id = match id {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let mut channel = Channel::new(position as u32);
                channel.name = monitor
                    .get("Name")
                    .and_then(|n| n.as_str())
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Monitor {id}"));
                channel.key = id;
                // A monitor set to "None" is configured but not running.
                let functioning = monitor
                    .get("Function")
                    .and_then(|f| f.as_str())
                    .map(|f| f != "None")
                    .unwrap_or(true);
                channel.online = truthy(monitor.get("Enabled")) && functioning;
                Some(channel)
            })
            .collect();

        self.info = DeviceInfo {
            name: "ZoneMinder".into(),
            model: "ZoneMinder".into(),
            channel_count: self.channels.len(),
            ..DeviceInfo::default()
        };
        Ok(self.info.clone())
    }

    fn logout(&self) {
        let token = self.token();
        if token.is_empty() {
            return;
        }
        let _ = http::request(
            &self.agent,
            "GET",
            &format!(
                "{}?token={}",
                self.url("/api/host/logout.json"),
                http::encode(&token)
            ),
            None,
            &[],
        );
    }

    fn channels(&self) -> &[Channel] {
        &self.channels
    }

    fn stream(&self, channel: &Channel, stream: StreamType) -> Result<StreamSource> {
        // nph-zms streams MJPEG. `scale` is the only quality control it offers,
        // so the sub stream is the same feed at half size.
        let scale = match stream {
            StreamType::Main => 100,
            StreamType::Sub => 50,
        };
        Ok(StreamSource::new(format!(
            "{}?mode=jpeg&monitor={}&scale={scale}&maxfps=15&token={}",
            self.url("/cgi-bin/nph-zms"),
            http::encode(&channel.key),
            http::encode(&self.token())
        )))
    }

    fn snapshot(&self, channel: &Channel) -> Result<Vec<u8>> {
        let url = format!(
            "{}?mode=single&monitor={}&token={}",
            self.url("/cgi-bin/nph-zms"),
            http::encode(&channel.key),
            http::encode(&self.token())
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
    fn zoneminders_mixed_boolean_spellings() {
        use serde_json::json;
        assert!(truthy(Some(&json!("1"))));
        assert!(truthy(Some(&json!(1))));
        assert!(truthy(Some(&json!(true))));
        assert!(!truthy(Some(&json!("0"))));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!(false))));
        // Absent means the install predates the field: assume it is running.
        assert!(truthy(None));
    }

    #[test]
    fn streams_differ_only_by_scale() {
        let config = DeviceConfig {
            host: "zm".into(),
            port: 80,
            ..DeviceConfig::default()
        };
        let zm = ZoneMinder::new(&config);
        let mut channel = Channel::new(0);
        channel.key = "7".into();

        let main = zm.stream(&channel, StreamType::Main).unwrap().url;
        let sub = zm.stream(&channel, StreamType::Sub).unwrap().url;
        assert!(main.contains("/zm/cgi-bin/nph-zms"), "{main}");
        assert!(main.contains("monitor=7"), "{main}");
        assert!(main.contains("scale=100"), "{main}");
        assert!(sub.contains("scale=50"), "{sub}");
    }
}
