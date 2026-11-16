//! Reading and writing device configuration.
//!
//! Reolink's `Set*` commands replace a whole configuration block. Sending a
//! partial structure does not merge — omitted fields take firmware defaults, so
//! a naive "just set the bitrate" call can silently reset the resolution
//! alongside it. Every write here is therefore **read-modify-write**: fetch the
//! current block, change one field, send the whole thing back.
//!
//! Writes are also verified. After a successful `Set` the block is re-read and
//! compared, because this API reports success for commands that did not
//! actually take effect.
//!
//! That re-read has to wait. A `Set` is applied asynchronously: on an RLN36 the
//! command returns in 0.1–0.35s but the device keeps serving the *old* value
//! from `Get` for up to ~2s afterwards. Reading once, immediately, therefore
//! reports a perfectly good write as a failure — so the check polls until the
//! device agrees, and only gives up after [`SETTLE_TIMEOUT`].

use std::time::{Duration, Instant};

use log::{debug, info};
use serde_json::{json, Value};

use super::client::ReolinkClient;
use super::error::{Error, Result};
use super::models::as_i64;

/// Commands this module refuses to send.
///
/// Nothing here is exposed through the UI, and the guard exists so a future
/// caller cannot reach them by accident: they either destroy recordings, strand
/// the device on the network, or remove the credentials we are authenticated
/// with. Any of those turns a settings mistake into a site visit.
const REFUSED: &[&str] = &[
    "Format",      // erases the HDD
    "Restore",     // factory reset
    "Reboot",      //
    "SetNetPort",  // can make the device unreachable
    "SetLocalLink",// ditto: IP/DHCP
    "SetWifi",     //
    "SetUser",     // can lock us out
    "AddUser",     //
    "DelUser",     //
    "SetDdns",     //
    "UpgradePrepare",
    "Upgrade",
];

/// How long to let a write settle before calling it failed. Measured worst
/// case on an RLN36 was 2.07s; this leaves generous headroom for a busy device.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(6);

/// Gap between read-backs. The device needs the better part of a second, and
/// each `Get` is itself a round trip, so polling faster only adds traffic.
const SETTLE_INTERVAL: Duration = Duration::from_millis(250);

fn refuse_if_dangerous(cmd: &str) -> Result<()> {
    if REFUSED.iter().any(|banned| banned.eq_ignore_ascii_case(cmd)) {
        return Err(Error::Unsupported(format!(
            "{cmd} is not available through Kestrel: it can destroy recordings \
             or leave the device unreachable. Use the device's own web UI."
        )));
    }
    Ok(())
}

/// One editable setting, described so the UI can render it without knowing
/// anything about the underlying command.
#[derive(Debug, Clone)]
pub struct Setting {
    pub key: String,
    pub label: String,
    pub value: SettingValue,
    /// Human-readable note, e.g. what a value affects.
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Toggle(bool),
    Number { value: i64, min: i64, max: i64 },
    Choice { value: String, options: Vec<String> },
    Text(String),
}

/// A configuration block: what was read, and where to write it back.
#[derive(Debug, Clone)]
pub struct Block {
    /// The `Get`/`Set` pair, e.g. ("GetOsd", "SetOsd", "Osd").
    pub get_cmd: &'static str,
    pub set_cmd: &'static str,
    pub root: &'static str,
    pub channel: Option<u32>,
    /// The block exactly as the device returned it.
    pub raw: Value,
}

impl Block {
    /// Produce the payload for a write: the original block with one field
    /// replaced, so nothing the device sent us is dropped.
    fn with_field(&self, path: &[&str], value: Value) -> Result<Value> {
        let mut root = self.raw.clone();
        let mut cursor = &mut root;
        for key in &path[..path.len().saturating_sub(1)] {
            cursor = cursor.get_mut(*key).ok_or_else(|| {
                Error::Unsupported(format!("{} has no field {key}", self.root))
            })?;
        }
        let last = path.last().ok_or_else(|| {
            Error::Unsupported("empty setting path".to_string())
        })?;
        let slot = cursor.get_mut(*last).ok_or_else(|| {
            Error::Unsupported(format!("{} has no field {last}", self.root))
        })?;
        *slot = value;
        Ok(root)
    }
}

impl ReolinkClient {
    /// Read a configuration block.
    pub fn get_block(
        &self,
        get_cmd: &'static str,
        set_cmd: &'static str,
        root: &'static str,
        channel: Option<u32>,
    ) -> Result<Block> {
        refuse_if_dangerous(set_cmd)?;
        let param = channel.map(|c| json!({ "channel": c }));
        let value = self.call(get_cmd, 0, param)?;
        let raw = value
            .get(root)
            .cloned()
            .ok_or_else(|| Error::command(get_cmd, -1, format!("no {root} in the response")))?;
        Ok(Block {
            get_cmd,
            set_cmd,
            root,
            channel,
            raw,
        })
    }

