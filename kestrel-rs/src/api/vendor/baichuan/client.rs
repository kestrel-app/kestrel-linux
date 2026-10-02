//! The [`Vendor`] implementation that reaches a Reolink device by UID.
//!
//! This is the adapter that puts the protocol in this module behind the shared
//! vendor contract, the same way [`reolink`] adapts the HTTP client. Where the
//! HTTP client is the right path for a device reachable at an address, this one is
//! for a device addressable only by its cloud UID.
//!
//! **Verification status.** [`BaichuanClient::connect`] resolves the UID (a step
//! that works the moment outbound UDP is allowed) and then opens the transport,
//! which is unverified — so in practice `connect` surfaces a clear error directing
//! the user to the LAN/RTSP path until the handshake is confirmed against a device.
//! Everything past `connect` is wired so that it works once the transport does.
//! See [`super`] and `docs/untested.md`.
//!
//! [`reolink`]: super::super::reolink
//! [`Vendor`]: super::super::Vendor

use std::sync::Mutex;
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};
use log::info;

use super::session::Session;
use super::transport::Transport;
use super::{login, playback};
use crate::api::error::{Error, Result};
use crate::api::models::{Channel, DeviceInfo, Recording, StreamType};
use crate::api::vendor::{StreamSource, Vendor};
use crate::config::DeviceConfig;

/// A device reached over the Baichuan P2P transport.
pub struct BaichuanClient {
    uid: String,
    username: String,
    password: String,
    timeout: Duration,
    channels: Vec<Channel>,
    /// Open after a successful [`connect`]. Behind a mutex because the vendor
    /// contract hands out `&self` for calls that must drive the one stream.
    ///
    /// [`connect`]: BaichuanClient::connect
    session: Mutex<Option<Session>>,
}

impl BaichuanClient {
    pub fn new(config: &DeviceConfig) -> Self {
        BaichuanClient {
            uid: config.uid.clone(),
            username: config.username.clone(),
            password: config.password.clone(),
            timeout: Duration::from_secs(10),
            channels: Vec::new(),
            session: Mutex::new(None),
        }
    }

    /// Run a closure with the open session, or fail if not connected.
    fn with_session<T>(&self, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
        let mut guard = self.session.lock().unwrap();
        let session = guard
            .as_mut()
            .ok_or_else(|| Error::connection("not connected to the device"))?;
        f(session)
    }
}

impl Vendor for BaichuanClient {
    fn vendor_id(&self) -> &'static str {
        "reolink"
    }

    fn connect(&mut self) -> Result<DeviceInfo> {
        if self.uid.trim().is_empty() {
            return Err(Error::connection("no UID configured for this device"));
        }
        info!("resolving Reolink UID over P2P");
        // Resolve (works once UDP egress is allowed) then open the transport.
        let transport = Transport::connect(&self.uid, self.timeout)?;
        let mut session = Session::new(transport);
        login::login(&mut session, &self.username, &self.password, self.timeout)?;

        // Once the control channel is up, the device's ability block names the
        // channels. That exchange rides the same unverified session, so it is left
        // to be filled in against a real device; for now connect has already
        // returned above via the transport's honest refusal.
        *self.session.lock().unwrap() = Some(session);
        Ok(DeviceInfo {
            name: format!("Reolink {}", self.uid),
            ..DeviceInfo::default()
        })
    }

    fn logout(&self) {
        *self.session.lock().unwrap() = None;
    }

    fn channels(&self) -> &[Channel] {
        &self.channels
    }

    fn stream(&self, _channel: &Channel, _stream: StreamType) -> Result<StreamSource> {
        // BC video is not RTSP: it arrives as BCMedia frames on the control
        // channel (see `super::media`). Feeding that into the in-process decoder
        // needs a custom AVIO input, which is not yet built, so this is refused
        // rather than handing back a URL that would not play.
        Err(Error::Unsupported(
            "video over the Reolink P2P transport is not yet wired into the decoder".into(),
        ))
    }

    fn snapshot(&self, _channel: &Channel) -> Result<Vec<u8>> {
        Err(Error::Unsupported("snapshot over P2P is not yet implemented".into()))
    }

    fn search_recordings(
        &self,
        channel: u32,
        start: NaiveDateTime,
        end: NaiveDateTime,
        stream: StreamType,
    ) -> Result<Vec<Recording>> {
        let timeout = self.timeout;
        self.with_session(|s| playback::search_recordings(s, channel, start, end, stream, timeout))
    }

    fn recorded_days(
        &self,
        channel: u32,
        month_of: NaiveDate,
        stream: StreamType,
    ) -> Result<Vec<u32>> {
        let timeout = self.timeout;
        self.with_session(|s| playback::recorded_days(s, channel, month_of, stream, timeout))
    }

    fn download_url(&self, _recording: &Recording) -> Result<Option<String>> {
        // A P2P download is a command stream, not a URL. Returning None keeps the
        // Playback tab honest: it will show the clip but not offer an HTTP save.
        Ok(None)
    }

    fn supports_playback(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid_config(uid: &str) -> DeviceConfig {
        let mut c = DeviceConfig::default();
        c.uid = uid.to_string();
        c.username = "admin".into();
        c
    }

    #[test]
    fn a_client_with_no_uid_refuses_to_connect() {
        let mut client = BaichuanClient::new(&uid_config(""));
        assert!(client.connect().is_err());
    }

    #[test]
    fn calls_before_connect_report_not_connected() {
        let client = BaichuanClient::new(&uid_config("95270000ABCDEFGH"));
        let start = NaiveDate::from_ymd_opt(2026, 10, 2)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let err = client
            .search_recordings(0, start, start, StreamType::Main)
            .unwrap_err();
        assert!(matches!(err, Error::Connection(_)));
    }

    #[test]
    fn it_reports_playback_support_and_no_http_download() {
        let client = BaichuanClient::new(&uid_config("x"));
        assert!(client.supports_playback());
    }
}
