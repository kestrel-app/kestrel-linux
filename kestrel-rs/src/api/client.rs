//! Client for the Reolink HTTP CGI API.
//!
//! Every call is a POST to /cgi-bin/api.cgi carrying a JSON *array* of command
//! objects; the device replies with a matching array. Failures are reported
//! inside an HTTP 200 body, so status codes alone tell you very little.
//!
//! The same type serves standalone cameras and NVRs. Behaviour that looks odd
//! here is almost always a firmware quirk established by probing real hardware —
//! each one is commented with what was observed.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use log::{debug, info, warn};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};

use super::error::{redact, Error, Result};
use super::models::{
    as_i64, as_string, is_dual_lens_model, to_device_time, Channel, DeviceInfo,
    Lens, Recording, StreamType,
};

/// Reolink tokens carry a leaseTime (usually 3600s). Renew early so a
/// long-running grid never blocks on a mid-request expiry.
const TOKEN_RENEW_MARGIN: Duration = Duration::from_secs(120);

struct Token {
    value: String,
    expires_at: Instant,
}

pub struct ReolinkClient {
    pub host: String,
    pub port: u16,
    pub https: bool,
    pub rtsp_port: u16,
    pub username: String,
    password: String,

    agent: ureq::Agent,
    token: Mutex<Option<Token>>,

    /// Firmware disagrees on which `action` the Search command wants; the wrong
    /// one is answered with HTTP 502. Learned on first use, then reused.
    search_action: Mutex<Option<i64>>,

    pub device_info: Option<DeviceInfo>,
    pub channels: Vec<Channel>,
    abilities: Value,
}

impl ReolinkClient {
    pub fn new(host: impl Into<String>, username: impl Into<String>, password: impl Into<String>) -> Self {
        Self::with_options(host, username, password, 80, false, 554, 10, 4, false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_options(
        host: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
        port: u16,
        https: bool,
        rtsp_port: u16,
        read_timeout_secs: u64,
        connect_timeout_secs: u64,
        allow_self_signed: bool,
    ) -> Self {
        // Separate connect and read budgets: an unreachable host should fail
        // fast (it otherwise blocks shutdown), while a reachable-but-busy NVR
        // answering a Search over a full day legitimately needs the long window.
        let agent = if allow_self_signed {
            ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(connect_timeout_secs))
                .timeout_read(Duration::from_secs(read_timeout_secs))
                .tls_config(super::http::accept_any_certificate())
                .build()
        } else {
            ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(connect_timeout_secs))
                .timeout_read(Duration::from_secs(read_timeout_secs))
                .build()
        };

        ReolinkClient {
            host: host.into(),
            port,
            https,
            rtsp_port,
            username: username.into(),
            password: password.into(),
            agent,
            token: Mutex::new(None),
            search_action: Mutex::new(None),
            device_info: None,
            channels: Vec::new(),
            abilities: Value::Null,
        }
    }

