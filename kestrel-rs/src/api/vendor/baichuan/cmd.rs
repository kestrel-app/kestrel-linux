//! Baichuan command numbers.
//!
//! Every name here was confirmed to exist in the official app's
//! `libBCSDKWrapper.so` (4.63.0.3) as an `E_BC_CMD_*` enumerator — the binary
//! carries all 663 of them as strings. The binary does *not* expose the numbers
//! in a form that could be read back out cheaply (they compile to a jump table,
//! not a `{id, name}` array), so the provenance of each *number* is called out
//! individually:
//!
//! - **`[stable]`** — long-settled values that every independent description of
//!   the protocol agrees on and that have not moved across firmware generations.
//!   Used with confidence for the control path.
//! - **`[APK]`** — recovered by disassembling the function that sends the command
//!   in `libBCSDKWrapper.so` and reading the command id it loads (the `w1`
//!   argument to the message builder). Cross-checked against a known anchor:
//!   `BaichuanDownloader::downloadSnap` loads `109`, which is the long-settled
//!   `SNAP` opcode, so the extraction is sound. These are as trustworthy as the
//!   `[stable]` set, short of a live device.
//! - **`[UNVERIFIED]`** — the number has *not* been confirmed. Either the sending
//!   function takes the id from its caller (so it is not an immediate to read) or
//!   it was not located. The command name is real; the integer is a guess.
//!
//! The playback-by-time V2 cluster and the calendar command were corrected from
//! earlier guesses this way — the guesses were off by hundreds, which is exactly
//! why they were not trusted. When a device becomes reachable, the remaining
//! `[UNVERIFIED]` numbers are the first thing to check: a wrong one fails as a flat
//! refusal from the device, not as a crash here.

/// A Baichuan command id as it travels in the message header.
pub type Cmd = u32;

// ---------------------------------------------------------------- control [stable]

/// Log in and open the session. Carries the credential nonce exchange.
pub const LOGIN: Cmd = 1;
/// Release the session.
pub const LOGOUT: Cmd = 2;
/// Start a live video stream for a channel.
pub const VIDEO: Cmd = 3;
/// Stop a live video stream.
pub const VIDEO_STOP: Cmd = 4;
/// Keep-alive ping on the control channel.
pub const PING: Cmd = 93;
/// The device's own identity and ability block.
pub const VERSION: Cmd = 80;
/// Enumerate abilities, which is how channel count and per-channel features are
/// learned.
pub const GET_ABILITY_SUPPORT: Cmd = 58;
/// A channel's on-screen-display settings, which carry its name
/// (`OsdChannelName/name`). Sent with [`super::wire::Message::for_channel`]. From
/// `BaichuanConfigurator::_config_read_osd_cfg`, which loads 44 (or 110, the
/// defaults); confirmed live on an RLN16-410.
pub const GET_OSD: Cmd = 44;
/// Ask the device to push motion and AI alarm events. Answered with an empty
/// `200`, then [`MOTION`] messages follow on their own.
pub const MOTION_REQUEST: Cmd = 31;
/// A pushed `AlarmEventList`: every channel's `status` (`MD` while there is motion,
/// else `none`) and `AItype` (`people`, `vehicle`, `dog_cat`… or `none`). Measured
/// on an RLN16-410: one arrives every few seconds after [`MOTION_REQUEST`], each a
/// full list of the channels.
pub const MOTION: Cmd = 33;
/// The per-channel connection list (`ChannelInfoList`). Pushed by the device right
/// after login, unasked — measured on an RLN16-410: `<state>connect</state>` for a
/// slot with a camera, `<state>none</state>` for an empty one.
pub const CHANNEL_INFO_LIST: Cmd = 145;
/// A full-resolution still over the control channel.
pub const SNAP: Cmd = 109;

// -------------------------------------------------------------------- ptz [stable]

pub const PTZ_CONTROL: Cmd = 18;
pub const PTZ_CONTROL_PRESET: Cmd = 19;
pub const GET_PTZ_PRESET: Cmd = 190;

// ----------------------------------------------------------------- playback
//
// The commands the Playback tab needs over the P2P transport. The V2 playback
// cluster and the calendar command were read out of the binary (see the module
// note); the search and download opcodes are not yet pinned.

/// Which days in a month hold recordings — fills the calendar dots.
/// From `BaichuanReplayer::playbackGetDates`, which loads `142` as the command id.
pub const GET_RECFILEDATE: Cmd = 142; // [APK]

