//! UniFi Protect.
//!
//! Needs a **local** account on the console, not a Ubiquiti cloud one: cloud
//! accounts require two-factor, which this cannot complete. The console answers
//! a login with a session cookie *and* a CSRF token, both of which every later
//! request needs — which is why the shared HTTP layer takes headers.
//!
//! Live video is RTSPS on port 7441, keyed by a per-stream alias that Protect
//! only publishes once the stream has been enabled for that camera in its
//! settings. A camera with no alias cannot be viewed, and says so.
//!
//! **Never run against a real UniFi Protect.** See `docs/untested.md`.

use crate::api::error::{Error, Result};
use crate::api::http;
use crate::api::models::{Channel, DeviceInfo, StreamType};
use crate::config::DeviceConfig;

use super::{StreamSource, Vendor};

/// Where Protect serves RTSPS, regardless of the console's HTTPS port.
const RTSPS_PORT: u16 = 7441;

#[derive(Default, Clone)]
struct Session {
    cookie: String,
    csrf: String,
}

pub struct Unifi {
    base: String,
    host: String,
    username: String,
    password: String,
    agent: ureq::Agent,
    session: std::sync::Mutex<Session>,
    channels: Vec<Channel>,
    /// The RTSPS alias per camera, learned at bootstrap. Absent means the
    /// camera's stream has not been enabled in Protect.
    aliases: std::collections::HashMap<String, String>,
    info: DeviceInfo,
}

impl Unifi {
    pub fn new(config: &DeviceConfig) -> Self {
        Unifi {
            // Protect is HTTPS-only in practice, whatever the config says.
            base: http::base_url(&config.host, config.port, true),
            host: config.host.clone(),
            username: config.username.clone(),
            password: config.password.clone(),
            agent: http::agent(25, config.allow_self_signed),
            session: std::sync::Mutex::new(Session::default()),
            channels: Vec::new(),
            aliases: std::collections::HashMap::new(),
            info: DeviceInfo::default(),
        }
    }

    /// The cookie and CSRF header every authenticated call carries.
    fn headers(&self) -> Vec<(String, String)> {
        let session = self.session.lock().unwrap();
        let mut headers = Vec::new();
        if !session.cookie.is_empty() {
            headers.push(("Cookie".to_string(), session.cookie.clone()));
        }
        if !session.csrf.is_empty() {
            headers.push(("X-CSRF-Token".to_string(), session.csrf.clone()));
        }
        headers
    }
}

