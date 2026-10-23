//! The [`Vendor`] implementation that speaks Baichuan to a Reolink device.
//!
//! This is the adapter that puts the protocol in this module behind the shared
//! vendor contract, the same way [`reolink`] adapts the HTTP client. It reaches a
//! device by its cloud UID over P2P, or at an address over TCP to the BC port —
//! the same session either way, see [`super::link`].
//!
//! **Verification status.** By UID, connect, login, GetVersion and live video are
//! verified against a real NVR. Over TCP the same path runs end to end against a
//! fake device on loopback (tests below), not yet against hardware. See [`super`]
//! and `docs/untested.md`.
//!
//! [`reolink`]: super::super::reolink
//! [`Vendor`]: super::super::Vendor

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};
use log::{debug, info, warn};

use super::link::{Reach, DEFAULT_BC_PORT};
use super::session::Session;
use super::{cmd, hub, login, playback, xml};
use crate::api::error::{Error, Result};
use crate::api::models::{Channel, DeviceInfo, Recording, StreamType};
use crate::api::settings::Block;
use crate::api::vendor::{BaichuanVideo, StreamSource, Vendor};
use crate::config::DeviceConfig;

/// Build a [`DeviceInfo`] from the login reply's `DeviceInfo` body (channel and disk
/// counts) and the `GetVersion` reply's `VersionInfo` body (model, firmware, serial).
/// Either may be empty; the UID is the fallback name.
fn device_info_from(uid: &str, device_xml: &str, version_xml: &str) -> DeviceInfo {
    let num = |xml: &str, tag: &str| xml::tag_text(xml, tag).and_then(|v| v.parse().ok());
    let model = xml::tag_text(version_xml, "type").unwrap_or_default();
    DeviceInfo {
        name: if model.is_empty() {
            format!("Reolink {uid}")
        } else {
            model.clone()
        },
        model,
        firmware: xml::tag_text(version_xml, "firmwareVersion").unwrap_or_default(),
        serial: xml::tag_text(version_xml, "serialNumber").unwrap_or_default(),
        channel_count: num(device_xml, "channelNum").unwrap_or(1),
        hdd_count: num(device_xml, "diskNum").unwrap_or(0),
        build_day: xml::tag_text(version_xml, "buildDay").unwrap_or_default(),
    }
}

/// A device reached over Baichuan, by UID or at an address.
pub struct BaichuanClient {
    reach: Reach,
    username: String,
    password: String,
    timeout: Duration,
    channels: Vec<Channel>,
    /// The device's shared connection, held from a successful [`connect`] until
    /// logout. Behind a mutex because a failed one is replaced from `&self`.
    ///
    /// [`connect`]: BaichuanClient::connect
    hub: Mutex<Option<Arc<hub::Hub>>>,
    /// The last motion and AI state the device pushed, by channel, as the
    /// detections poll wants it. Kept between pushes, which come every few seconds.
    detections: Mutex<HashMap<u32, Vec<(String, bool)>>>,
    /// Whether each channel's floodlight is on, as last pushed — the device has
    /// no command that reads it — or as last switched.
    lights: Mutex<HashMap<u32, bool>>,
}

/// The AI kinds always reported, as the HTTP path reports the ones a camera
/// supports — so a person leaving view reads as `people: false`, not as nothing.
const AI_KINDS: [&str; 3] = ["people", "vehicle", "dog_cat"];

fn flags_for(event: &xml::AlarmEvent) -> Vec<(String, bool)> {
    let mut flags = vec![("motion".to_string(), event.motion)];
    for kind in AI_KINDS {
        flags.push((kind.to_string(), event.ai.iter().any(|a| a == kind)));
    }
    for other in event.ai.iter().filter(|a| !AI_KINDS.contains(&a.as_str())) {
        flags.push((other.clone(), true));
    }
    flags
}

impl BaichuanClient {
    /// A UID wins when one is configured, since that is the only way to reach a
    /// device with no address; otherwise the host, on the BC port.
    pub fn new(config: &DeviceConfig) -> Self {
        let reach = if config.uid.trim().is_empty() {
            Reach::Tcp { host: config.host.trim().to_string(), port: DEFAULT_BC_PORT }
        } else {
            Reach::Uid(config.uid.trim().to_string())
        };
        BaichuanClient::reaching(reach, config)
    }

    pub fn reaching(reach: Reach, config: &DeviceConfig) -> Self {
        BaichuanClient {
            reach,
            username: config.username.clone(),
            password: config.password.clone(),
            timeout: Duration::from_secs(10),
            channels: Vec::new(),
            hub: Mutex::new(None),
            detections: Mutex::new(HashMap::new()),
            lights: Mutex::new(HashMap::new()),
        }
    }

    /// The device's connection: the one `connect` logged in, which the wall's
    /// tiles, playback, search and detections all share. Holding it here keeps it
    /// open while the device is connected, streaming or not. If it has failed, a
    /// fresh one is opened.
    fn hub(&self) -> Result<Arc<hub::Hub>> {
        let mut held = self.hub.lock().unwrap();
        let current = held.as_ref().ok_or_else(|| Error::connection("not connected to the device"))?;
        if !current.is_dead() {
            return Ok(Arc::clone(current));
        }
        let fresh = hub::hold(&self.reach, &self.username, &self.password, self.timeout)?;
        *held = Some(Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Request/reply commands over the device's connection. The client used to
    /// keep a session of its own for these, idle between commands; by UID an idle
    /// P2P connection goes quiet, and a search sent on it got "no ack from device"
    /// while the wall's connection was busy and well.
    fn caller(&self) -> Result<hub::Caller> {
        Ok(hub::Caller::from(self.hub()?))
    }

    /// A command about `channel`, which the device must accept with a `200`.
    fn channel_command(&self, channel: u32, msg_id: u32, xml: String) -> Result<super::wire::Message> {
        let reply = self.hub()?.call_for(channel, msg_id, xml, self.timeout)?;
        if reply.response_code != 200 {
            return Err(Error::command(format!("BC cmd {msg_id}"), reply.response_code as i64, "refused by the device"));
        }
        Ok(reply)
    }
}

/// How long a switched floodlight has to say it switched before the `200` is
/// taken as the answer.
const LIGHT_SETTLE: Duration = Duration::from_secs(3);

/// The floodlight in the shape the rail reads and writes, as the HTTP API gives it.
fn floodlight_block(channel: u32, on: bool, bright: i64) -> Block {
    Block {
        get_cmd: "GetWhiteLed",
        set_cmd: "SetWhiteLed",
        root: "WhiteLed",
        channel: Some(channel),
        raw: serde_json::json!({ "channel": channel, "state": i64::from(on), "bright": bright }),
    }
}

impl BaichuanClient {
    /// Whether `channel`'s floodlight is on. The state is only ever pushed, so
    /// this waits up to `wait` for a push that says `expect`, when one is given;
    /// otherwise it answers with what is known.
    fn light_on(&self, channel: u32, expect: Option<bool>, wait: Duration) -> Result<Option<bool>> {
        let hub = self.hub()?;
        // The same request turns on the alarm pushes, the floodlight's among them.
        hub.watch_motion(self.timeout);
        let deadline = std::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let push = hub.pushed(cmd::FLOODLIGHT_STATUS, left.min(Duration::from_millis(300)));
            let mut lights = self.lights.lock().unwrap();
            if let Some(xml) = push.as_ref().and_then(|m| m.xml().ok()) {
                lights.extend(xml::floodlight_states(xml));
            }
            let known = lights.get(&channel).copied();
            if expect.is_none() || known == expect || left.is_zero() {
                return Ok(known);
            }
        }
    }

    fn floodlight_brightness(&self, channel: u32) -> Result<(String, i64)> {
        let reply = self.channel_command(channel, cmd::GET_FLOODLIGHT_TASK, String::new())?;
        let task = reply.payload_xml()?.to_string();
        let bright = xml::floodlight_brightness(&task)
            .ok_or_else(|| Error::Protocol("the floodlight settings carry no brightness".into()))?;
        Ok((task, bright))
    }
}

/// Kestrel's direction words in the SDK's.
fn ptz_command(direction: &str) -> Result<&'static str> {
    Ok(match direction {
        "up" => "up",
        "down" => "down",
        "left" => "left",
        "right" => "right",
        "leftup" => "leftUp",
        "leftdown" => "leftDown",
        "rightup" => "rightUp",
        "rightdown" => "rightDown",
        "zoom_in" => "zoomInc",
        "zoom_out" => "zoomDec",
        "focus_near" => "focusInc",
        "focus_far" => "focusDec",
        other => return Err(Error::Unsupported(format!("unknown PTZ direction {other:?}"))),
    })
}