/// Open playback of recorded video addressed by time (V2 — the variant the app
/// uses). From `BaichuanReplayer::playbackStreamOpenV2`.
pub const PLAYBACK_BY_TIME_V2: Cmd = 381; // [APK]
/// Stop an open V2 playback. From `BaichuanReplayer::playbackStreamCloseV2`.
pub const PLAYBACK_BY_TIME_STOP_V2: Cmd = 382; // [APK]
/// Seek within an open V2 playback. From `BaichuanReplayer::playbackSeekToV2`.
pub const PLAYBACK_BY_TIME_SEEK_V2: Cmd = 383; // [APK]
/// Seek within a legacy (non-V2) playback. From `BaichuanReplayer::playbackSeekTo`.
pub const PLAYBACK_SEEK: Cmd = 123; // [APK]

/// Recording search, in three steps: open a search (`findAlarmVideo` with the
/// channel, stream and time range; the reply gives a `fileHandle`), fetch a page of
/// up to 50 clips with that handle (`alarmVideoInfo`, with `bFinished`), and close
/// it. The numbers come from reolink_aio's Baichuan client and were confirmed live
/// on an RLN16-410. The app's own search path takes its command id from a struct
/// field, which is why disassembly could not recover it.
pub const FIND_ALARM_VIDEO_OPEN: Cmd = 272;
pub const FIND_ALARM_VIDEO_NEXT: Cmd = 273;
pub const FIND_ALARM_VIDEO_CLOSE: Cmd = 274;
/// Begin playback addressed by file name (legacy). Not located in the binary.
pub const PLAYBACK_BY_NAME: Cmd = 260; // [UNVERIFIED]

/// Download a recording by time: as fast as the link allows, where replay is paced
/// at real time. From `BaichuanDownloader::downloadFileByTime` (loads 143, beside
/// `downloadSnap` loading the known 109) and confirmed live: a `FileInfoList` size
/// reply, the footage as BCMedia, then a bodiless `300` when it is all sent.
pub const DOWNLOAD_BY_TIME: Cmd = 143; // [APK]
/// Abandon a download by time (`stopDownloadFileByTime`).
pub const DOWNLOAD_BY_TIME_STOP: Cmd = 144; // [APK]

// There are deliberately no other download commands here. Guesses of 264/265/266 sat here
// once, and reolink_aio's Baichuan client shows 264 and 265 are *get and set the
// audio settings* — a "download" sent as 265 would have rewritten them. Recordings
// are fetched by replaying them instead (see `super::replay`).

/// Whether a command number is still an unverified guess, so higher layers can
/// label results and remind in logs. The `[APK]`-recovered opcodes are not listed
/// here — they are as trusted as the stable control path.
pub fn is_unverified(cmd: Cmd) -> bool {
    matches!(
        cmd,
        PLAYBACK_BY_NAME
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_opcodes_are_the_settled_values() {
        // These are pinned deliberately: they are the stable control path and a
        // silent edit would break every session.
        assert_eq!(LOGIN, 1);
        assert_eq!(LOGOUT, 2);
        assert_eq!(VIDEO, 3);
        assert_eq!(PING, 93);
    }

    /// The APK-recovered opcodes form a clean consecutive cluster for the V2
    /// playback trio — pinned so a careless edit that collides them is caught.
    #[test]
    fn apk_recovered_playback_opcodes_are_the_disassembled_values() {
        assert_eq!(GET_RECFILEDATE, 142);
        assert_eq!(PLAYBACK_BY_TIME_V2, 381);
        assert_eq!(PLAYBACK_BY_TIME_STOP_V2, 382);
        assert_eq!(PLAYBACK_BY_TIME_SEEK_V2, 383);
        assert_eq!(PLAYBACK_SEEK, 123);
    }

    #[test]
    fn only_the_unpinned_opcodes_are_flagged_unverified() {
        // Still guesses.
        assert!(is_unverified(PLAYBACK_BY_NAME));
        // Confirmed live.
        assert!(!is_unverified(FIND_ALARM_VIDEO_OPEN));
        // Recovered from the binary — trusted, so not flagged.
        assert!(!is_unverified(PLAYBACK_BY_TIME_V2));
        assert!(!is_unverified(GET_RECFILEDATE));
        assert!(!is_unverified(LOGIN));
        assert!(!is_unverified(PTZ_CONTROL));
    }
}
