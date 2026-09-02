//! Reolink as one vendor among several.
//!
//! All the work is still in [`ReolinkClient`]; this is the adapter that puts it
//! behind the shared contract. It is deliberately thin — the point of the seam
//! is that the vendor everything was written against did not have to change to
//! sit behind it.

use chrono::{NaiveDate, NaiveDateTime};

use crate::api::client::ReolinkClient;
use crate::api::error::Result;
use crate::api::models::{Channel, DeviceInfo, Recording, StreamType};
use crate::api::settings::Block;
use crate::config::DeviceConfig;

use super::{StreamSource, Vendor};

pub fn client_for(config: &DeviceConfig) -> ReolinkClient {
    ReolinkClient::with_options(
        config.host.clone(),
        config.username.clone(),
        config.password.clone(),
        config.port,
        config.https,
        config.rtsp_port,
        30,
        4,
        config.allow_self_signed,
    )
}

impl Vendor for ReolinkClient {
    fn vendor_id(&self) -> &'static str {
        "reolink"
    }

    fn connect(&mut self) -> Result<DeviceInfo> {
        ReolinkClient::connect(self)
    }

    fn logout(&self) {
        ReolinkClient::logout(self)
    }

    fn channels(&self) -> &[Channel] {
        &self.channels
    }

    fn stream(&self, channel: &Channel, stream: StreamType) -> Result<StreamSource> {
        Ok(StreamSource::new(self.rtsp_url(channel.index, stream)))
    }

    fn snapshot(&self, channel: &Channel) -> Result<Vec<u8>> {
        ReolinkClient::snapshot(self, channel.index)
    }

    fn detections(&self, channels: &[u32]) -> Result<Vec<(u32, Vec<(String, bool)>)>> {
        // Motion and AI are two calls on this API; the poller wants them as one
        // list per channel, so they are merged here rather than in the poller.
        let motion = self.motion_state(channels)?;
        let ai = self.ai_state(channels).unwrap_or_default();

        Ok(channels
            .iter()
            .map(|&channel| {
                let mut flags = vec![(
                    "motion".to_string(),
                    motion
                        .iter()
                        .find(|(c, _)| *c == channel)
                        .map(|(_, on)| *on)
                        .unwrap_or(false),
                )];
                if let Some((_, kinds)) = ai.iter().find(|(c, _)| *c == channel) {
                    flags.extend(kinds.iter().cloned());
                }
                (channel, flags)
            })
            .collect())
    }

    fn ptz_move(&self, channel: u32, direction: &str, speed: i64) -> Result<()> {
        ReolinkClient::ptz_move(self, channel, direction, speed)
    }
    fn ptz_stop(&self, channel: u32) -> Result<()> {
        ReolinkClient::ptz_stop(self, channel)
    }
    fn ptz_presets(&self, channel: u32) -> Result<Vec<(i64, String)>> {
        ReolinkClient::ptz_presets(self, channel)
    }
    fn ptz_goto_preset(&self, channel: u32, preset: i64, speed: i64) -> Result<()> {
        ReolinkClient::ptz_goto_preset(self, channel, preset, speed)
    }
    fn ptz_go_home(&self, channel: u32) -> Result<()> {
        ReolinkClient::ptz_go_home(self, channel)
    }
    fn ptz_calibrate(&self, channel: u32) -> Result<()> {
        ReolinkClient::ptz_calibrate(self, channel)
    }

    fn white_led(&self, channel: u32) -> Result<Block> {
        ReolinkClient::white_led(self, channel)
    }
    fn set_floodlight(&self, block: &Block, field: &str, value: i64) -> Result<Block> {
        ReolinkClient::set_floodlight(self, block, field, value)
    }

    fn search_recordings(
        &self,
        channel: u32,
        start: NaiveDateTime,
        end: NaiveDateTime,
        stream: StreamType,
    ) -> Result<Vec<Recording>> {
        ReolinkClient::search_recordings(self, channel, start, end, stream)
    }
    fn recorded_days(
        &self,
        channel: u32,
        month_of: NaiveDate,
        stream: StreamType,
    ) -> Result<Vec<u32>> {
        ReolinkClient::recorded_days(self, channel, month_of, stream)
    }
    fn download_url(&self, recording: &Recording) -> Result<Option<String>> {
        ReolinkClient::download_url(self, recording)
    }

    fn supports_playback(&self) -> bool {
        true
    }
}