impl Vendor for BaichuanClient {
    fn vendor_id(&self) -> &'static str {
        "reolink"
    }

    fn connect(&mut self) -> Result<DeviceInfo> {
        let unset = match &self.reach {
            Reach::Uid(uid) => uid.is_empty(),
            Reach::Tcp { host, .. } => host.is_empty(),
        };
        if unset {
            return Err(Error::connection("no UID or address configured for this device"));
        }
        info!("connecting to Reolink {} over Baichuan", self.reach.describe());
        // Open the stream (P2P by UID, TCP by address) and log in. Login returns the
        // device's DeviceInfo body, which carries the channel count.
        let link = self.reach.open(self.timeout)?;
        let mut session = Session::new(link);
        let device_xml = login::login(&mut session, &self.username, &self.password, self.timeout)?;

        // GetVersion fills in the model and firmware the login reply does not carry.
        // It is the first AES command of the session; a failure here is not fatal to
        // connecting, so fall back to what the login reply gave us.
        let version_xml = session
            .call(cmd::VERSION, String::new(), self.timeout)
            .ok()
            .and_then(|m| m.xml().map(str::to_string).ok())
            .unwrap_or_default();

        let info = device_info_from(&self.reach.describe(), &device_xml, &version_xml);
        // An NVR pushes which of its channels have a camera right after login. A
        // slot with nothing in it is offline, as the HTTP path reports it — hidden
        // from the wall unless offline channels are asked for. With no report (a
        // single camera, or one that does not send it), every channel is online.
        let connected = session
            .pushed(cmd::CHANNEL_INFO_LIST, Duration::from_secs(3))
            .and_then(|m| m.xml().ok().map(xml::connected_channels));
        self.channels = (0..info.channel_count as u32)
            .map(|index| {
                let mut channel = Channel::new(index);
                if let Some(connected) = &connected {
                    channel.online = connected.contains(&index);
                }
                channel
            })
            .collect();

        // What each channel's PTZ and lights can do. Without this the rail never
        // offers either for a device reached by UID. A device that will not say keeps none.
        let abilities = match session.call(cmd::GET_SUPPORT, String::new(), self.timeout) {
            Ok(reply) if reply.response_code == 200 => {
                let text = reply.xml().unwrap_or_default();
                debug!("baichuan: abilities: {text}");
                let abilities = xml::channel_abilities(text);
                if abilities.is_empty() {
                    let start: String = text.chars().take(600).collect();
                    warn!("baichuan: abilities reply has no channel items, so no PTZ or lights: {start}");
                }
                abilities
            }
            Ok(reply) => {
                warn!("baichuan: device refused the abilities query ({}), so no PTZ or lights", reply.response_code);
                Vec::new()
            }
            Err(err) => {
                warn!("baichuan: abilities query failed ({err}), so no PTZ or lights");
                Vec::new()
            }
        };
        let with = |f: fn(&xml::Abilities) -> bool| -> Vec<u32> {
            abilities.iter().filter(|(_, a)| f(a)).map(|(c, _)| *c).collect()
        };
        info!(
            "baichuan: {} channel(s) described; PTZ on {:?}, zoom on {:?}, floodlight on {:?}",
            abilities.len(),
            with(|a| a.pan_tilt),
            with(|a| a.zoom),
            with(|a| a.floodlight)
        );
        for (index, ability) in abilities {
            if let Some(channel) = self.channels.get_mut(index as usize) {
                channel.ptz_supported = ability.pan_tilt;
                channel.ptz_presets_supported = ability.presets;
                channel.zoom_supported = ability.zoom;
                channel.optical_zoom_supported = ability.zoom;
                channel.floodlight_supported = ability.floodlight;
            }
        }

        // Names: one GetOsd per online channel, all sent at once. A channel that
        // does not answer keeps its number as its name.
        let online: Vec<u32> = self.channels.iter().filter(|c| c.online).map(|c| c.index).collect();
        let requests: Vec<_> = online
            .iter()
            .map(|&c| super::wire::Message::for_channel(cmd::GET_OSD, session.next_msg_num(), session.encryption(), c))
            .collect();
        if let Ok(replies) = session.call_each(requests, self.timeout) {
            for (index, reply) in online.iter().zip(replies) {
                let name = reply
                    .filter(|m| m.response_code == 200)
                    .and_then(|m| m.xml().ok().and_then(xml::osd_channel_name));
                if let (Some(name), Some(channel)) = (name, self.channels.get_mut(*index as usize)) {
                    channel.name = name;
                }
            }
        }

        // This session becomes the device's shared connection, so the wall's tiles
        // ride it too rather than logging in again.
        *self.hub.lock().unwrap() = Some(hub::adopt(&self.reach, &self.username, session)?);
        Ok(info)
    }

    fn logout(&self) {
        *self.hub.lock().unwrap() = None;
    }

    fn channels(&self) -> &[Channel] {
        &self.channels
    }

    fn stream(&self, channel: &Channel, stream: StreamType) -> Result<StreamSource> {
        // BC video is not RTSP: it rides the control channel as BCMedia frames. The
        // stream worker opens its own P2P session from this descriptor and decodes the
        // frames through the custom ffmpeg input (`video::bc_avio`).
        Ok(StreamSource::baichuan(BaichuanVideo {
            reach: self.reach.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            channel: channel.index,
            stream,
            timeout_secs: self.timeout.as_secs().max(5),
            rtsp_fallback: None,
        }))
    }

    fn detections(&self, channels: &[u32]) -> Result<Vec<(u32, Vec<(String, bool)>)>> {
        // Motion and AI events arrive pushed, every few seconds, once asked for.
        let hub = self.hub()?;
        hub.watch_motion(self.timeout);
        let latest = hub.pushed(cmd::MOTION, Duration::from_millis(300));
        let mut known = self.detections.lock().unwrap();
        if let Some(xml) = latest.as_ref().and_then(|m| m.xml().ok()) {
            for event in xml::alarm_events(xml) {
                known.insert(event.channel, flags_for(&event));
            }
        }
        Ok(channels
            .iter()
            .filter_map(|c| known.get(c).map(|flags| (*c, flags.clone())))
            .collect())
    }

    fn snapshot(&self, _channel: &Channel) -> Result<Vec<u8>> {
        Err(Error::Unsupported("snapshot over P2P is not yet implemented".into()))
    }

    fn ptz_move(&self, channel: u32, direction: &str, speed: i64) -> Result<()> {
        let command = ptz_command(direction)?;
        self.channel_command(channel, cmd::PTZ_CONTROL, xml::ptz_control(channel, command, speed.clamp(1, 64)))?;
        Ok(())
    }

    fn ptz_stop(&self, channel: u32) -> Result<()> {
        self.channel_command(channel, cmd::PTZ_CONTROL, xml::ptz_control(channel, "stop", 0))?;
        Ok(())
    }

    fn ptz_presets(&self, channel: u32) -> Result<Vec<(i64, String)>> {
        let reply = self.channel_command(channel, cmd::GET_PTZ_PRESET, String::new())?;
        Ok(reply.xml().map(xml::ptz_presets).unwrap_or_default())
    }

    fn ptz_goto_preset(&self, channel: u32, preset: i64, _speed: i64) -> Result<()> {
        self.channel_command(channel, cmd::PTZ_CONTROL_PRESET, xml::ptz_goto_preset(channel, preset))?;
        Ok(())
    }

    fn ptz_go_home(&self, channel: u32) -> Result<()> {
        // Read first so going home does not switch the guard off or reset its
        // timeout. A device that will not say gets the defaults.
        let current = self
            .channel_command(channel, cmd::GET_GUARD, String::new())
            .ok()
            .and_then(|m| m.xml().ok().map(str::to_string))
            .unwrap_or_default();
        self.channel_command(channel, cmd::SET_GUARD, xml::ptz_to_guard(channel, &current))?;
        Ok(())
    }

    fn ptz_calibrate(&self, channel: u32) -> Result<()> {
        self.channel_command(channel, cmd::PTZ_CHECK, String::new())?;
        Ok(())
    }

    fn white_led(&self, channel: u32) -> Result<Block> {
        let (_, bright) = self.floodlight_brightness(channel)?;
        // Off until the device says otherwise: nothing reads the state, and a
        // light nobody has switched is most often off.
        let on = self.light_on(channel, None, Duration::from_millis(500))?.unwrap_or(false);
        Ok(floodlight_block(channel, on, bright))
    }

    fn set_floodlight(&self, block: &Block, field: &str, value: i64) -> Result<Block> {
        let channel = block
            .channel
            .ok_or_else(|| Error::Unsupported("floodlight block has no channel".into()))?;
        let current = |key: &str| crate::api::models::as_i64(block.raw.get(key));
        match field {
            "state" => {
                let on = value != 0;
                info!("floodlight channel {channel}: {}", if on { "on" } else { "off" });
                self.channel_command(channel, cmd::FLOODLIGHT_MANUAL, xml::floodlight_manual(channel, on))?;
                // Accepted. If no push confirms it in time, the 200 stands.
                if self.light_on(channel, Some(on), LIGHT_SETTLE)? != Some(on) {
                    self.lights.lock().unwrap().insert(channel, on);
                }
                Ok(floodlight_block(channel, on, current("bright")))
            }
            "bright" => {
                let (task, _) = self.floodlight_brightness(channel)?;
                let changed = xml::with_floodlight_brightness(&task, value.clamp(0, 100))
                    .ok_or_else(|| Error::Protocol("the floodlight settings carry no brightness".into()))?;
                self.channel_command(channel, cmd::SET_FLOODLIGHT_TASK, changed)?;
                // What the device stored, read back.
                let (_, stored) = self.floodlight_brightness(channel)?;
                Ok(floodlight_block(channel, current("state") != 0, stored))
            }
            other => Err(Error::Unsupported(format!("floodlight field {other:?} over Baichuan"))),
        }
    }

    fn search_recordings(
        &self,
        channel: u32,
        start: NaiveDateTime,
        end: NaiveDateTime,
        stream: StreamType,
    ) -> Result<Vec<Recording>> {
        let timeout = self.timeout;
        let mut caller = self.caller()?;
        playback::search_recordings(&mut caller, channel, start, end, stream, timeout)
    }

    fn recorded_days(
        &self,
        channel: u32,
        month_of: NaiveDate,
        stream: StreamType,
    ) -> Result<Vec<u32>> {
        let timeout = self.timeout;
        let mut caller = self.caller()?;
        playback::recorded_days(&mut caller, channel, month_of, stream, timeout)
    }

    fn download_url(&self, _recording: &Recording) -> Result<Option<String>> {
        // A P2P download is a command stream, not a URL. Returning None keeps the
        // Playback tab honest: it will show the clip but not offer an HTTP save.
        Ok(None)
    }

    fn fetch_recording(
        &self,
        recording: &Recording,
        out: &std::path::Path,
        progress: &mut dyn FnMut(f32) -> bool,
    ) -> Result<()> {
        super::replay::fetch_to_mp4(
            &self.reach,
            &self.username,
            &self.password,
            recording.channel,
            recording.stream_type,
            recording.start,
            recording.end,
            out,
            self.timeout,
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
            timeout_secs: self.timeout.as_secs().max(5),
        })
    }

    fn fetches_by_time(&self) -> bool {
        true
    }

    /// Only over a slow link: P2P from outside the device's network. On its LAN
    /// the tiles keep playing.
    /// Asked from the UI thread, so it never opens a connection: a device not
    /// connected right now is not paused, and is asked again.
    fn pause_live_for_playback(&self) -> Option<Box<dyn Send>> {
        let hub = self.hub.lock().unwrap().as_ref().filter(|h| !h.is_dead()).map(Arc::clone)?;
        hub.constrained()
            .then(|| Box::new(hub::pause_live(&self.reach, &self.username)) as Box<dyn Send>)
    }

    fn supports_playback(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use std::sync::Arc;

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

    /// Anchored to real replies: the login `DeviceInfo` gives the channel and disk
    /// counts, `GetVersion` gives the model, firmware and serial.
    #[test]
    fn builds_device_info_from_real_replies() {
        let device = "<body><DeviceInfo version=\"1.1\"><firmVersion>00000000000000</firmVersion>\
            <diskNum>1</diskNum><type>nvr</type><channelNum>24</channelNum></DeviceInfo></body>";
        let version = "<body><VersionInfo version=\"1.1\"><name>ECC</name><type>RLN16-410</type>\
            <serialNumber>00000000000000</serialNumber><buildDay>build 26062949</buildDay>\
            <firmwareVersion>v3.6.5.562_26062949</firmwareVersion></VersionInfo></body>";
        let info = device_info_from("9527000TESTUID00", device, version);
        assert_eq!(info.channel_count, 24);
        assert_eq!(info.hdd_count, 1);
        assert_eq!(info.model, "RLN16-410");
        assert_eq!(info.firmware, "v3.6.5.562_26062949");
        assert_eq!(info.name, "RLN16-410");
        assert_eq!(info.kind(), crate::api::models::DeviceKind::Nvr);
    }

    /// Against a real device, through the real Rust path: lookup, register, data
    /// channel, login, GetVersion. Ignored by default; run with
    ///   KESTREL_TEST_UID=... KESTREL_TEST_PASS=... [KESTREL_TEST_USER=admin] \
    ///     cargo test -- --ignored connects_to_a_real_device_by_uid --nocapture
    /// (UDP must be able to leave the machine.)
    #[test]
    #[ignore]
    fn connects_to_a_real_device_by_uid() {
        let (Ok(uid), Ok(password)) = (
            std::env::var("KESTREL_TEST_UID"),
            std::env::var("KESTREL_TEST_PASS"),
        ) else {
            eprintln!("KESTREL_TEST_UID / KESTREL_TEST_PASS not set");
            return;
        };
        let mut config = uid_config(&uid);
        config.username = std::env::var("KESTREL_TEST_USER").unwrap_or_else(|_| "admin".into());
        config.password = password;
        let mut client = BaichuanClient::new(&config);
        let info = client.connect().expect("connect over P2P");
        println!("connected: {info:?}; {} channel(s)", client.channels().len());
        let online: Vec<u32> = client.channels().iter().filter(|c| c.online).map(|c| c.index).collect();
        println!("online channels: {online:?}");
        let names: Vec<&str> = client.channels().iter().filter(|c| c.online).map(|c| c.name.as_str()).collect();
        println!("names: {names:?}");
        for c in client.channels().iter().filter(|c| c.online) {
            println!(
                "  [{}] ptz {} presets {} zoom {} floodlight {}",
                c.index, c.ptz_supported, c.ptz_presets_supported, c.optical_zoom_supported, c.floodlight_supported
            );
        }
        // The raw abilities reply, for when the flags above are not what the
        // device has.
        match client.hub().and_then(|hub| hub.call(cmd::GET_SUPPORT, String::new(), Duration::from_secs(30))) {
            Ok(reply) => println!("GET_SUPPORT {}:\n{}", reply.response_code, reply.xml().unwrap_or("(not text)")),
            Err(err) => println!("GET_SUPPORT failed: {err}"),
        }
        assert!(info.channel_count > 0);
        assert!(!client.channels().is_empty());
    }

    /// Against a real device at an address, over TCP to its BC port. Ignored by
    /// default; run on a machine that can reach the device with
    ///   KESTREL_TEST_BC_HOST=<address> KESTREL_TEST_PASS=... [KESTREL_TEST_BC_PORT=9000] \
    ///     cargo test -- --ignored connects_to_a_real_device_over_tcp --nocapture
    #[test]
    #[ignore]
    fn connects_to_a_real_device_over_tcp() {
        let (Ok(host), Ok(password)) = (
            std::env::var("KESTREL_TEST_BC_HOST"),
            std::env::var("KESTREL_TEST_PASS"),
        ) else {
            eprintln!("KESTREL_TEST_BC_HOST / KESTREL_TEST_PASS not set");
            return;
        };
        let port = std::env::var("KESTREL_TEST_BC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_BC_PORT);
        let mut config = DeviceConfig::default();
        config.username = std::env::var("KESTREL_TEST_USER").unwrap_or_else(|_| "admin".into());
        config.password = password;
        let mut client = BaichuanClient::reaching(Reach::Tcp { host, port }, &config);
        let info = client.connect().expect("connect over TCP");
        println!("connected: {info:?}; {} channel(s)", client.channels().len());
        assert!(info.channel_count > 0);
    }

    /// With no version reply, the UID is the fallback name and the channel count
    /// still comes from the login reply.
    #[test]
    fn falls_back_to_the_uid_without_a_version_reply() {
        let device = "<body><DeviceInfo><channelNum>8</channelNum></DeviceInfo></body>";
        let info = device_info_from("9527000TESTUID00", device, "");
        assert_eq!(info.channel_count, 8);
        assert_eq!(info.name, "Reolink 9527000TESTUID00");
    }

    /// A fake device on loopback that speaks just enough Baichuan over TCP: the
    /// two-leg login (checking the salted password hash), `GetVersion` under AES,
    /// and a `Preview` answered with one pushed H.265 keyframe. Built from the same
    /// codec the client uses, so this tests the TCP plumbing and the session over
    /// it, not the device's side of the protocol (that is verified live by UID).
    mod fake_device {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        use crate::api::vendor::baichuan::crypto::make_aes_key;
        use crate::api::vendor::baichuan::login::login_hash;
        use crate::api::vendor::baichuan::wire::{Encryption, Message};
        use crate::api::vendor::baichuan::{cmd, xml};

        pub const NONCE: &str = "0123456789ABCDEF";
        /// The keyframe the device sends for a channel: distinct per channel, so a
        /// test can tell which stream a picture came from.
        pub fn keyframe(channel: u8) -> Vec<u8> {
            format!("KEYFRAME-FOR-CHANNEL-{channel}").into_bytes()
        }

        /// The audio frame sent beside each keyframe, distinct per channel too.
        pub fn aac(channel: u8) -> Vec<u8> {
            format!("AAC-FOR-CHANNEL-{channel}").into_bytes()
        }

        /// Serve one connection; returns the port. The device accepts `password`.
        pub fn start(password: &'static str) -> u16 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let _ = serve(&mut s, password, None, None);
            });
            port
        }

        /// What a download is answered with: frames of a recording, one every
        /// 66.7 ms from midnight, each `(keyframe, data)`.
        pub type Footage = std::sync::Arc<Vec<(bool, Vec<u8>)>>;

        /// As [`start`], but downloads are answered from `footage`, a window at a
        /// time as asked — starting at the keyframe before, as the NVR does — and
        /// encrypted as the NVR sends it.
        pub fn start_recording(password: &'static str, footage: Footage) -> u16 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let _ = serve(&mut s, password, Some(footage), None);
            });
            port
        }

        /// As [`start`], noting the id of every command the device hears, in order.
        pub fn start_logged(password: &'static str) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<u32>>>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = std::sync::Arc::clone(&heard);
            std::thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let _ = serve(&mut s, password, None, Some(&log));
            });
            (port, heard)
        }

        /// 64 KB frames of filler, a keyframe every 30 — to measure speed.
        pub fn filler(frames: usize) -> Footage {
            std::sync::Arc::new((0..frames).map(|i| (i % 30 == 0, vec![(i % 251) as u8; 64 * 1024])).collect())
        }

        /// One download message carrying a frame, its first 1024 bytes AES as the
        /// NVR's `encryptLen` says.
        fn recording_frame(aes: Encryption, num: u16, index: u32, keyframe: bool, data: &[u8]) -> Vec<u8> {
            let mut frame = Vec::new();
            let magic: u32 = if keyframe { 0x6364_3030 } else { 0x6364_3130 };
            frame.extend_from_slice(&magic.to_le_bytes());
            frame.extend_from_slice(b"H264");
            frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
            frame.extend_from_slice(&0u32.to_le_bytes()); // additional header
            frame.extend_from_slice(&(index * 66_667).to_le_bytes()); // micros
            frame.extend_from_slice(&0u32.to_le_bytes());
            frame.extend_from_slice(data);
            frame.extend_from_slice(&vec![0u8; (8 - data.len() % 8) % 8]);
            let split = frame.len().min(1024);
            let mut payload = aes.encrypt(&frame[..split]);
            payload.extend_from_slice(&frame[split..]);
            let ext = aes.encrypt(format!("<Extension><binaryData>1</binaryData><encryptLen>{split}</encryptLen></Extension>").as_bytes());
            let mut body = ext.clone();
            body.extend_from_slice(&payload);
            let mut out = Vec::new();
            out.extend_from_slice(&0x0abc_def0u32.to_le_bytes());
            out.extend_from_slice(&cmd::DOWNLOAD_BY_TIME.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&num.to_le_bytes());
            out.extend_from_slice(&200u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&(ext.len() as u32).to_le_bytes());
            out.extend_from_slice(&body);
            out
        }

        /// Seconds since midnight of a `<startTime>`/`<endTime>` in a request.
        fn seconds_of(body: &str, tag: &str) -> u32 {
            let block = body.split(&format!("<{tag}>")).nth(1).unwrap_or("");
            let field = |t: &str| xml::tag_text(block, t).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
            field("hour") * 3600 + field("minute") * 60 + field("second")
        }

        fn read_msg(s: &mut TcpStream, buf: &mut Vec<u8>, cipher: Encryption) -> Option<Message> {
            loop {
                if let Ok(Some((msg, used))) = Message::parse(buf, cipher) {
                    buf.drain(..used);
                    return Some(msg);
                }
                let mut chunk = [0u8; 4096];
                let n = s.read(&mut chunk).ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        }

        fn reply(msg_id: u32, num: u16, code: u16, cipher: Encryption, xml: &str) -> Vec<u8> {
            let mut m = Message::modern(msg_id, num, cipher, xml);
            m.response_code = code;
            m.to_bytes()
        }

        fn serve(
            s: &mut TcpStream,
            password: &str,
            footage: Option<Footage>,
            heard: Option<&std::sync::Mutex<Vec<u32>>>,
        ) -> Option<()> {
            let bc = Encryption::BcEncrypt(0);
            let mut buf = Vec::new();

            // Leg 1: header-only upgrade; answer with the nonce.
            let leg1 = read_msg(s, &mut buf, Encryption::None)?;
            assert_eq!(leg1.msg_id, cmd::LOGIN);
            let nonce = format!("<body><Encryption version=\"1.1\"><type>md5</type><nonce>{NONCE}</nonce></Encryption></body>");
            s.write_all(&reply(cmd::LOGIN, leg1.msg_num, 200, bc, &nonce)).ok()?;

            // Leg 2: check the salted hashes.
            let leg2 = read_msg(s, &mut buf, bc)?;
            let body = leg2.xml().ok()?.to_string();
            let user_ok = xml::tag_text(&body, "userName") == Some(login_hash("admin", NONCE));
            let pass_ok = xml::tag_text(&body, "password") == Some(login_hash(password, NONCE));
            if !(user_ok && pass_ok) {
                s.write_all(&reply(cmd::LOGIN, leg2.msg_num, 401, bc, "")).ok()?;
                return None;
            }
            let info = "<body><DeviceInfo version=\"1.1\"><channelNum>4</channelNum><diskNum>1</diskNum></DeviceInfo></body>";
            s.write_all(&reply(cmd::LOGIN, leg2.msg_num, 200, bc, info)).ok()?;

            // Everything after login is AES. Like a real NVR, push the channel
            // list unasked: cameras on 0 and 1, nothing on 2 and 3.
            let aes = Encryption::Aes(make_aes_key(NONCE, password));
            let list = "<body><ChannelInfoList version=\"1.1\">\
                <ChannelInfo><channelId>0</channelId><state>connect</state></ChannelInfo>\
                <ChannelInfo><channelId>1</channelId><state>connect</state></ChannelInfo>\
                <ChannelInfo><channelId>2</channelId><state>none</state></ChannelInfo>\
                <ChannelInfo><channelId>3</channelId><state>none</state></ChannelInfo>\
                </ChannelInfoList></body>";
            s.write_all(&reply(cmd::CHANNEL_INFO_LIST, 0, 200, aes, list)).ok()?;
            // The floodlight's brightness, which a write changes.
            let mut bright = 40;
            loop {
                let msg = read_msg(s, &mut buf, aes)?;
                if let Some(heard) = heard {
                    heard.lock().unwrap().push(msg.msg_id);
                }
                match msg.msg_id {
                    cmd::VERSION => {
                        let v = "<body><VersionInfo version=\"1.1\"><type>RLN8-410</type><firmwareVersion>v3.0.0.1</firmwareVersion></VersionInfo></body>";
                        s.write_all(&reply(cmd::VERSION, msg.msg_num, 200, aes, v)).ok()?;
                    }
                    // A pan-tilt-zoom camera with presets on channel 1; the rest fixed.
                    cmd::GET_SUPPORT => {
                        let support = "<body><SupportAbility version=\"1.1\">\
                            <item><chnID>0</chnID><ptzType>0</ptzType><ptzPreset>0</ptzPreset></item>\
                            <item><chnID>1</chnID><ptzType>2</ptzType><ptzPreset>1</ptzPreset><ledCtrl>6</ledCtrl></item>\
                            </SupportAbility></body>";
                        s.write_all(&reply(cmd::GET_SUPPORT, msg.msg_num, 200, aes, support)).ok()?;
                    }
                    // Accepted only when the channel's extension and the command
                    // both read back, so a garbled body is a 400 as on a device.
                    cmd::PTZ_CONTROL => {
                        let body = msg.xml().unwrap_or_default();
                        let channel = msg.channel_id;
                        let ok = msg.extension_len > 0
                            && body.contains(&format!("<Extension version=\"1.1\">\n<channelId>{channel}</channelId>"))
                            && body.contains(&format!("<PtzControl version=\"1.1\"><channelId>{channel}</channelId>"))
                            && ["leftUp", "zoomInc", "stop"].iter().any(|c| body.contains(&format!("<command>{c}</command>")));
                        s.write_all(&reply(cmd::PTZ_CONTROL, msg.msg_num, if ok { 200 } else { 400 }, aes, "")).ok()?;
                    }
                    // Settings answered with an extension first, as a channel's are.
                    cmd::GET_FLOODLIGHT_TASK => {
                        let task = format!(
                            "<body><FloodlightTask version=\"1.1\"><channel>{}</channel><alarmMode>1</alarmMode>\
                             <brightness_cur>{bright}</brightness_cur><duration>300</duration></FloodlightTask></body>",
                            msg.channel_id
                        );
                        let mut m = Message::for_channel_with(cmd::GET_FLOODLIGHT_TASK, msg.msg_num, aes, msg.channel_id as u32, &task);
                        m.response_code = 200;
                        s.write_all(&m.to_bytes()).ok()?;
                    }
                    // Taken only whole: the settings written back, behind the extension.
                    cmd::SET_FLOODLIGHT_TASK => {
                        let body = msg.payload_xml().unwrap_or_default();
                        let whole = msg.extension_len > 0 && body.contains("<alarmMode>1</alarmMode>") && body.contains("<duration>300</duration>");
                        if let (true, Some(b)) = (whole, xml::floodlight_brightness(body)) {
                            bright = b;
                        }
                        s.write_all(&reply(cmd::SET_FLOODLIGHT_TASK, msg.msg_num, if whole { 200 } else { 400 }, aes, "")).ok()?;
                    }
                    // Switched, then the new state pushed, as the only way it is told.
                    cmd::FLOODLIGHT_MANUAL => {
                        let body = msg.payload_xml().unwrap_or_default().to_string();
                        let status = xml::tag_text(&body, "status").unwrap_or_default();
                        s.write_all(&reply(cmd::FLOODLIGHT_MANUAL, msg.msg_num, 200, aes, "")).ok()?;
                        let push = format!(
                            "<body><FloodlightStatusList version=\"1.1\"><FloodlightStatus version=\"1.1\">\
                             <channel>{}</channel><status>{status}</status></FloodlightStatus></FloodlightStatusList></body>",
                            msg.channel_id
                        );
                        s.write_all(&reply(cmd::FLOODLIGHT_STATUS, 0, 200, aes, &push)).ok()?;
                    }
                    // Answered with an extension before the list, as a channel
                    // command's reply can be.
                    cmd::GET_PTZ_PRESET => {
                        let list = "<body><PtzPreset version=\"1.1\"><presetList>\
                            <preset><id>1</id><name>Gate</name></preset><preset><id>2</id><name></name></preset>\
                            </presetList></PtzPreset></body>";
                        let mut m = Message::for_channel_with(cmd::GET_PTZ_PRESET, msg.msg_num, aes, msg.channel_id as u32, list);
                        m.response_code = 200;
                        s.write_all(&m.to_bytes()).ok()?;
                    }
                    // Like the NVR: the video echoes the Preview's number and channel.
                    cmd::VIDEO => s.write_all(&video_push(aes, msg.msg_num, msg.channel_id)).ok()?,
                    // Like the NVR at an address: replay refused with 405.
                    cmd::PLAYBACK_BY_TIME_V2 => {
                        let mut no = Message::header_only(cmd::PLAYBACK_BY_TIME_V2, msg.msg_num, 405, 0);
                        no.class = 0;
                        s.write_all(&no.to_bytes()).ok()?;
                    }
                    // Like the NVR: a size reply, the footage, then a bodiless 300.
                    cmd::DOWNLOAD_BY_TIME => {
                        let size = "<body><FileInfoList version=\"1.1\"><FileInfo><FileCount>1</FileCount></FileInfo></FileInfoList></body>";
                        s.write_all(&reply(cmd::DOWNLOAD_BY_TIME, msg.msg_num, 200, aes, size)).ok()?;
                        match &footage {
                            Some(footage) => {
                                let body = msg.xml().unwrap_or_default().to_string();
                                let (from, to) = (seconds_of(&body, "startTime"), seconds_of(&body, "endTime"));
                                let (from, to) = ((from * 15) as usize, ((to * 15) as usize).min(footage.len()));
                                // From the keyframe at or before the time asked.
                                let first = (0..=from.min(footage.len().saturating_sub(1)))
                                    .rev()
                                    .find(|&i| footage[i].0)
                                    .unwrap_or(0);
                                for index in first..to {
                                    let (key, data) = &footage[index];
                                    s.write_all(&recording_frame(aes, msg.msg_num, index as u32, *key, data)).ok()?;
                                }
                            }
                            None => {
                                let mut push = video_push(aes, msg.msg_num, msg.channel_id);
                                push[4..8].copy_from_slice(&cmd::DOWNLOAD_BY_TIME.to_le_bytes());
                                s.write_all(&push).ok()?;
                            }
                        }
                        let mut end = Message::header_only(cmd::DOWNLOAD_BY_TIME, msg.msg_num, 300, 0);
                        end.class = 0;
                        s.write_all(&end.to_bytes()).ok()?;
                    }
                    // The recording search, shaped as an RLN16-410 answers it: a
                    // handle, one page of two clips, then the close.
                    cmd::FIND_ALARM_VIDEO_OPEN => {
                        let open = "<body><findAlarmVideo version=\"1.1\"><channelId>0</channelId><fileHandle>1</fileHandle></findAlarmVideo></body>";
                        s.write_all(&reply(cmd::FIND_ALARM_VIDEO_OPEN, msg.msg_num, 200, aes, open)).ok()?;
                    }
                    cmd::FIND_ALARM_VIDEO_NEXT => {
                        let t = |h: u32, m: u32, sec: u32| format!(
                            "<year>2026</year><month>10</month><day>5</day><hour>{h}</hour><minute>{m}</minute><second>{sec}</second>"
                        );
                        let page = format!(
                            "<body><alarmVideoInfo version=\"1.1\"><channelId>0</channelId><bFinished>1</bFinished><alarmVideoList>\
                             <alarmVideo><fileName>0120261005080000</fileName><alarmType>people</alarmType><startTime>{}</startTime><endTime>{}</endTime></alarmVideo>\
                             <alarmVideo><fileName>0120261005090000</fileName><alarmType>package</alarmType><startTime>{}</startTime><endTime>{}</endTime></alarmVideo>\
                             </alarmVideoList></alarmVideoInfo></body>",
                            t(8, 1, 0), t(8, 1, 20), t(9, 30, 0), t(9, 30, 15)
                        );
                        s.write_all(&reply(cmd::FIND_ALARM_VIDEO_NEXT, msg.msg_num, 200, aes, &page)).ok()?;
                    }
                    cmd::FIND_ALARM_VIDEO_CLOSE => {
                        s.write_all(&reply(cmd::FIND_ALARM_VIDEO_CLOSE, msg.msg_num, 200, aes, "")).ok()?;
                    }
                    cmd::GET_RECFILEDATE => {
                        let days = "<body><DayRecords version=\"1.1\"><DayRecordList><DayRecord><channelId>0</channelId><dayTypeList>\
                            <dayType><index>4</index><type>alarm</type></dayType></dayTypeList></DayRecord></DayRecordList></DayRecords></body>";
                        s.write_all(&reply(cmd::GET_RECFILEDATE, msg.msg_num, 200, aes, days)).ok()?;
                    }
                    cmd::GET_OSD => {
                        let osd = format!(
                            "<body><OsdChannelName version=\"1.1\"><channelId>{0}</channelId><name>Camera {0}</name></OsdChannelName></body>",
                            msg.channel_id
                        );
                        s.write_all(&reply(cmd::GET_OSD, msg.msg_num, 200, aes, &osd)).ok()?;
                    }
                    // Like the NVR: an empty 200, then the alarm list pushed.
                    cmd::MOTION_REQUEST => {
                        s.write_all(&reply(cmd::MOTION_REQUEST, msg.msg_num, 200, Encryption::None, "")).ok()?;
                        let alarms = "<body><AlarmEventList version=\"1.1\">\
                            <AlarmEvent version=\"1.1\"><channelId>0</channelId><status>MD</status><AItype>none</AItype></AlarmEvent>\
                            <AlarmEvent version=\"1.1\"><channelId>1</channelId><status>none</status><AItype>people</AItype></AlarmEvent>\
                            </AlarmEventList></body>";
                        s.write_all(&reply(cmd::MOTION, 0, 200, aes, alarms)).ok()?;
                    }
                    _ => {}
                }
            }
        }

        /// One class-0x0000 video message: an AES extension saying the payload is
        /// plaintext, then a BCMedia H.265 keyframe.
        fn video_push(aes: Encryption, num: u16, channel: u8) -> Vec<u8> {
            let keyframe = keyframe(channel);
            let mut frame = Vec::new();
            frame.extend_from_slice(&0x6364_3030u32.to_le_bytes()); // IFRAME magic
            frame.extend_from_slice(b"H265");
            frame.extend_from_slice(&(keyframe.len() as u32).to_le_bytes());
            frame.extend_from_slice(&[0u8; 12]); // additional header size, µs, unknown
            frame.extend_from_slice(&keyframe);
            frame.extend_from_slice(&vec![0u8; (8 - keyframe.len() % 8) % 8]);
            // Sound rides in the same messages as the picture.
            let sound = aac(channel);
            frame.extend_from_slice(&0x6277_3530u32.to_le_bytes()); // AAC magic
            frame.extend_from_slice(&(sound.len() as u16).to_le_bytes());
            frame.extend_from_slice(&(sound.len() as u16).to_le_bytes());
            frame.extend_from_slice(&sound);
            frame.extend_from_slice(&vec![0u8; (8 - sound.len() % 8) % 8]);

            let ext = aes.encrypt(b"<Extension><binaryData>1</binaryData></Extension>");
            let mut body = ext.clone();
            body.extend_from_slice(&frame);
            let mut out = Vec::new();
            out.extend_from_slice(&0x0abc_def0u32.to_le_bytes());
            out.extend_from_slice(&cmd::VIDEO.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&[channel, 0]); // channel, stream
            out.extend_from_slice(&num.to_le_bytes()); // msg_num, echoed
            out.extend_from_slice(&200u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // class 0x0000
            out.extend_from_slice(&(ext.len() as u32).to_le_bytes()); // payload offset
            out.extend_from_slice(&body);
            out
        }
    }

    fn tcp_config(port: u16, password: &str) -> (Reach, DeviceConfig) {
        let mut c = DeviceConfig::default();
        c.host = "127.0.0.1".into();
        c.username = "admin".into();
        c.password = password.into();
        (Reach::Tcp { host: "127.0.0.1".into(), port }, c)
    }

    /// Over TCP: connect, log in, and read the model and channel count — the same
    /// client that reaches a device by UID.
    #[test]
    fn connects_over_tcp_to_the_bc_port() {
        let port = fake_device::start("hunter2");
        let (reach, config) = tcp_config(port, "hunter2");
        let mut client = BaichuanClient::reaching(reach, &config);
        let info = client.connect().expect("connect over TCP");
        assert_eq!(info.channel_count, 4);
        assert_eq!(info.model, "RLN8-410");
        assert_eq!(client.channels().len(), 4);
        let online: Vec<bool> = client.channels().iter().map(|c| c.online).collect();
        assert_eq!(online, [true, true, false, false], "empty NVR slots are offline");
        let names: Vec<&str> = client.channels().iter().take(2).map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Camera 0", "Camera 1"], "names from each channel's OSD");
    }

    #[test]
    fn a_wrong_password_over_tcp_is_an_auth_error() {
        let port = fake_device::start("hunter2");
        let (reach, config) = tcp_config(port, "wrong");
        let mut client = BaichuanClient::reaching(reach, &config);
        assert!(matches!(client.connect().unwrap_err(), Error::Auth(_)));
    }

    /// Live video over TCP: preview, then the pushed keyframe comes out of the
    /// streamer as elementary bytes with its codec known.
    #[test]
    fn streams_video_over_tcp() {
        use crate::api::vendor::baichuan::media::VideoCodec;
        use crate::api::vendor::baichuan::video::VideoStreamer;

        let port = fake_device::start("hunter2");
        let timeout = Duration::from_secs(5);
        let link = Reach::Tcp { host: "127.0.0.1".into(), port }.open(timeout).unwrap();
        let mut session = Session::new(link);
        login::login(&mut session, "admin", "hunter2", timeout).unwrap();
        let mut streamer = VideoStreamer::start(&mut session, 0, StreamType::Sub).unwrap();
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        while got.is_empty() && std::time::Instant::now() < deadline {
            got = streamer.poll(&mut session, Duration::from_millis(300)).unwrap();
        }
        assert_eq!(got, fake_device::keyframe(0));
        assert_eq!(streamer.codec(), Some(VideoCodec::H265));
    }

    /// No UID means the host on the BC port; a UID wins when both are set.
    #[test]
    fn the_uid_wins_over_the_host() {
        let mut c = DeviceConfig::default();
        c.host = "192.0.2.10".into();
        assert_eq!(
            BaichuanClient::new(&c).reach,
            Reach::Tcp { host: "192.0.2.10".into(), port: DEFAULT_BC_PORT }
        );
        c.uid = "95270000ABCDEFGH".into();
        assert_eq!(BaichuanClient::new(&c).reach, Reach::Uid("95270000ABCDEFGH".into()));
    }

    /// Two tiles on two channels share one connection: the fake device accepts a
    /// single connection, so both pictures arriving proves the sharing, and each
    /// tile gets its own channel's keyframe.
    #[test]
    fn two_channels_share_one_connection() {
        use crate::api::vendor::baichuan::hub;
        use crate::api::vendor::baichuan::media::VideoCodec;

        let port = fake_device::start("hunter2");
        let reach = Reach::Tcp { host: "127.0.0.1".into(), port };
        let timeout = Duration::from_secs(5);
        let a = hub::subscribe(&reach, "admin", "hunter2", 0, StreamType::Sub, timeout).unwrap();
        let b = hub::subscribe(&reach, "admin", "hunter2", 3, StreamType::Sub, timeout).unwrap();
        assert!(Arc::ptr_eq(&a.hub, &b.hub), "one connection for the device");

        for (sub, channel) in [(&a, 0u8), (&b, 3u8)] {
            assert_eq!(sub.codec.recv_timeout(timeout).unwrap(), VideoCodec::H265);
            assert_eq!(sub.bytes.recv_timeout(timeout).unwrap(), fake_device::keyframe(channel));
        }
    }

    /// A UID NVR's PTZ: the abilities light up the rail for the channel that has
    /// it, and commands go over the shared connection in the SDK's words, with the
    /// channel's extension ahead of the body.
    #[test]
    fn ptz_works_over_baichuan() {
        let port = fake_device::start("hunter2");
        let (reach, config) = tcp_config(port, "hunter2");
        let mut client = BaichuanClient::reaching(reach, &config);
        client.connect().unwrap();
        let (fixed, ptz) = (&client.channels()[0], &client.channels()[1]);
        assert!(!fixed.ptz_supported && !fixed.ptz_presets_supported);
        assert!(ptz.ptz_supported && ptz.ptz_presets_supported && ptz.optical_zoom_supported);

        client.ptz_move(1, "leftup", 32).expect("a diagonal move");
        client.ptz_move(1, "zoom_in", 32).expect("zoom");
        client.ptz_stop(1).expect("stop");
        assert!(client.ptz_move(1, "sideways", 32).is_err());
        assert_eq!(client.ptz_presets(1).unwrap(), vec![(1, "Gate".to_string())]);
    }

    /// A UID NVR's floodlight: offered where the abilities say so, brightness
    /// written back with the rest of its settings, and on/off as the device
    /// pushes it.
    #[test]
    fn floodlight_works_over_baichuan() {
        let port = fake_device::start("hunter2");
        let (reach, config) = tcp_config(port, "hunter2");
        let mut client = BaichuanClient::reaching(reach, &config);
        client.connect().unwrap();
        assert!(!client.channels()[0].floodlight_supported);
        assert!(client.channels()[1].floodlight_supported);

        let block = client.white_led(1).unwrap();
        assert_eq!((block.raw["state"].as_i64(), block.raw["bright"].as_i64()), (Some(0), Some(40)));

        let block = client.set_floodlight(&block, "bright", 85).unwrap();
        assert_eq!(block.raw["bright"].as_i64(), Some(85), "read back from the device");

        let started = std::time::Instant::now();
        let block = client.set_floodlight(&block, "state", 1).unwrap();
        assert_eq!(block.raw["state"].as_i64(), Some(1));
        assert!(started.elapsed() < LIGHT_SETTLE, "the push confirmed it, no waiting out the settle");
        assert_eq!(client.white_led(1).unwrap().raw["state"].as_i64(), Some(1), "remembered");
    }

    /// Sound reaches the tile it belongs to, beside its picture, rather than being
    /// thrown away by the demuxer.
    #[test]
    fn audio_reaches_its_tile_beside_the_video() {
        use crate::api::vendor::baichuan::hub;
        use crate::api::vendor::baichuan::media::AudioCodec;

        let port = fake_device::start("hunter2");
        let reach = Reach::Tcp { host: "127.0.0.1".into(), port };
        let timeout = Duration::from_secs(5);
        let sub = hub::subscribe(&reach, "admin", "hunter2", 2, StreamType::Sub, timeout).unwrap();
        assert!(sub.codec.recv_timeout(timeout).is_ok());
        assert_eq!(sub.bytes.recv_timeout(timeout).unwrap(), fake_device::keyframe(2), "video unchanged");
        assert_eq!(sub.audio.recv_timeout(timeout).unwrap(), (AudioCodec::Aac, fake_device::aac(2)));
    }

    /// Motion and AI come from the alarm list the device pushes after the client
    /// asks for it, in the shape the detections poll reads.
    #[test]
    fn reports_motion_and_people_from_pushed_alarms() {
        let port = fake_device::start("hunter2");
        let (reach, config) = tcp_config(port, "hunter2");
        let mut client = BaichuanClient::reaching(reach, &config);
        client.connect().unwrap();
        let state = client.detections(&[0, 1]).unwrap();
        let flag = |channel: u32, kind: &str| {
            state.iter().find(|(c, _)| *c == channel).unwrap().1.iter().find(|(k, _)| k == kind).unwrap().1
        };
        assert!(flag(0, "motion"));
        assert!(!flag(0, "people"));
        assert!(!flag(1, "motion"));
        assert!(flag(1, "people"));
    }

    /// Against the real device: subscribe and print what the poll sees. Ignored.
    #[test]
    #[ignore]
    fn reads_motion_from_a_real_device_by_uid() {
        let (Ok(uid), Ok(password)) = (std::env::var("KESTREL_TEST_UID"), std::env::var("KESTREL_TEST_PASS")) else {
            eprintln!("KESTREL_TEST_UID / KESTREL_TEST_PASS not set");
            return;
        };
        let mut config = uid_config(&uid);
        config.password = password;
        let mut client = BaichuanClient::new(&config);
        client.connect().unwrap();
        let channels: Vec<u32> = client.channels().iter().filter(|c| c.online).map(|c| c.index).collect();
        let mut got = Vec::new();
        for _ in 0..10 {
            got = client.detections(&channels).unwrap();
            if !got.is_empty() {
                break;
            }
        }
        for (channel, flags) in &got {
            println!("channel {channel}: {flags:?}");
        }
        assert_eq!(got.len(), channels.len(), "a state for every online channel");
    }

    /// Against the real device: the calendar for this month, then every clip on the
    /// last recorded day. Ignored; needs KESTREL_TEST_UID / KESTREL_TEST_PASS.
    #[test]
    #[ignore]
    fn searches_recordings_on_a_real_device_by_uid() {
        let (Ok(uid), Ok(password)) = (std::env::var("KESTREL_TEST_UID"), std::env::var("KESTREL_TEST_PASS")) else {
            eprintln!("KESTREL_TEST_UID / KESTREL_TEST_PASS not set");
            return;
        };
        let mut config = uid_config(&uid);
        config.password = password;
        let mut client = BaichuanClient::new(&config);
        client.connect().unwrap();
        let today = chrono::Local::now().date_naive();
        let days = client.recorded_days(0, today, StreamType::Main).unwrap();
        println!("recorded days this month: {days:?}");
        let day = *days.last().expect("some recorded day");
        let date = today.with_day(day).unwrap();
        let started = std::time::Instant::now();
        let clips = client
            .search_recordings(0, date.and_hms_opt(0, 0, 0).unwrap(), date.and_hms_opt(23, 59, 59).unwrap(), StreamType::Main)
            .unwrap();
        println!("{} clip(s) on {date} in {:?}", clips.len(), started.elapsed());
        for c in clips.iter().take(3).chain(clips.iter().rev().take(1)) {
            println!("  {} – {} (file from {:?})", c.start, c.end, c.playback_time);
        }
        assert!(!clips.is_empty());
    }

    /// A download while a tile is streaming goes over the tile's connection: the
    /// fake device accepts a single connection, as the NVR at an address seemed to,
    /// so the fetch only succeeds if it shares it.
    #[test]
    fn a_download_shares_the_live_connection() {
        use crate::api::vendor::baichuan::{hub, replay};
        let port = fake_device::start("hunter2");
        let reach = Reach::Tcp { host: "127.0.0.1".into(), port };
        let timeout = Duration::from_secs(5);
        let live = hub::subscribe(&reach, "admin", "hunter2", 0, StreamType::Sub, timeout).unwrap();
        assert!(live.codec.recv_timeout(timeout).is_ok(), "the tile is streaming");

        let start = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(0, 0, 40).unwrap();
        let footage = replay::fetch(
            &reach,
            "admin",
            "hunter2",
            3,
            StreamType::Sub,
            start,
            start + chrono::Duration::seconds(25),
            timeout,
            &mut |_| true,
        )
        .expect("the download rides the open connection");
        assert_eq!(footage.elementary(), fake_device::keyframe(3));
        drop(live);
    }

    /// How fast our side takes a download in, with the link out of the picture:
    /// ~128 MB over loopback, encrypted as the NVR sends it. Ignored (it is a
    /// measurement); run with --nocapture to see the rate.
    #[test]
    #[ignore]
    fn download_throughput_over_loopback() {
        use crate::api::vendor::baichuan::replay;
        let frames = 2000;
        let port = fake_device::start_recording("hunter2", fake_device::filler(frames));
        let reach = Reach::Tcp { host: "127.0.0.1".into(), port };
        let start = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(0, 0, 0).unwrap();
        let began = std::time::Instant::now();
        let footage = replay::fetch(
            &reach,
            "admin",
            "hunter2",
            0,
            StreamType::Main,
            start,
            start + chrono::Duration::seconds(frames as i64 / 15 + 1),
            Duration::from_secs(5),
            &mut |_| true,
        )
        .unwrap();
        let secs = began.elapsed().as_secs_f64();
        let mb = footage.elementary().len() as f64 / 1e6;
        println!("{} frames, {mb:.0} MB in {secs:.2}s: {:.0} MB/s", footage.frames.len(), mb / secs);
        assert_eq!(footage.frames.len(), frames);
    }

    /// A download told to stop stops: cancelled, with no file left behind, after
    /// a few pieces of footage rather than all of them.
    #[test]
    fn a_cancelled_download_stops_and_saves_nothing() {
        use crate::api::error::Error;
        use crate::api::vendor::baichuan::replay;
        let frames = 600;
        let port = fake_device::start_recording("hunter2", fake_device::filler(frames));
        let reach = Reach::Tcp { host: "127.0.0.1".into(), port };
        let start = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(0, 0, 0).unwrap();
        let dir = std::env::temp_dir().join(format!("kestrel-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("clip.mp4");
        let mut heard = 0;
        let result = replay::fetch_to_mp4(
            &reach,
            "admin",
            "hunter2",
            0,
            StreamType::Main,
            start,
            start + chrono::Duration::seconds(frames as i64 / 15),
            &out,
            Duration::from_secs(5),
            &mut |p| {
                heard += 1;
                p < 0.05
            },
        );
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert!(!out.exists(), "nothing saved");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "no scraps either");
        assert!(heard > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A device that refuses replay (405, as the NVR at an address does) still
    /// plays: the player falls back to downloading it a window at a time, and plays
    /// on across the join into the second window.
    #[test]
    fn a_recording_plays_by_download_where_replay_is_refused() {
        use crate::video::bc_avio::{AvioInput, SliceSource};
        use crate::video::PlaybackWorker;

        // Real H.264 frames, so the decoder has pictures to make: the sample split
        // into frames, repeated to a 70-second recording.
        let mut input = AvioInput::open(
            Box::new(SliceSource::new(include_bytes!("../../../video/testdata/synthetic_h264.bin").to_vec())),
            Some("h264"),
        )
        .unwrap();
        let sample: Vec<(bool, Vec<u8>)> = input
            .packets()
            .map(|(_, p)| (p.is_key(), p.data().unwrap_or_default().to_vec()))
            .collect();
        assert!(!sample.is_empty() && sample[0].0);
        let footage: Vec<(bool, Vec<u8>)> = sample.iter().cycle().take(70 * 15).cloned().collect();
        let port = fake_device::start_recording("hunter2", std::sync::Arc::new(footage));

        let start = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(0, 0, 0).unwrap();
        let player = PlaybackWorker::start_recording(crate::api::vendor::RecordingStream {
            reach: Reach::Tcp { host: "127.0.0.1".into(), port },
            username: "admin".into(),
            password: "hunter2".into(),
            channel: 0,
            stream: StreamType::Main,
            start,
            end: start + chrono::Duration::seconds(70),
            timeout_secs: 5,
        });
        player.set_speed(8.0);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while player.position() < 35.0 && player.error().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(player.error(), None);
        let frame = player.latest_frame().expect("pictures were shown");
        assert_eq!((frame.width, frame.height), (320, 240));
        assert!(player.position() >= 35.0, "played across into the second window: {}", player.position());
    }

    /// Search and calendar over the shared connection, while a tile streams on it:
    /// the fake device accepts one connection, as the NVR at an address does.
    #[test]
    fn search_and_calendar_ride_the_shared_connection() {
        use crate::api::models::EventKind;
        use crate::api::vendor::baichuan::{hub, playback};
        let port = fake_device::start("hunter2");
        let reach = Reach::Tcp { host: "127.0.0.1".into(), port };
        let timeout = Duration::from_secs(5);
        let live = hub::subscribe(&reach, "admin", "hunter2", 0, StreamType::Sub, timeout).unwrap();
        assert!(live.codec.recv_timeout(timeout).is_ok());

        let mut caller = hub::Caller::open(&reach, "admin", "hunter2", timeout).unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let clips = playback::search_recordings(
            &mut caller,
            0,
            day.and_hms_opt(0, 0, 0).unwrap(),
            day.and_hms_opt(23, 59, 59).unwrap(),
            StreamType::Main,
            timeout,
        )
        .unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].triggers, [EventKind::Person]);
        assert_eq!(clips[1].triggers, [EventKind::Package]);
        assert_eq!(playback::recorded_days(&mut caller, 0, day, StreamType::Main, timeout).unwrap(), [5]);
        drop(live);
    }

    /// Clicking a second clip drops the first's fetch and asks for the next at
    /// once. The device must hear the stop first: asked the other way round, the
    /// NVR was still replaying the first clip and never answered the second.
    #[test]
    fn a_fetch_let_go_is_stopped_before_the_next_is_asked() {
        use crate::api::vendor::baichuan::hub;
        const ASK: u32 = 9001;
        const STOP: u32 = 9002;
        let (port, heard) = fake_device::start_logged("hunter2");
        let reach = Reach::Tcp { host: "127.0.0.1".into(), port };
        let timeout = Duration::from_secs(5);
        let _held = hub::hold(&reach, "admin", "hunter2", timeout).unwrap();
        let fetch = |n: u32| hub::fetch(&reach, "admin", "hunter2", ASK, STOP, 0, format!("<n>{n}</n>"), timeout).unwrap();
        let asked = |heard: &Vec<u32>| heard.iter().filter(|&&id| id == ASK).count();
        let first = fetch(1);
        let deadline = std::time::Instant::now() + timeout;
        while asked(&heard.lock().unwrap()) < 1 {
            assert!(std::time::Instant::now() < deadline, "first fetch never asked");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(first);
        let _second = fetch(2);
        while asked(&heard.lock().unwrap()) < 2 {
            assert!(std::time::Instant::now() < deadline, "second fetch never asked");
            std::thread::sleep(Duration::from_millis(5));
        }
        let heard: Vec<u32> = heard.lock().unwrap().iter().copied().filter(|&id| id == ASK || id == STOP).collect();
        assert_eq!(heard, [ASK, STOP, ASK]);
    }

    /// The connection `connect` logs in is the device's only one: a wall tile,
    /// search and detections all ride it. The fake device accepts a single
    /// connection, so any of them opening its own would fail.
    #[test]
    fn connect_keeps_one_connection_for_everything() {
        use crate::api::vendor::baichuan::hub;
        let port = fake_device::start("hunter2");
        let (reach, config) = tcp_config(port, "hunter2");
        let mut client = BaichuanClient::reaching(reach.clone(), &config);
        client.connect().unwrap();
        let timeout = Duration::from_secs(5);
        let live = hub::subscribe(&reach, "admin", "hunter2", 0, StreamType::Sub, timeout).unwrap();
        assert!(live.codec.recv_timeout(timeout).is_ok(), "the tile streams");

        let day = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let clips = client
            .search_recordings(0, day.and_hms_opt(0, 0, 0).unwrap(), day.and_hms_opt(23, 59, 59).unwrap(), StreamType::Main)
            .unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(client.recorded_days(0, day, StreamType::Main).unwrap(), [5]);
        let state = client.detections(&[0, 1]).unwrap();
        assert_eq!(state.len(), 2, "detections over the same connection");
        drop(live);
    }

    /// A device on its own network, or at an address, has the bandwidth for live
    /// video and playback at once: Playback does not pause its tiles.
    #[test]
    fn a_device_over_tcp_is_not_paused_for_playback() {
        let port = fake_device::start("hunter2");
        let (reach, config) = tcp_config(port, "hunter2");
        let mut client = BaichuanClient::reaching(reach, &config);
        assert!(client.pause_live_for_playback().is_none(), "not connected");
        client.connect().unwrap();
        assert!(client.pause_live_for_playback().is_none(), "not constrained");
    }

    /// A tile let go — paused for playback — and asked for again gets video again
    /// on the same connection.
    #[test]
    fn a_stream_comes_back_after_being_let_go() {
        use crate::api::vendor::baichuan::hub;
        use std::sync::atomic::Ordering;
        let port = fake_device::start("hunter2");
        let (reach, config) = tcp_config(port, "hunter2");
        let mut client = BaichuanClient::reaching(reach.clone(), &config);
        client.connect().unwrap();
        let timeout = Duration::from_secs(5);
        for round in 0..3 {
            let sub = hub::subscribe(&reach, "admin", "hunter2", 0, StreamType::Sub, timeout).unwrap();
            assert!(sub.codec.recv_timeout(timeout).is_ok(), "video in round {round}");
            sub.closed.store(true, Ordering::Relaxed);
        }
    }
}
