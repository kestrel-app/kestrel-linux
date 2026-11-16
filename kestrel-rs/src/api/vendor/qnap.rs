//! QNAP QVR Pro and QVR Elite.
//!
//! Authentication is the NAS's own `authLogin.cgi`, which answers XML and hands
//! back a session id used as `sid` on every later call. The camera list and the
//! snapshot path differ between QVR Pro and QVR Elite in ways the published
//! documentation is vague about, so both spellings of the list response are
//! accepted.
//!
//! **Never run against a real QNAP.** See `docs/untested.md`.

use base64::Engine;

use crate::api::error::{Error, Result};
use crate::api::http;
use crate::api::models::{Channel, DeviceInfo, StreamType};
use crate::config::DeviceConfig;

use super::{StreamSource, Vendor};

pub struct Qnap {
    base: String,
    host: String,
    rtsp_port: u16,
    username: String,
    password: String,
    agent: ureq::Agent,
    sid: std::sync::Mutex<String>,
    channels: Vec<Channel>,
    info: DeviceInfo,
}

impl Qnap {
    pub fn new(config: &DeviceConfig) -> Self {
        Qnap {
            base: http::base_url(&config.host, config.port, config.https),
            host: config.host.clone(),
            rtsp_port: config.rtsp_port,
            username: config.username.clone(),
            password: config.password.clone(),
            agent: http::agent(20, config.allow_self_signed),
            sid: std::sync::Mutex::new(String::new()),
            channels: Vec::new(),
            info: DeviceInfo::default(),
        }
    }

    fn sid(&self) -> String {
        self.sid.lock().unwrap().clone()
    }
}

/// Pull one element's text out of a small XML document.
///
/// Deliberately a string search rather than a parser: exactly two fields are
/// ever read from this response, and pulling in an XML dependency to find them
/// would be the larger cost. It would be the wrong call for anything more.
fn xml_value(body: &str, name: &str) -> String {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let Some(start) = body.find(&open) else { return String::new() };
    let rest = &body[start + open.len()..];
    let Some(end) = rest.find(&close) else { return String::new() };
    rest[..end].trim().to_string()
}

impl Vendor for Qnap {
    fn vendor_id(&self) -> &'static str {
        "qnap"
    }

    fn connect(&mut self) -> Result<DeviceInfo> {
        // The NAS wants the password base64'd in the query string.
        let encoded = base64::engine::general_purpose::STANDARD.encode(self.password.as_bytes());
        let login = http::request(
            &self.agent,
            "GET",
            &format!(
                "{}/cgi-bin/authLogin.cgi?user={}&pwd={}",
                self.base,
                http::encode(&self.username),
                http::encode(&encoded)
            ),
            None,
            &[],
        )?;
        if !login.ok() {
            return Err(http::describe(&login, "login"));
        }
        let sid = xml_value(&login.body, "authSid");
        if sid.is_empty() || xml_value(&login.body, "authPassed") == "0" {
            return Err(Error::auth(
                "login rejected — check the username and password",
            ));
        }
        *self.sid.lock().unwrap() = sid.clone();

        let list = http::request(
            &self.agent,
            "GET",
            &format!(
                "{}/qvrpro/apis/qvrpro/camera/list?sid={}",
                self.base,
                http::encode(&sid)
            ),
            None,
            &[],
        )?;
        if !list.ok() {
            return Err(http::describe(&list, "camera list"));
        }
        let parsed = list.json()?;
        // QVR Pro answers under "datas", QVR Elite under "cameras".
        let cameras = parsed
            .get("datas")
            .or_else(|| parsed.get("cameras"))
            .and_then(|c| c.as_array())
            .ok_or_else(|| Error::Protocol("camera list: no cameras in the response".into()))?;

        self.channels = cameras
            .iter()
            .enumerate()
            .filter_map(|(position, entry)| {
                let guid = entry
                    .get("guid")
                    .or_else(|| entry.get("id"))
                    .and_then(|g| g.as_str())?;
                let mut channel = Channel::new(position as u32);
                channel.key = guid.to_string();
                channel.name = entry
                    .get("name")
                    .and_then(|n| n.as_str())
                    .filter(|n| !n.is_empty())
                    .unwrap_or("Camera")
                    .to_string();
                let state = entry.get("state").or_else(|| entry.get("status"));
                channel.online = state.and_then(|s| s.as_i64()).map(|s| s != 0).unwrap_or(true);
                Some(channel)
            })
            .collect();

        self.info = DeviceInfo {
            name: "QVR".into(),
            model: "QNAP QVR".into(),
            channel_count: self.channels.len(),
            ..DeviceInfo::default()
        };
        Ok(self.info.clone())
    }

    fn logout(&self) {
        let sid = self.sid();
        if sid.is_empty() {
            return;
        }
        let _ = http::request(
            &self.agent,
            "GET",
            &format!("{}/cgi-bin/authLogout.cgi?sid={}", self.base, http::encode(&sid)),
            None,
            &[],
        );
    }

    fn channels(&self) -> &[Channel] {
        &self.channels
    }

    fn stream(&self, channel: &Channel, stream: StreamType) -> Result<StreamSource> {
        // QVR republishes each camera over RTSP keyed by its GUID. The stream
        // number selects the profile the camera was added with.
        let profile = match stream {
            StreamType::Main => 0,
            StreamType::Sub => 1,
        };
        Ok(StreamSource::new(format!(
            "rtsp://{}:{}@{}:{}/qvrpro/{}/{profile}",
            http::encode(&self.username),
            http::encode(&self.password),
            self.host,
            self.rtsp_port,
            http::encode(&channel.key)
        )))
    }

    fn snapshot(&self, channel: &Channel) -> Result<Vec<u8>> {
        let url = format!(
            "{}/qvrpro/camera/snapshot/{}?sid={}",
            self.base,
            http::encode(&channel.key),
            http::encode(&self.sid())
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
    fn reads_a_field_out_of_the_login_xml() {
        let body = r#"<?xml version="1.0"?><QDocRoot version="1.0">
            <authPassed>1</authPassed><authSid>abc123</authSid></QDocRoot>"#;
        assert_eq!(xml_value(body, "authSid"), "abc123");
        assert_eq!(xml_value(body, "authPassed"), "1");
        assert_eq!(xml_value(body, "missing"), "");
    }

    #[test]
    fn a_rejected_login_is_recognisable() {
        let body = "<QDocRoot><authPassed>0</authPassed></QDocRoot>";
        assert_eq!(xml_value(body, "authSid"), "");
        assert_eq!(xml_value(body, "authPassed"), "0");
    }

    #[test]
    fn guids_are_encoded_into_the_stream_url() {
        let config = DeviceConfig {
            host: "nas".into(),
            rtsp_port: 554,
            username: "admin".into(),
            ..DeviceConfig::default()
        };
        let qnap = Qnap::new(&config);
        let mut channel = Channel::new(0);
        channel.key = "5f9a-11ec".into();
        let url = qnap.stream(&channel, StreamType::Main).unwrap().url;
        assert!(url.starts_with("rtsp://admin:@nas:554/qvrpro/"), "{url}");
        assert!(url.ends_with("/0"), "{url}");
    }
}