    /// Change one field of a block and write the whole block back.
    ///
    /// Returns the re-read block so the caller can confirm what the device
    /// actually stored, which is not always what was asked for.
    pub fn update_field(&self, block: &Block, path: &[&str], value: Value) -> Result<Block> {
        refuse_if_dangerous(block.set_cmd)?;

        let before = block
            .raw
            .pointer(&format!("/{}", path.join("/")))
            .cloned()
            .unwrap_or(Value::Null);
        if before == value {
            debug!("{} {:?} already {value}", block.set_cmd, path);
            return Ok(block.clone());
        }

        let payload = block.with_field(path, value.clone())?;
        info!(
            "{} channel {:?}: {:?} {before} -> {value}",
            block.set_cmd, block.channel, path
        );
        self.call(block.set_cmd, 0, Some(json!({ block.root: payload })))?;

        // The device can accept a command and store something else — clamping a
        // value, or ignoring a field its firmware does not implement. Re-read
        // rather than trusting the acknowledgement.
        //
        // But give it time to apply: the value it serves immediately after a Set
        // is the *old* one, so a single read here would fail every write.
        self.wait_until_stored(block, path, &value)
    }

    /// Poll the device until it reports `value` at `path`.
    ///
    /// A `Set` is applied asynchronously — the command returns in 0.1–0.35s but
    /// `Get` keeps serving the old value for up to ~2s. Anything still wrong
    /// after [`SETTLE_TIMEOUT`] is a real refusal: this firmware acknowledges
    /// writes it then discards.
    fn wait_until_stored(&self, block: &Block, path: &[&str], value: &Value) -> Result<Block> {
        let pointer = format!("/{}", path.join("/"));
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        loop {
            std::thread::sleep(SETTLE_INTERVAL);
            let after = self.get_block(block.get_cmd, block.set_cmd, block.root, block.channel)?;
            let stored = after.raw.pointer(&pointer).cloned().unwrap_or(Value::Null);
            if &stored == value {
                debug!("{} {:?} settled at {value}", block.set_cmd, path);
                return Ok(after);
            }
            if Instant::now() >= deadline {
                return Err(Error::command(
                    block.set_cmd,
                    -1,
                    format!(
                        "device still reports {stored} instead of {value} after \
                         {}s — the setting may be clamped or unsupported on this \
                         firmware",
                        SETTLE_TIMEOUT.as_secs()
                    ),
                ));
            }
        }
    }

    /// Write one floodlight field.
    ///
    /// `WhiteLed` is the exception to this module's read-modify-write rule. If
    /// the payload contains `state`, the firmware treats the whole write as an
    /// on/off command and silently ignores every other field — brightness sent
    /// that way never applied, holding its old value through a 6s settle window,
    /// while the same change sent alone applied in 0.30s. Isolating the field is
    /// safe here: measured against an RLN36, a `{channel, bright}` write altered
    /// `bright` and nothing else — `LightingSchedule`, `wlAiDetectType`, `mode`
    /// and `state` all survived untouched.
    pub fn set_floodlight(&self, block: &Block, field: &str, value: i64) -> Result<Block> {
        refuse_if_dangerous(block.set_cmd)?;
        let channel = block
            .channel
            .map(i64::from)
            .or_else(|| block.raw.get("channel").and_then(Value::as_i64))
            .ok_or_else(|| Error::Unsupported("floodlight block has no channel".into()))?;

        let value = json!(value);
        if block.raw.get(field) == Some(&value) {
            debug!("floodlight {field} already {value}");
            return Ok(block.clone());
        }
        info!(
            "SetWhiteLed channel {channel}: {field} {} -> {value}",
            block.raw.get(field).unwrap_or(&Value::Null)
        );
        self.call(
            block.set_cmd,
            0,
            Some(json!({ block.root: { "channel": channel, field: value.clone() } })),
        )?;
        self.wait_until_stored(block, &[field], &value).map_err(|err| {
            // A refused floodlight write is often not a fault. Inside its
            // lighting schedule, and while a detection is live, the camera
            // holds the light on and overrides a manual switch-off — observed
            // on channel 2 of an RLN36 at 01:00, where every payload shape was
            // ignored until the detection cleared, after which off took 0.3s.
            if field == "state" && err.is_command_failure() {
                Error::command(
                    block.set_cmd,
                    -1,
                    "the camera did not apply it. Its lighting schedule or a live \
                     detection can hold the floodlight on and override a manual \
                     change; try again once the camera settles."
                        .to_string(),
                )
            } else {
                err
            }
        })
    }