impl Vendor for Unifi {
    fn vendor_id(&self) -> &'static str {
        "unifi"
    }

    fn connect(&mut self) -> Result<DeviceInfo> {
        let body = serde_json::json!({
            "username": self.username,
            "password": self.password,
            "rememberMe": true,
        })
        .to_string();
        let login = http::request(
            &self.agent,
            "POST",
            &format!("{}/api/auth/login", self.base),
            Some(&body),
            &[("Content-Type".into(), "application/json".into())],
        )?;
        if !login.ok() {
            // Protect answers 499 when the account needs two-factor.
            if login.status == 499 {
                return Err(Error::auth(
                    "this account requires two-factor — use a local account on the console",
                ));
            }
            return Err(http::describe(&login, "login"));
        }

        let cookie = login
            .header("set-cookie")
            .map(|c| c.split(';').next().unwrap_or(c).to_string())
            .unwrap_or_default();
        if cookie.is_empty() {
            return Err(Error::auth("login returned no session cookie"));
        }
        *self.session.lock().unwrap() = Session {
            cookie,
            csrf: login.header("x-csrf-token").unwrap_or_default().to_string(),
        };

        let bootstrap = http::request(
            &self.agent,
            "GET",
            &format!("{}/proxy/protect/api/bootstrap", self.base),
            None,
            &self.headers(),
        )?;
        if !bootstrap.ok() {
            return Err(http::describe(&bootstrap, "bootstrap"));
        }
        let parsed = bootstrap.json()?;
        let cameras = parsed
            .get("cameras")
            .and_then(|c| c.as_array())
            .ok_or_else(|| Error::Protocol("bootstrap: no cameras in the response".into()))?;

        self.channels.clear();
        self.aliases.clear();
        for (position, camera) in cameras.iter().enumerate() {
            let Some(id) = camera.get("id").and_then(|i| i.as_str()) else { continue };
            let mut channel = Channel::new(position as u32);
            channel.key = id.to_string();
            channel.name = camera
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|n| !n.is_empty())
                .unwrap_or("Camera")
                .to_string();
            channel.online = camera
                .get("isConnected")
                .and_then(|c| c.as_bool())
                .unwrap_or_else(|| {
                    camera.get("state").and_then(|s| s.as_str()) == Some("CONNECTED")
                });
            channel.model = camera
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string();

            // The highest-quality channel with an alias is the main stream.
            if let Some(streams) = camera.get("channels").and_then(|c| c.as_array()) {
                if let Some(alias) = streams
                    .iter()
                    .find_map(|s| s.get("rtspAlias").and_then(|a| a.as_str()))
                {
                    self.aliases.insert(id.to_string(), alias.to_string());
                }
            }
            self.channels.push(channel);
        }

        let nvr = parsed.get("nvr");
        self.info = DeviceInfo {
            name: nvr
                .and_then(|n| n.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("UniFi Protect")
                .to_string(),
            model: "UniFi Protect".into(),
            firmware: nvr
                .and_then(|n| n.get("version"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            channel_count: self.channels.len(),
            ..DeviceInfo::default()
        };
        Ok(self.info.clone())
    }

    fn logout(&self) {
        if self.session.lock().unwrap().cookie.is_empty() {
            return;
        }
        let _ = http::request(
            &self.agent,
            "POST",
            &format!("{}/api/auth/logout", self.base),
            None,
            &self.headers(),
        );
    }

    fn channels(&self) -> &[Channel] {
        &self.channels
    }

    fn stream(&self, channel: &Channel, _stream: StreamType) -> Result<StreamSource> {
        // Protect serves RTSPS by alias, and only for cameras whose stream has
        // been switched on. Saying which camera and what to do about it beats a
        // connection that simply never opens.
        let alias = self.aliases.get(&channel.key).ok_or_else(|| {
            Error::Unsupported(format!(
                "{} has no RTSP stream enabled — turn it on in Protect under \
                 the camera's settings",
                channel.display_name()
            ))
        })?;
        Ok(StreamSource::new(format!(
            "rtsps://{}:{RTSPS_PORT}/{}?enableSrtp",
            self.host,
            http::encode(alias)
        )))
    }

    fn detections(&self, channels: &[u32]) -> Result<Vec<(u32, Vec<(String, bool)>)>> {
        // Protect has no dedicated endpoint for this; the bootstrap carries a
        // live isMotionDetected per camera, which is what its own app reads.
        let response = http::request(
            &self.agent,
            "GET",
            &format!("{}/proxy/protect/api/bootstrap", self.base),
            None,
            &self.headers(),
        )?;
        if !response.ok() {
            return Err(http::describe(&response, "bootstrap"));
        }
        let parsed = response.json()?;
        let cameras = parsed.get("cameras").and_then(|c| c.as_array());
        let Some(cameras) = cameras else { return Ok(Vec::new()) };

        Ok(channels
            .iter()
            .filter_map(|&index| {
                let channel = self.channels.iter().find(|c| c.index == index)?;
                let camera = cameras
                    .iter()
                    .find(|c| c.get("id").and_then(|i| i.as_str()) == Some(&channel.key))?;
                if camera.get("isMotionDetected").and_then(|m| m.as_bool()) != Some(true) {
                    return Some((index, Vec::new()));
                }
                // Protect names what it saw when its smart detection is on;
                // otherwise all that is known is that something moved.
                let smart = camera
                    .get("lastDetectedObjects")
                    .or_else(|| camera.get("smartDetectTypes"))
                    .and_then(|o| o.as_array())
                    .and_then(|o| o.first())
                    .and_then(|o| o.as_str())
                    .unwrap_or("motion");
                let kind = match smart {
                    "person" => "people",
                    "vehicle" => "vehicle",
                    "animal" => "dog_cat",
                    "package" => "package",
                    "face" => "face",
                    _ => "motion",
                };
                Some((index, vec![(kind.to_string(), true)]))
            })
            .collect())
    }

    fn snapshot(&self, channel: &Channel) -> Result<Vec<u8>> {
        let url = format!(
            "{}/proxy/protect/api/cameras/{}/snapshot?force=true",
            self.base,
            http::encode(&channel.key)
        );
        let mut request = self.agent.get(&url);
        for (name, value) in self.headers() {
            request = request.set(&name, &value);
        }
        let response = request
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

    fn unifi() -> Unifi {
        Unifi::new(&DeviceConfig {
            host: "console".into(),
            port: 443,
            ..DeviceConfig::default()
        })
    }

    #[test]
    fn a_camera_without_an_alias_says_what_to_do() {
        let client = unifi();
        let mut channel = Channel::new(0);
        channel.key = "abc".into();
        channel.name = "Driveway".into();

        let err = client.stream(&channel, StreamType::Main).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Driveway"), "{message}");
        assert!(message.contains("Protect"), "{message}");
    }

    #[test]
    fn an_enabled_camera_streams_over_rtsps() {
        let mut client = unifi();
        let mut channel = Channel::new(0);
        channel.key = "abc".into();
        client.aliases.insert("abc".into(), "7NqQqLmS".into());

        let url = client.stream(&channel, StreamType::Main).unwrap().url;
        assert_eq!(url, "rtsps://console:7441/7NqQqLmS?enableSrtp");
    }

    #[test]
    fn both_session_pieces_travel_on_every_call() {
        let client = unifi();
        *client.session.lock().unwrap() = Session {
            cookie: "TOKEN=abc".into(),
            csrf: "xyz".into(),
        };
        let headers = client.headers();
        assert!(headers.iter().any(|(k, v)| k == "Cookie" && v == "TOKEN=abc"));
        assert!(headers.iter().any(|(k, v)| k == "X-CSRF-Token" && v == "xyz"));
    }
}
