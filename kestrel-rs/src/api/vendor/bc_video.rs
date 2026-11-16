//! A Reolink device at an address, with live video over Baichuan.
//!
//! The HTTP client does everything it already did — channels, snapshots,
//! detections, PTZ, floodlight — because the Baichuan client does not yet have
//! those commands. Live video and playback move. Playback is the same as for a
//! device reached by UID: search, calendar and footage over Baichuan, all on the
//! one connection live video uses (this NVR answers no second login). For live video: each stream it hands out keeps
//! its RTSP URL *and* carries a Baichuan descriptor, so the stream worker tries
//! Baichuan over TCP and falls back to the RTSP URL if that fails. The first
//! failure is remembered for the whole device, so a device that does not answer
//! Baichuan costs one wait, not one per tile.
//!
//! Rolling back: switch the device's "Live video" setting to RTSP only, which
//! builds the plain HTTP client again (see [`super::build`]).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use chrono::{NaiveDate, NaiveDateTime};
use log::warn;

use super::baichuan::link::{Reach, DEFAULT_BC_PORT};
use super::{BaichuanVideo, StreamSource, Vendor};
use crate::api::error::Result;
use crate::api::models::{BatteryLevel, Channel, DeviceInfo, Recording, StreamType};
use crate::api::settings::Block;
use crate::config::DeviceConfig;

pub struct WithBcVideo {
    http: Box<dyn Vendor>,
    reach: Reach,
    username: String,
    password: String,
    rtsp_fallback: Arc<AtomicBool>,
    /// The HTTP client's channels, with what Baichuan adds: which run on a
    /// battery. Empty until connected.
    channels: Vec<Channel>,
}

impl WithBcVideo {
    pub fn new(http: Box<dyn Vendor>, config: &DeviceConfig) -> Self {
        WithBcVideo {
            http,
            reach: Reach::Tcp { host: config.host.trim().to_string(), port: DEFAULT_BC_PORT },
            username: config.username.clone(),
            password: config.password.clone(),
            rtsp_fallback: Arc::new(AtomicBool::new(false)),
            channels: Vec::new(),
        }
    }
}

/// How long a Baichuan command over the shared connection may take.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl WithBcVideo {
    fn caller(&self) -> Result<super::baichuan::hub::Caller> {
        super::baichuan::hub::Caller::open(&self.reach, &self.username, &self.password, TIMEOUT)
    }
}

impl WithBcVideo {
    /// The battery over Baichuan: `BatteryInfoList` (253) for the channel,
    /// on the device's shared connection.
    fn baichuan_battery(&self, channel: u32) -> Result<Option<BatteryLevel>> {
        let hub = super::baichuan::hub::hold(&self.reach, &self.username, &self.password, TIMEOUT)?;
        let reply = hub.call_for(channel, super::baichuan::cmd::BATTERY_INFO, String::new(), TIMEOUT)?;
        if reply.response_code != 200 {
            return Err(crate::api::error::Error::command("BatteryInfo", reply.response_code as i64, "refused by the device"));
        }
        let text = reply.xml().unwrap_or_default();
        log::debug!("{}: baichuan battery for channel {channel}: {text}", self.reach.describe());
        match super::baichuan::xml::battery_for(text, channel) {
            Some(level) => Ok(Some(level)),
            // Say what did come back: the reply is the only clue to its shape.
            None => {
                let shown: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(400).collect();
                Err(crate::api::error::Error::Protocol(format!("no level in the reply: {shown:?}")))
            }
        }
    }
}

