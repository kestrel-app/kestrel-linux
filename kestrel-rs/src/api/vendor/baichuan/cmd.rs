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

/// Search the recording index for a time range.
///
/// UNVERIFIED, and — unlike the rest — confirmed *not* statically recoverable.
/// The playback opcodes were read from the `w1` the sending method loads;
/// `BaichuanReplayer::rfsSearch` does no such thing. The full trace: rfsSearch
/// packs the command from a field at `BC_FILE_FIND+0x7c` and dispatches through a
/// `std::function` (resolved via `.rela.dyn` to lambdas at `0x47dbac`/`0x50287c`),
/// neither of which loads an opcode into `w1`. No inline immediate anywhere in
/// `libBCSDKWrapper.so` or `libJniAPI.so` writes that `0x7c` field, and the React
/// Native bundle calls native methods by name, carrying no opcode. So this number
/// can only come from a packet capture. The value below is a placeholder, **not**
/// a measurement — `128` was the size-looking immediate near rfsSearch and is more
/// likely a buffer length than the command. A refused search is expected until a
/// capture replaces it.
pub const SEARCH_ALARM_VIDEOS: Cmd = 128; // [UNVERIFIED — needs a capture, see above]
/// Begin playback addressed by file name (legacy). Not located in the binary.
pub const PLAYBACK_BY_NAME: Cmd = 260; // [UNVERIFIED]
/// Start a download of a recording by time. Not yet pinned.
pub const DOWNLOAD: Cmd = 264; // [UNVERIFIED]
/// Poll a running download's progress.
pub const DOWNLOAD_PROGRESS: Cmd = 265; // [UNVERIFIED]
/// Stop a running download.
pub const DOWNLOAD_STOP: Cmd = 266; // [UNVERIFIED]

/// Whether a command number is still an unverified guess, so higher layers can
/// label results and remind in logs. The `[APK]`-recovered opcodes are not listed
/// here — they are as trusted as the stable control path.
pub fn is_unverified(cmd: Cmd) -> bool {
    matches!(
        cmd,
        SEARCH_ALARM_VIDEOS | PLAYBACK_BY_NAME | DOWNLOAD | DOWNLOAD_PROGRESS | DOWNLOAD_STOP
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
        assert!(is_unverified(SEARCH_ALARM_VIDEOS));
        assert!(is_unverified(DOWNLOAD_STOP));
        // Recovered from the binary — trusted, so not flagged.
        assert!(!is_unverified(PLAYBACK_BY_TIME_V2));
        assert!(!is_unverified(GET_RECFILEDATE));
        assert!(!is_unverified(LOGIN));
        assert!(!is_unverified(PTZ_CONTROL));
    }
}