    /// Write a block back exactly as it was read.
    ///
    /// Used to prove the read-modify-write round trip works against a device
    /// without changing any of its settings.
    pub fn rewrite_unchanged(&self, block: &Block) -> Result<()> {
        refuse_if_dangerous(block.set_cmd)?;
        self.call(
            block.set_cmd,
            0,
            Some(json!({ block.root: block.raw.clone() })),
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------- blocks

    /// Encoder settings: resolution, bitrate, frame rate and I-frame interval.
    pub fn encoding(&self, channel: u32) -> Result<Block> {
        self.get_block("GetEnc", "SetEnc", "Enc", Some(channel))
    }

    /// On-screen display: channel name and timestamp overlay.
    pub fn osd(&self, channel: u32) -> Result<Block> {
        self.get_block("GetOsd", "SetOsd", "Osd", Some(channel))
    }

    /// Picture controls: brightness, contrast, saturation, sharpness.
    pub fn image(&self, channel: u32) -> Result<Block> {
        self.get_block("GetImage", "SetImage", "Image", Some(channel))
    }

    /// Day/night, exposure, anti-flicker, rotation.
    pub fn isp(&self, channel: u32) -> Result<Block> {
        self.get_block("GetIsp", "SetIsp", "Isp", Some(channel))
    }

    /// The white-light floodlight: on/off state, brightness and schedule.
    pub fn white_led(&self, channel: u32) -> Result<Block> {
        self.get_block("GetWhiteLed", "SetWhiteLed", "WhiteLed", Some(channel))
    }

    /// Infrared illuminator.
    pub fn ir_lights(&self, channel: u32) -> Result<Block> {
        self.get_block("GetIrLights", "SetIrLights", "IrLights", Some(channel))
    }

    /// Describe the encoder block as editable settings.
    ///
    /// The I-frame interval is the interesting one: it sets how long a new
    /// viewer waits for a decodable picture. On this NVR that wait measured
    /// ~3.5s of the ~6.1s it takes a cold stream to show anything.
    pub fn encoding_settings(block: &Block) -> Vec<Setting> {
        let mut out = Vec::new();
        for (stream, label) in [("mainStream", "Main stream"), ("subStream", "Sub stream")] {
            let Some(section) = block.raw.get(stream) else { continue };
            for (field, name, hint) in [
                ("bitRate", "Bitrate (kbps)", None),
                ("frameRate", "Frame rate", None),
                (
                    "gop",
                    "I-frame interval",
                    Some("Lower means a new viewer sees a picture sooner, at the cost of bitrate."),
                ),
            ] {
                let Some(current) = section.get(field) else { continue };
                out.push(Setting {
                    key: format!("{stream}/{field}"),
                    label: format!("{label}: {name}"),
                    value: SettingValue::Number {
                        value: as_i64(Some(current)),
                        min: 1,
                        max: 8192,
                    },
                    hint: hint.map(str::to_string),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block() -> Block {
        Block {
            get_cmd: "GetEnc",
            set_cmd: "SetEnc",
            root: "Enc",
            channel: Some(0),
            raw: json!({
                "channel": 0,
                "mainStream": {"bitRate": 4096, "frameRate": 15, "gop": 60, "size": "2560*1920"},
                "subStream": {"bitRate": 512, "frameRate": 15, "gop": 60, "size": "640*480"}
            }),
        }
    }

    #[test]
    fn destructive_commands_are_refused() {
        for cmd in ["Format", "Restore", "SetLocalLink", "setuser", "Reboot"] {
            assert!(refuse_if_dangerous(cmd).is_err(), "{cmd} should be refused");
        }
        for cmd in ["SetEnc", "SetOsd", "SetImage", "SetIrLights"] {
            assert!(refuse_if_dangerous(cmd).is_ok(), "{cmd} should be allowed");
        }
    }

    /// The whole block must be sent back, not just the changed field — a
    /// partial write resets everything it omits.
    #[test]
    fn a_write_preserves_every_other_field() {
        let updated = block()
            .with_field(&["mainStream", "gop"], json!(15))
            .expect("field exists");

        assert_eq!(updated["mainStream"]["gop"], 15);
        // Everything else survived.
        assert_eq!(updated["mainStream"]["bitRate"], 4096);
        assert_eq!(updated["mainStream"]["size"], "2560*1920");
        assert_eq!(updated["subStream"]["gop"], 60);
        assert_eq!(updated["channel"], 0);
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_invented() {
        assert!(block().with_field(&["mainStream", "nonsense"], json!(1)).is_err());
        assert!(block().with_field(&["noSuchSection", "gop"], json!(1)).is_err());
    }

    #[test]
    fn encoder_settings_are_described_for_both_streams() {
        let settings = ReolinkClient::encoding_settings(&block());
        let keys: Vec<&str> = settings.iter().map(|s| s.key.as_str()).collect();
        assert!(keys.contains(&"mainStream/gop"));
        assert!(keys.contains(&"subStream/bitRate"));

        let gop = settings.iter().find(|s| s.key == "mainStream/gop").unwrap();
        assert_eq!(gop.value, SettingValue::Number { value: 60, min: 1, max: 8192 });
        assert!(gop.hint.is_some(), "the latency trade-off should be explained");
    }
}
