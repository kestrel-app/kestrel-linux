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
use crate::api::models::{Channel, DeviceInfo, Recording, StreamType};
use crate::api::settings::Block;
use crate::config::DeviceConfig;

pub struct WithBcVideo {
    http: Box<dyn Vendor>,
    reach: Reach,
    username: String,
    password: String,
    rtsp_fallback: Arc<AtomicBool>,
}

impl WithBcVideo {
    pub fn new(http: Box<dyn Vendor>, config: &DeviceConfig) -> Self {
        WithBcVideo {
            http,
            reach: Reach::Tcp { host: config.host.trim().to_string(), port: DEFAULT_BC_PORT },
            username: config.username.clone(),
            password: config.password.clone(),
            rtsp_fallback: Arc::new(AtomicBool::new(false)),
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

impl Vendor for WithBcVideo {
    fn vendor_id(&self) -> &'static str {
        self.http.vendor_id()
    }
    fn connect(&mut self) -> Result<DeviceInfo> {
        self.http.connect()
    }
    fn logout(&self) {
        self.http.logout()
    }
    fn channels(&self) -> &[Channel] {
        self.http.channels()
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
        progress: &mut dyn FnMut(f32),
    ) -> Result<()> {
        // The HTTP files are the main recording; the sub stream, and anything
        // HTTP does not name, comes over Baichuan.
        if recording.is_fetchable() && recording.stream_type == crate::api::models::StreamType::Main {
            if let Ok(()) = self.http.fetch_recording(recording, out, progress) {
                return Ok(());
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