impl Vendor for WithBcVideo {
    fn vendor_id(&self) -> &'static str {
        self.http.vendor_id()
    }
    fn connect(&mut self) -> Result<DeviceInfo> {
        let info = self.http.connect()?;
        self.channels = self.http.channels().to_vec();
        // HTTP's abilities do not always say which cameras run on a battery —
        // a hub's battery cameras have been seen reported wired. reolink_aio
        // asks Baichuan's GET_SUPPORT as well, and so does this. A device that
        // does not answer keeps what HTTP said.
        use super::baichuan::session::Calls;
        let support = self.caller().and_then(|mut caller| caller.call(super::baichuan::cmd::GET_SUPPORT, String::new(), TIMEOUT));
        match support {
            Ok(reply) if reply.response_code == 200 => {
                let text = reply.xml().unwrap_or_default();
                log::debug!("{}: baichuan abilities: {text}", self.reach.describe());
                for (index, ability) in super::baichuan::xml::channel_abilities(text) {
                    if let Some(channel) = self.channels.get_mut(index as usize) {
                        channel.battery |= ability.battery;
                    }
                }
            }
            Ok(reply) => log::info!("{}: no Baichuan abilities ({})", self.reach.describe(), reply.response_code),
            Err(err) => log::info!("{}: no Baichuan abilities ({err})", self.reach.describe()),
        }
        Ok(info)
    }
    fn logout(&self) {
        self.http.logout()
    }
    fn channels(&self) -> &[Channel] {
        if self.channels.is_empty() {
            self.http.channels()
        } else {
            &self.channels
        }
    }

    fn stream(&self, channel: &Channel, stream: StreamType) -> Result<StreamSource> {
        let mut source = self.http.stream(channel, stream)?;
        source.baichuan = Some(BaichuanVideo {
            reach: self.reach.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            channel: channel.index,
            stream,
            timeout_secs: 10,
            rtsp_fallback: Some(Arc::clone(&self.rtsp_fallback)),
        });
        Ok(source)
    }

    fn snapshot(&self, channel: &Channel) -> Result<Vec<u8>> {
        self.http.snapshot(channel)
    }
    fn battery(&self, channel: u32) -> Result<Option<BatteryLevel>> {
        // Baichuan first: it is what the device itself speaks, and an NVR that
        // does not list a camera's battery over HTTP need not answer for it
        // there either. HTTP only when Baichuan gives nothing.
        let baichuan = self.baichuan_battery(channel);
        match &baichuan {
            Ok(Some(level)) => return Ok(Some(*level)),
            Ok(None) => log::debug!("{}: Baichuan gives no battery for channel {channel}", self.reach.describe()),
            Err(err) => log::debug!("{}: no battery over Baichuan ({err})", self.reach.describe()),
        }
        match (self.http.battery(channel), baichuan) {
            (Ok(Some(level)), _) => Ok(Some(level)),
            // Say why both failed, Baichuan's reason first.
            (Err(http), Err(bc)) => Err(crate::api::error::Error::connection(format!("Baichuan: {bc}; HTTP: {http}"))),
            (Err(http), Ok(_)) => Err(crate::api::error::Error::connection(format!("Baichuan: no level; HTTP: {http}"))),
            (Ok(None), _) => Ok(None),
        }
    }
    fn detections(&self, channels: &[u32]) -> Result<Vec<(u32, Vec<(String, bool)>)>> {
        self.http.detections(channels)
    }
    fn ptz_move(&self, channel: u32, direction: &str, speed: i64) -> Result<()> {
        self.http.ptz_move(channel, direction, speed)
    }
    fn ptz_stop(&self, channel: u32) -> Result<()> {
        self.http.ptz_stop(channel)
    }
    fn ptz_presets(&self, channel: u32) -> Result<Vec<(i64, String)>> {
        self.http.ptz_presets(channel)
    }
    fn ptz_goto_preset(&self, channel: u32, preset: i64, speed: i64) -> Result<()> {
        self.http.ptz_goto_preset(channel, preset, speed)
    }
    fn ptz_go_home(&self, channel: u32) -> Result<()> {
        self.http.ptz_go_home(channel)
    }
    fn ptz_calibrate(&self, channel: u32) -> Result<()> {
        self.http.ptz_calibrate(channel)
    }
    fn white_led(&self, channel: u32) -> Result<Block> {
        self.http.white_led(channel)
    }
    fn set_floodlight(&self, block: &Block, field: &str, value: i64) -> Result<Block> {
        self.http.set_floodlight(block, field, value)
    }
    /// The Baichuan search — events with their triggers, as for a device reached
    /// by UID — over the shared connection. HTTP only if Baichuan cannot answer.
    fn search_recordings(
        &self,
        channel: u32,
        start: NaiveDateTime,
        end: NaiveDateTime,
        stream: StreamType,
    ) -> Result<Vec<Recording>> {
        let found = self.caller().and_then(|mut caller| {
            super::baichuan::playback::search_recordings(&mut caller, channel, start, end, stream, TIMEOUT)
        });
        match found {
            Ok(found) => Ok(found),
            Err(err) => {
                warn!("baichuan search on {} failed ({err}); searching over HTTP", self.reach.describe());
                self.http.search_recordings(channel, start, end, stream)
            }
        }
    }
    fn recorded_days(&self, channel: u32, month_of: NaiveDate, stream: StreamType) -> Result<Vec<u32>> {
        let found = self.caller().and_then(|mut caller| {
            super::baichuan::playback::recorded_days(&mut caller, channel, month_of, stream, TIMEOUT)
        });
        match found {
            Ok(found) => Ok(found),
            Err(err) => {
                warn!("baichuan calendar on {} failed ({err}); asking over HTTP", self.reach.describe());
                self.http.recorded_days(channel, month_of, stream)
            }
        }
    }
    fn download_url(&self, recording: &Recording) -> Result<Option<String>> {
        self.http.download_url(recording)
    }
    /// A clip the HTTP API names is copied from its URL, as before — for the main
    /// stream, which is what those files are. One it lists by time only (which
    /// newer NVR firmware does for everything, and which that API cannot serve),
    /// or the sub stream of any clip, is downloaded over Baichuan instead.
    fn fetch_recording(
        &self,
        recording: &Recording,
        out: &std::path::Path,
        progress: &mut dyn FnMut(f32) -> bool,
    ) -> Result<()> {
        // The HTTP files are the main recording; the sub stream, and anything
        // HTTP does not name, comes over Baichuan.
        if recording.is_fetchable() && recording.stream_type == crate::api::models::StreamType::Main {
            match self.http.fetch_recording(recording, out, progress) {
                Ok(()) => return Ok(()),
                Err(crate::api::error::Error::Cancelled) => return Err(crate::api::error::Error::Cancelled),
                Err(_) => {}
            }
        }
        super::baichuan::replay::fetch_to_mp4(
            &self.reach,
            &self.username,
            &self.password,
            recording.channel,
            recording.stream_type,
            recording.start,
            recording.end,
            out,
            std::time::Duration::from_secs(10),
            progress,
        )
    }

    fn recording_stream(&self, recording: &Recording) -> Option<crate::api::vendor::RecordingStream> {
        Some(crate::api::vendor::RecordingStream {
            reach: self.reach.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            channel: recording.channel,
            stream: recording.stream_type,
            start: recording.start,
            end: recording.end,
            timeout_secs: 10,
        })
    }

    fn fetches_by_time(&self) -> bool {
        true
    }

    fn supports_playback(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DeviceConfig {
        let mut c = DeviceConfig::default();
        c.vendor = "reolink".into();
        c.host = "192.0.2.50".into();
        c.rtsp_port = 554;
        c
    }

    /// A stream keeps its RTSP URL and gains a Baichuan descriptor for the BC
    /// port, sharing one fallback flag across every channel of the device.
    #[test]
    fn video_goes_over_baichuan_with_rtsp_kept_as_the_fallback() {
        let c = config();
        let vendor = WithBcVideo::new(Box::new(super::super::reolink::client_for(&c)), &c);
        let a = vendor.stream(&Channel::new(0), StreamType::Main).unwrap();
        let b = vendor.stream(&Channel::new(3), StreamType::Sub).unwrap();
        assert!(a.url.starts_with("rtsp://"), "{}", a.url);
        let bc = a.baichuan.as_ref().unwrap();
        assert_eq!(bc.reach, Reach::Tcp { host: "192.0.2.50".into(), port: 9000 });
        assert_eq!(b.baichuan.as_ref().unwrap().channel, 3);
        let (fa, fb) = (bc.rtsp_fallback.as_ref().unwrap(), b.baichuan.as_ref().unwrap().rtsp_fallback.as_ref().unwrap());
        assert!(Arc::ptr_eq(fa, fb), "one flag for the whole device");
    }

    /// The setting picks the client: on wraps the HTTP client, off is the plain
    /// HTTP client with RTSP only — the rollback.
    #[test]
    fn the_device_setting_is_the_rollback() {
        let mut c = config();
        let on = super::super::build(&c);
        assert!(on.stream(&Channel::new(0), StreamType::Main).unwrap().baichuan.is_some());
        c.baichuan_video = false;
        let off = super::super::build(&c);
        assert!(off.stream(&Channel::new(0), StreamType::Main).unwrap().baichuan.is_none());
    }

    /// A config written before the setting existed reads as on.
    #[test]
    fn older_configs_get_baichuan_video() {
        let c: DeviceConfig = serde_json::from_str(r#"{"host": "192.0.2.50"}"#).unwrap();
        assert!(c.baichuan_video);
    }
}