    pub fn base_url(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    // ------------------------------------------------------------- transport

    fn post(&self, body: &Value, with_token: bool, cmd_hint: &str) -> Result<Vec<Value>> {
        let url = format!("{}/cgi-bin/api.cgi", self.base_url());
        let mut request = self.agent.post(&url).query("cmd", cmd_hint);
        if with_token {
            let token = self.ensure_token()?;
            request = request.query("token", &token);
        }

        let response = match request.send_json(body.clone()) {
            Ok(response) => response,
            Err(ureq::Error::Status(code, _)) if code >= 500 => {
                // Reolink answers a request it cannot parse with a gateway error
                // rather than a JSON error object. That is a rejected *command*,
                // not a broken connection, and callers retry it differently.
                return Err(Error::command(
                    cmd_hint,
                    code as i64,
                    format!("device rejected the request (HTTP {code})"),
                ));
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(Error::connection(format!(
                    "{}: HTTP {code}",
                    self.host
                )))
            }
            Err(err) => {
                return Err(Error::connection(format!("{}: {}", self.host, err)));
            }
        };

        let parsed: Value = response
            .into_json()
            .map_err(|err| Error::connection(format!("{}: malformed JSON response ({err})", self.host)))?;

        Ok(match parsed {
            Value::Array(items) => items,
            other => vec![other],
        })
    }

    /// Raise on any per-command failure, else hand the entries back.
    fn check(entries: Vec<Value>) -> Result<Vec<Value>> {
        for entry in &entries {
            let code = as_i64(entry.get("code"));
            if code == 0 {
                continue;
            }
            let cmd = as_string(entry.get("cmd"));
            let error = entry.get("error");
            let detail = error.map(|e| as_string(e.get("detail"))).unwrap_or_default();
            let rsp_code = error.and_then(|e| e.get("rspCode")).map(|v| as_i64(Some(v)));

            // rspCode -6 / "please login first" means the token died.
            if matches!(rsp_code, Some(-6) | Some(-7)) || detail.to_lowercase().contains("login") {
                return Err(Error::auth(format!(
                    "{cmd}: {}",
                    if detail.is_empty() { "token rejected" } else { &detail }
                )));
            }
            return Err(Error::Command {
                cmd,
                code,
                detail,
                rsp_code,
            });
        }
        Ok(entries)
    }

    /// Run a single command and return its `value` payload.
    pub fn call(&self, cmd: &str, action: i64, param: Option<Value>) -> Result<Value> {
        let mut entry = json!({"cmd": cmd, "action": action});
        if let Some(param) = param {
            entry["param"] = param;
        }
        let body = Value::Array(vec![entry]);

        let entries = match Self::check(self.post(&body, true, cmd)?) {
            Ok(entries) => entries,
            Err(Error::Auth(_)) => {
                // One transparent retry after a forced re-login covers a token
                // that expired ahead of our renewal margin (clock skew, reboot).
                info!("token rejected on {cmd}, re-authenticating");
                self.invalidate_token();
                Self::check(self.post(&body, true, cmd)?)?
            }
            Err(other) => return Err(other),
        };

        Ok(entries
            .into_iter()
            .next()
            .and_then(|mut e| e.get_mut("value").map(Value::take))
            .unwrap_or(Value::Null))
    }

    /// Run several commands in one round trip.
    ///
    /// Individual failures come back as `Null` rather than erroring, because
    /// callers batch *optional* probes where one unsupported command must not
    /// sink the whole request.
    pub fn batch(&self, commands: &[(&str, i64, Option<Value>)]) -> Result<Vec<Value>> {
        let items: Vec<Value> = commands
            .iter()
            .map(|(cmd, action, param)| {
                let mut entry = json!({"cmd": cmd, "action": action});
                if let Some(param) = param {
                    entry["param"] = param.clone();
                }
                entry
            })
            .collect();

        let hint = commands.first().map(|c| c.0).unwrap_or("");
        let entries = self.post(&Value::Array(items), true, hint)?;

        let mut out: Vec<Value> = entries
            .into_iter()
            .map(|entry| {
                if as_i64(entry.get("code")) != 0 {
                    debug!("batched {} failed: {:?}", as_string(entry.get("cmd")), entry.get("error"));
                    Value::Null
                } else {
                    entry.get("value").cloned().unwrap_or(Value::Null)
                }
            })
            .collect();
        // Short responses happen when firmware drops commands it does not know.
        out.resize(commands.len(), Value::Null);
        Ok(out)
    }

    // ------------------------------------------------------------- auth

    fn ensure_token(&self) -> Result<String> {
        {
            let guard = self.token.lock().unwrap();
            if let Some(token) = guard.as_ref() {
                if Instant::now() + TOKEN_RENEW_MARGIN < token.expires_at {
                    return Ok(token.value.clone());
                }
            }
        }
        self.login()
    }

    fn invalidate_token(&self) {
        *self.token.lock().unwrap() = None;
    }

    fn login(&self) -> Result<String> {
        let body = json!([{
            "cmd": "Login",
            "action": 0,
            "param": {"User": {
                "userName": self.username,
                "password": self.password,
                // Some firmware rejects the login without an explicit version.
                "Version": "0",
            }},
        }]);

        let entries = self.post(&body, false, "Login")?;
        let entry = entries.first().cloned().unwrap_or(Value::Null);
        if as_i64(entry.get("code")) != 0 {
            let detail = entry
                .get("error")
                .map(|e| as_string(e.get("detail")))
                .unwrap_or_else(|| "login rejected".into());
            return Err(Error::auth(format!("{}: {detail}", self.host)));
        }

        let token = entry.get("value").and_then(|v| v.get("Token")).cloned().unwrap_or(Value::Null);
        let name = as_string(token.get("name"));
        if name.is_empty() {
            return Err(Error::auth(format!("{}: device returned no token", self.host)));
        }
        let lease = as_i64(token.get("leaseTime")).max(1) as u64;

        info!("logged in to {} (lease {lease}s)", self.host);
        *self.token.lock().unwrap() = Some(Token {
            value: name.clone(),
            expires_at: Instant::now() + Duration::from_secs(lease),
        });
        Ok(name)
    }

    pub fn logout(&self) {
        if self.token.lock().unwrap().is_none() {
            return;
        }
        let param = json!({"User": {"userName": self.username}});
        if let Err(err) = self.call("Logout", 0, Some(param)) {
            debug!("logout failed for {}: {err}", self.host);
        }
        self.invalidate_token();
    }

    // ------------------------------------------------------------- discovery

    /// Log in, identify the device, and enumerate its channels.
    pub fn connect(&mut self) -> Result<DeviceInfo> {
        self.ensure_token()?;

        let results = self.batch(&[
            ("GetDevInfo", 0, None),
            ("GetAbility", 0, Some(json!({"User": {"userName": self.username}}))),
        ])?;

        let info_value = match results.first() {
            Some(Value::Null) | None => json!({"DevInfo": self.call("GetDevInfo", 0, None)?}),
            Some(value) => value.clone(),
        };
        self.device_info = Some(DeviceInfo::parse(&info_value));
        self.abilities = results
            .get(1)
            .and_then(|v| v.get("Ability"))
            .cloned()
            .unwrap_or(Value::Null);

        let count = self.device_info.as_ref().map(|d| d.channel_count).unwrap_or(1);
        self.channels = self.enumerate_channels(count)?;
        Ok(self.device_info.clone().unwrap_or_default())
    }

    fn enumerate_channels(&mut self, count: usize) -> Result<Vec<Channel>> {
        let mut channels: Vec<Channel> = (0..count.max(1)).map(|i| Channel::new(i as u32)).collect();

        // NVRs expose per-channel names and online state here; cameras reject it.
        match self.call("GetChannelstatus", 0, None) {
            Ok(status) => {
                let rows = status
                    .get("status")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                // GetChannelstatus is authoritative about how many channels
                // exist. When GetDevInfo comes back thin — which happens while
                // the device is busy — believe this instead, rather than
                // silently presenting a 36-channel NVR as a single camera.
                let reported = {
                    let declared = as_i64(status.get("count")).max(0) as usize;
                    if declared > 0 { declared } else { rows.len() }
                };
                if reported > channels.len() {
                    warn!(
                        "{}: GetDevInfo reported {} channel(s) but GetChannelstatus lists {}; \
                         using the larger count",
                        self.host,
                        channels.len(),
                        reported
                    );
                    for i in channels.len()..reported {
                        channels.push(Channel::new(i as u32));
                    }
                    if let Some(info) = self.device_info.as_mut() {
                        info.channel_count = channels.len();
                    }
                }

                for row in &rows {
                    let index = as_i64(row.get("channel")).max(0) as usize;
                    if let Some(channel) = channels.get_mut(index) {
                        let name = as_string(row.get("name"));
                        if !name.is_empty() {
                            channel.name = name;
                        }
                        channel.online = row
                            .get("online")
                            .map(|v| as_i64(Some(v)) != 0)
                            .unwrap_or(true);
                        channel.model = as_string(row.get("typeInfo"));
                    }
                }
            }
            Err(err) => debug!("{}: no channel status ({err})", self.host),
        }

        if let Some(per_channel) = self.abilities.get("abilityChn").and_then(Value::as_array) {
            for channel in channels.iter_mut() {
                if let Some(ability) = per_channel.get(channel.index as usize) {
                    channel.apply_abilities(ability);
                }
            }
        }

        // A single camera has no GetChannelstatus, so take its name from GetDevInfo.
        if channels.len() == 1 && channels[0].name.is_empty() {
            if let Some(info) = &self.device_info {
                channels[0].name = info.name.clone();
            }
        }

        self.link_dual_lens(&mut channels);
        self.probe_encoders(&mut channels)?;
        Ok(channels)
    }

    /// Mark the two channels of a dual-lens camera as wide and telephoto.
    ///
    /// Assumes channel 0 is wide and channel 1 telephoto, which is how TrackMix
    /// orders them. Restricted to models known to work this way so an NVR with
    /// two cameras attached is never mistaken for one camera with two lenses.
    fn link_dual_lens(&self, channels: &mut [Channel]) {
        let model = self.device_info.as_ref().map(|d| d.model.clone()).unwrap_or_default();
        if channels.len() != 2 || !is_dual_lens_model(&model) {
            return;
        }
        let base = self.device_info.as_ref().map(|d| d.name.clone()).unwrap_or_default();

        channels[0].lens = Lens::Wide;
        channels[0].lens_partner = Some(channels[1].index);
        channels[1].lens = Lens::Tele;
        channels[1].lens_partner = Some(channels[0].index);

        if channels[0].name.is_empty() {
            channels[0].name = if base.is_empty() { "Wide".into() } else { base };
        }
        if channels[1].name.is_empty() || channels[1].name == channels[0].name {
            channels[1].name = format!("{} (Tele)", channels[0].name);
        }
        info!(
            "{model}: dual-lens camera, ch{} wide / ch{} tele",
            channels[0].index, channels[1].index
        );
    }

    fn probe_encoders(&self, channels: &mut [Channel]) -> Result<()> {
        let commands: Vec<(&str, i64, Option<Value>)> = channels
            .iter()
            .map(|ch| ("GetEnc", 0, Some(json!({"channel": ch.index}))))
            .collect();
        if commands.is_empty() {
            return Ok(());
        }

        let results = self.batch(&commands)?;
        for (channel, value) in channels.iter_mut().zip(results) {
            let enc = value.get("Enc").cloned().unwrap_or(Value::Null);
            let main = as_string(enc.get("mainStream").and_then(|s| s.get("vType")));
            let sub = as_string(enc.get("subStream").and_then(|s| s.get("vType")));
            if !main.is_empty() {
                channel.main_codec = main.to_lowercase();
            }
            if !sub.is_empty() {
                channel.sub_codec = sub.to_lowercase();
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------- streaming

    /// Build the RTSP URL for a channel.
    ///
    /// The path encodes the codec and a *1-based* channel number, e.g.
    /// `h264Preview_01_main`. Credentials are inlined because RTSP has no
    /// separate auth hook here.
    pub fn rtsp_url(&self, channel: u32, stream: StreamType) -> String {
        let codec = self
            .channels
            .iter()
            .find(|c| c.index == channel)
            .map(|c| match stream {
                StreamType::Main => c.main_codec.clone(),
                StreamType::Sub => c.sub_codec.clone(),
            })
            .unwrap_or_else(|| "h264".into());
        let codec = if codec.contains("265") { "h265" } else { "h264" };

        let user = utf8_percent_encode(&self.username, NON_ALPHANUMERIC);
        let pass = utf8_percent_encode(&self.password, NON_ALPHANUMERIC);
        format!(
            "rtsp://{user}:{pass}@{}:{}/{codec}Preview_{:02}_{}",
            self.host,
            self.rtsp_port,
            channel + 1,
            stream.as_str()
        )
    }

    // ------------------------------------------------------------- ptz

    pub fn ptz_move(&self, channel: u32, direction: &str, speed: i64) -> Result<()> {
        let op = match direction {
            "up" => "Up",
            "down" => "Down",
            "left" => "Left",
            "right" => "Right",
            "leftup" => "LeftUp",
            "leftdown" => "LeftDown",
            "rightup" => "RightUp",
            "rightdown" => "RightDown",
            "zoom_in" => "ZoomInc",
            "zoom_out" => "ZoomDec",
            "focus_near" => "FocusInc",
            "focus_far" => "FocusDec",
            other => return Err(Error::Unsupported(format!("unknown PTZ direction {other:?}"))),
        };
        self.call(
            "PtzCtrl",
            0,
            Some(json!({"channel": channel, "op": op, "speed": speed.clamp(1, 64)})),
        )?;
        Ok(())
    }

    pub fn ptz_stop(&self, channel: u32) -> Result<()> {
        self.call("PtzCtrl", 0, Some(json!({"channel": channel, "op": "Stop"})))?;
        Ok(())
    }

    /// List the presets stored on a channel.
    ///
    /// Firmware disagrees about the shape: most return `PtzPreset` as a flat
    /// list, some wrap it as `{"preset": [...]}`, and a few return a single
    /// object. An RLN36 returns the flat list — parsing it as a dict was a real
    /// crash. All three are accepted.
    pub fn ptz_presets(&self, channel: u32) -> Result<Vec<(i64, String)>> {
        let value = self.call("GetPtzPreset", 1, Some(json!({"channel": channel})))?;
        let raw = value.get("PtzPreset").cloned().unwrap_or(Value::Null);

        let entries: Vec<Value> = match raw {
            Value::Array(items) => items,
            Value::Object(ref map) => match map.get("preset") {
                Some(Value::Array(items)) => items.clone(),
                _ => vec![raw.clone()],
            },
            _ => {
                debug!("unexpected GetPtzPreset payload for channel {channel}: {raw:?}");
                return Ok(Vec::new());
            }
        };

        let mut presets = Vec::new();
        for entry in entries {
            let Some(id) = entry.get("id") else { continue };
            // `enable` marks a slot as programmed; treat it as enabled when
            // absent, since not every firmware reports it.
            let enabled = entry.get("enable").map(|v| as_i64(Some(v)) != 0).unwrap_or(true);
            if !enabled {
                continue;
            }
            let id = as_i64(Some(id));
            let name = as_string(entry.get("name"));
            presets.push((id, if name.is_empty() { format!("Preset {id}") } else { name }));
        }
        Ok(presets)
    }

    pub fn ptz_goto_preset(&self, channel: u32, preset_id: i64, speed: i64) -> Result<()> {
        self.call(
            "PtzCtrl",
            0,
            Some(json!({"channel": channel, "op": "ToPos", "id": preset_id, "speed": speed})),
        )?;
        Ok(())
    }

    /// Return the camera to its guard ("home") position. Reolink exposes this
    /// through the guard-position command rather than PtzCtrl.
    pub fn ptz_go_home(&self, channel: u32) -> Result<()> {
        self.call(
            "SetPtzGuard",
            0,
            Some(json!({"PtzGuard": {"channel": channel, "cmdStr": "toPos", "bSaveCurrentPos": 0}})),
        )?;
        Ok(())
    }

    /// Run the PTZ self-calibration sweep. The camera drives its full range and
    /// ignores other commands until it finishes.
    pub fn ptz_calibrate(&self, channel: u32) -> Result<()> {
        self.call("PtzCheck", 0, Some(json!({"channel": channel})))?;
        Ok(())
    }

    // ------------------------------------------------------------- playback

    /// Run a Search, working out which `action` this firmware accepts.
    ///
    /// Some devices want action=1 and answer action=0 with an empty result;
    /// others reject action=1 outright with HTTP 502 (observed on an RLN36).
    /// The first call tries both and the winner is remembered.
    fn search(&self, params: Value) -> Result<Value> {
        let cached = *self.search_action.lock().unwrap();
        let candidates: Vec<i64> = match cached {
            // Known-good value first, but keep the other as a fallback so a
            // transient rejection cannot wedge the client permanently.
            Some(action) => std::iter::once(action).chain([1, 0].into_iter().filter(|a| *a != action)).collect(),
            None => vec![1, 0],
        };

        let mut last_error = None;
        for action in candidates {
            match self.call("Search", action, Some(json!({"Search": params}))) {
                Ok(value) => {
                    let mut guard = self.search_action.lock().unwrap();
                    if *guard != Some(action) {
                        info!("{}: using Search action={action}", self.host);
                        *guard = Some(action);
                    }
                    return Ok(value);
                }
                Err(err) if err.is_command_failure() => {
                    info!("Search action={action} rejected by {} ({err})", self.host);
                    last_error = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error.unwrap_or_else(|| Error::command("Search", -1, "no usable action")))
    }

    pub fn search_recordings(
        &self,
        channel: u32,
        start: NaiveDateTime,
        end: NaiveDateTime,
        stream: StreamType,
    ) -> Result<Vec<Recording>> {
        let value = self.search(json!({
            "channel": channel,
            "onlyStatus": 0,
            "streamType": stream.as_str(),
            "StartTime": to_device_time(&start),
            "EndTime": to_device_time(&end),
        }))?;

        let files = value
            .get("SearchResult")
            .and_then(|r| r.get("File"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut out: Vec<Recording> = files
            .iter()
            .filter_map(|entry| {
                let parsed = Recording::parse(channel, stream, entry);
                if parsed.is_none() {
                    warn!("skipping search result with no usable time span: {entry}");
                }
                parsed
            })
            .collect();

        if !out.is_empty() && !out.iter().any(Recording::is_fetchable) {
            info!(
                "{}: search results carry no file names; this firmware indexes \
                 recordings by time only",
                self.host
            );
        }
        out.sort_by_key(|r| r.start);
        Ok(out)
    }

    /// Which days in a month hold footage — drives the calendar. Uses the
    /// compact per-day bitmask rather than the full file list.
    pub fn recorded_days(&self, channel: u32, month_of: NaiveDate, stream: StreamType) -> Result<Vec<u32>> {
        let first = month_of.with_day(1).unwrap_or(month_of);
        let next_month = if first.month() == 12 {
            NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
        }
        .unwrap_or(first);
        let last = next_month.pred_opt().unwrap_or(first);

        let value = self.search(json!({
            "channel": channel,
            "onlyStatus": 1,
            "streamType": stream.as_str(),
            "StartTime": to_device_time(&first.and_hms_opt(0, 0, 0).unwrap()),
            "EndTime": to_device_time(&last.and_hms_opt(23, 59, 59).unwrap()),
        }))?;

        let mut days = Vec::new();
        if let Some(statuses) = value
            .get("SearchResult")
            .and_then(|r| r.get("Status"))
            .and_then(Value::as_array)
        {
            for status in statuses {
                // `table` is a string of '0'/'1', one character per day.
                for (i, flag) in as_string(status.get("table")).chars().enumerate() {
                    if flag == '1' {
                        days.push(i as u32 + 1);
                    }
                }
            }
        }
        days.sort_unstable();
        days.dedup();
        Ok(days)
    }

    /// URL for fetching a clip, with the token already attached.
    ///
    /// Returns None for firmware that lists recordings without a file name —
    /// there is no handle to ask for, so the clip cannot be fetched over HTTP
    /// at all. Callers must say so rather than opening a request that 404s.
    pub fn download_url(&self, recording: &Recording) -> Result<Option<String>> {
        if !recording.is_fetchable() {
            return Ok(None);
        }
        let token = self.ensure_token()?;
        let name = recording.name.clone();
        let output = name.rsplit('/').next().unwrap_or(&name).to_string();
        let encode = |value: &str| {
            utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
        };
        Ok(Some(format!(
            "{}/cgi-bin/api.cgi?cmd=Download&source={}&output={}&token={}",
            self.base_url(),
            encode(&name),
            encode(&output),
            encode(&token)
        )))
    }

    // ------------------------------------------------------------- events

    /// Current motion flag per channel, in one round trip.
    pub fn motion_state(&self, channels: &[u32]) -> Result<Vec<(u32, bool)>> {
        let commands: Vec<(&str, i64, Option<Value>)> = channels
            .iter()
            .map(|c| ("GetMdState", 0, Some(json!({"channel": c}))))
            .collect();
        let results = self.batch(&commands)?;
        Ok(channels
            .iter()
            .copied()
            .zip(results)
            .map(|(c, v)| (c, as_i64(v.get("state")) != 0))
            .collect())
    }

    /// Current AI detection flags per channel.
    ///
    /// The response mixes scalars ("channel") with per-type objects, so anything
    /// that is not an object carrying `alarm_state` is ignored.
    pub fn ai_state(&self, channels: &[u32]) -> Result<Vec<(u32, Vec<(String, bool)>)>> {
        let commands: Vec<(&str, i64, Option<Value>)> = channels
            .iter()
            .map(|c| ("GetAiState", 0, Some(json!({"channel": c}))))
            .collect();
        let results = self.batch(&commands)?;

        Ok(channels
            .iter()
            .copied()
            .zip(results)
            .map(|(channel, value)| {
                let mut flags = Vec::new();
                if let Some(map) = value.as_object() {
                    for (key, sub) in map {
                        let Some(sub) = sub.as_object() else { continue };
                        if !sub.contains_key("alarm_state") {
                            continue;
                        }
                        let supported = sub.get("support").map(|v| as_i64(Some(v)) != 0).unwrap_or(true);
                        if supported {
                            flags.push((key.clone(), as_i64(sub.get("alarm_state")) != 0));
                        }
                    }
                }
                (channel, flags)
            })
            .collect())
    }

    /// Pull a still JPEG straight from the device.
    pub fn snapshot(&self, channel: u32) -> Result<Vec<u8>> {
        let token = self.ensure_token()?;
        let url = format!("{}/cgi-bin/api.cgi", self.base_url());
        let response = self
            .agent
            .get(&url)
            .query("cmd", "Snap")
            .query("channel", &channel.to_string())
            .query("rs", &format!("{}", Instant::now().elapsed().as_nanos() as u64 | 1))
            .query("token", &token)
            .call()
            .map_err(|err| Error::connection(format!("snapshot failed: {}", redact(&err.to_string()))))?;

        // An error comes back as JSON rather than image bytes.
        if response.content_type().contains("json") {
            return Err(Error::command("Snap", -1, "device returned JSON instead of an image"));
        }

        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|err| Error::connection(format!("snapshot read failed: {err}")))?;
        Ok(bytes)
    }
}

use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::Lens;

    fn client_with(model: &str, channels: usize) -> ReolinkClient {
        let mut client = ReolinkClient::new("host", "user", "pass");
        client.device_info = Some(DeviceInfo {
            name: "Yard".into(),
            model: model.into(),
            channel_count: channels,
            ..Default::default()
        });
        client
    }

    #[test]
    fn rtsp_urls_are_one_based_and_escape_credentials() {
        let mut client = ReolinkClient::with_options("198.51.100.5", "admin", "p@ss/word", 80, false, 554, 10, 4, false);
        client.channels = vec![
            Channel { main_codec: "h265".into(), ..Channel::new(0) },
            Channel { main_codec: "h264".into(), ..Channel::new(3) },
        ];
        assert_eq!(
            client.rtsp_url(0, StreamType::Main),
            "rtsp://admin:p%40ss%2Fword@198.51.100.5:554/h265Preview_01_main"
        );
        assert_eq!(
            client.rtsp_url(3, StreamType::Sub),
            "rtsp://admin:p%40ss%2Fword@198.51.100.5:554/h264Preview_04_sub"
        );
    }

    #[test]
    fn links_trackmix_lenses_but_not_a_two_camera_nvr() {
        let client = client_with("TrackMix PoE", 2);
        let mut channels = vec![Channel::new(0), Channel::new(1)];
        client.link_dual_lens(&mut channels);
        assert_eq!(channels[0].lens, Lens::Wide);
        assert_eq!(channels[1].lens, Lens::Tele);
        assert_eq!(channels[0].lens_partner, Some(1));
        assert!(channels[1].name.contains("Tele"));

        let nvr = client_with("RLN8-410", 2);
        let mut channels = vec![Channel::new(0), Channel::new(1)];
        nvr.link_dual_lens(&mut channels);
        assert!(!channels[0].is_dual_lens(), "a 2-channel NVR was misread as dual-lens");
    }

    #[test]
    fn command_failures_are_distinguishable_from_transport_ones() {
        let cmd = Error::command("Search", 502, "device rejected the request (HTTP 502)");
        assert!(cmd.is_command_failure());
        assert!(!Error::connection("boom").is_command_failure());
    }
}
