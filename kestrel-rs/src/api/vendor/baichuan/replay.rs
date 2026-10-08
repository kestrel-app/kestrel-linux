//! Fetching a recording over Baichuan: downloaded, or failing that replayed.
//!
//! A recording is asked for by time, not by file: `ReplayByTimeV2` (381) names the
//! channel, stream and span, and the device answers with the footage as BCMedia —
//! the same `[extension][payload]` messages live video uses, under the replay's own
//! command id. Stop (382) carries no body. Both were read out of the app
//! (`BaichuanReplayer::playbackStreamOpenV2`'s request builder, and the bare
//! `simpleSndFunc(382)` in `playbackStreamCloseV2`) and the request confirmed live
//! on an RLN16-410: anything short of the full shape — the `durationList` with the
//! day and a `times` entry in seconds of that day — is refused with `400`.
//!
//! Each fetch opens a connection of its own, so a long download never holds up the
//! live tiles or the motion poll sharing the device's other connections.
//!
//! Because the request is by time, this also fetches what an NVR at an address
//! lists over HTTP without a file name — the recordings its API cannot serve.

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDateTime, Timelike};

use super::link::Reach;
use super::media::{demux, starts_with_frame, Frame, VideoCodec};
use super::cmd;
use crate::api::error::{Error, Result};
use crate::api::models::StreamType;

/// Replayed footage: the codec and each frame with the device's timestamp.
pub struct Footage {
    pub codec: VideoCodec,
    pub frames: Vec<(u32, Vec<u8>)>,
}

impl Footage {
    /// The rate the frames were recorded at, from their timestamps. Falls back to
    /// the recording's span when the timestamps say nothing useful.
    pub fn frame_rate(&self, span: Duration) -> f64 {
        let (first, last) = match (self.frames.first(), self.frames.last()) {
            (Some(f), Some(l)) if self.frames.len() > 1 => (f.0, l.0),
            _ => return 15.0,
        };
        let micros = last.wrapping_sub(first) as f64;
        let rate = (self.frames.len() - 1) as f64 / (micros / 1e6);
        if rate.is_finite() && (1.0..=60.0).contains(&rate) {
            rate
        } else {
            (self.frames.len() as f64 / span.as_secs_f64().max(1.0)).clamp(1.0, 60.0)
        }
    }

    /// The video as one elementary stream.
    pub fn elementary(&self) -> Vec<u8> {
        self.frames.iter().flat_map(|(_, data)| data.iter().copied()).collect()
    }
}

fn time_fields(at: NaiveDateTime) -> String {
    format!(
        "<year>{}</year><month>{}</month><day>{}</day><hour>{}</hour><minute>{}</minute><second>{}</second>",
        at.year(),
        at.month(),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// The `ReplayByTimeV2` body, in the app's own shape. A span is one day's: `s` and
/// `e` are seconds of that day, so a span past midnight is cut there.
pub fn replay_request(
    channel: u32,
    stream: StreamType,
    start: NaiveDateTime,
    end: NaiveDateTime,
    speed: u32,
) -> String {
    let end = end.min(start.date().and_hms_opt(23, 59, 59).unwrap_or(end));
    let name = match stream {
        StreamType::Main => "mainStream",
        StreamType::Sub => "subStream",
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<ReplayByTimeV2 version=\"1.1\">\n\
         <seq>0</seq>\n<startTime>{}</startTime>\n<endTime>{}</endTime>\n<playSpeed>{speed}</playSpeed>\n\
         <streamType>{name}</streamType>\n<durationList>\n<year>{}</year>\n<month>{}</month>\n<day>{}</day>\n\
         <duration>\n<channelId>{channel}</channelId>\n<logicChnBitmap>255</logicChnBitmap>\n\
         <streamType>{name}</streamType>\n<times>\n<i>\n<s>{}</s>\n<e>{}</e>\n</i>\n</times>\n</duration>\n\
         </durationList>\n</ReplayByTimeV2>\n</body>\n",
        time_fields(start),
        time_fields(end),
        start.year(),
        start.month(),
        start.day(),
        start.num_seconds_from_midnight(),
        end.num_seconds_from_midnight(),
        speed = speed.max(1),
    )
}

/// How long the device may go quiet, once it has sent something, before the
/// replay is taken as finished. A clip's end often comes a little short of the
/// span asked for, and nothing marks it.
const QUIET_END: Duration = Duration::from_secs(5);

/// The `FileInfoList` body asking to download `start..end`, in the shape of the
/// app's `downloadFileByTime` builder.
pub fn download_request(channel: u32, stream: StreamType, start: NaiveDateTime, end: NaiveDateTime) -> String {
    let name = match stream {
        StreamType::Main => "mainStream",
        StreamType::Sub => "subStream",
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<FileInfoList version=\"1.1\">\n<FileInfo>\n\
         <logicChnBitmap>255</logicChnBitmap>\n<channelId>{channel}</channelId>\n<supportSub>0</supportSub>\n\
         <streamType>{name}</streamType>\n<startTime>{}</startTime>\n<endTime>{}</endTime>\n\
         </FileInfo>\n</FileInfoList>\n</body>\n",
        time_fields(start),
        time_fields(end),
    )
}

/// How one request's footage ended.
enum Ended {
    /// The device said it had sent everything (a download's `300`), or the span
    /// is covered, or it went quiet after sending.
    Done,
    /// The device refused the request before sending anything.
    Refused(u16),
}

/// Collect the footage a fetch hears until it ends.
#[allow(clippy::too_many_arguments)]
fn collect(
    fetch: &super::hub::Fetch,
    span: Duration,
    timeout: Duration,
    paced: bool,
    frames: &mut Vec<(u32, Vec<u8>)>,
    codec: &mut Option<VideoCodec>,
    progress: &mut dyn FnMut(f32),
) -> Result<Ended> {
    use super::hub::FetchEvent;
    use std::sync::mpsc::RecvTimeoutError;

    let mut pending: Vec<u8> = Vec::new();
    let mut seen_keyframe = false;
    let began = Instant::now();
    let mut last_data = Instant::now();
    // A replay runs at real time, so it gets the span and then some; a download
    // is as fast as the link, but a slow link must still be let finish.
    let give_up = if paced { span * 4 + timeout * 3 } else { span * 8 + timeout * 6 };
    // A download ends with its 300, so silence only ends it after longer: a slow
    // link goes quiet for seconds mid-transfer.
    let quiet = if paced { QUIET_END } else { QUIET_END * 3 };
    loop {
        match fetch.events.recv_timeout(Duration::from_millis(250)) {
            Ok(FetchEvent::Status(300)) => return Ok(Ended::Done),
            Ok(FetchEvent::Status(code)) if code >= 400 && frames.is_empty() => {
                return Ok(Ended::Refused(code))
            }
            Ok(FetchEvent::Status(_)) => {}
            Ok(FetchEvent::Data(bcmedia)) => {
                // Footage, or the rest of a frame already begun. A message that is
                // neither — the header a replay or download opens with — would
                // stop the demuxer for good, so it is passed over.
                if !pending.is_empty() || starts_with_frame(&bcmedia) {
                    pending.extend_from_slice(&bcmedia);
                }
                last_data = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The connection went: what arrived is all there is.
            Err(RecvTimeoutError::Disconnected) => return Ok(Ended::Done),
        }
        let (parsed, used) = demux(&pending);
        pending.drain(..used);
        for frame in parsed {
            if let Frame::Video { codec: c, keyframe, micros, data } = frame {
                seen_keyframe |= keyframe;
                if seen_keyframe {
                    *codec = Some(c);
                    frames.push((micros, data));
                }
            }
        }

        let covered = match (frames.first(), frames.last()) {
            (Some(f), Some(l)) => Duration::from_micros(u64::from(l.0.wrapping_sub(f.0))),
            _ => Duration::ZERO,
        };
        progress((covered.as_secs_f32() / span.as_secs_f32()).min(0.99));
        if paced && covered >= span {
            return Ok(Ended::Done);
        }
        if !frames.is_empty() && last_data.elapsed() > quiet {
            return Ok(Ended::Done);
        }
        if began.elapsed() > give_up {
            return Ok(Ended::Done);
        }
    }
}

/// Fetch `start..end` of a channel and collect its frames. `progress` hears the
/// fraction of the span received, 0.0 to 1.0.
///
/// Downloads (143), which go as fast as the link allows; replays (381), paced at
/// real time, only for a device that refuses the download. Both ride the device's
/// shared connection (see [`super::hub`]).
#[allow(clippy::too_many_arguments)]
pub fn fetch(
    reach: &Reach,
    username: &str,
    password: &str,
    channel: u32,
    stream: StreamType,
    start: NaiveDateTime,
    end: NaiveDateTime,
    timeout: Duration,
    progress: &mut dyn FnMut(f32),
) -> Result<Footage> {
    use super::hub;
    let span = (end - start).to_std().unwrap_or_default().max(Duration::from_secs(1));
    let mut frames: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut codec = None;
    // Held across the download and any replay after it, so the connection does
    // not close between them.
    let _connection = hub::hold(reach, username, password, timeout)?;

    let download = hub::fetch(
        reach,
        username,
        password,
        cmd::DOWNLOAD_BY_TIME,
        cmd::DOWNLOAD_BY_TIME_STOP,
        channel,
        download_request(channel, stream, start, end),
        timeout,
    )?;
    let ended = collect(&download, span, timeout, false, &mut frames, &mut codec, progress)?;
    drop(download);

    if let Ended::Refused(code) = ended {
        log::info!("download refused ({code}); replaying the recording instead");
        let replay = hub::fetch(
            reach,
            username,
            password,
            cmd::PLAYBACK_BY_TIME_V2,
            cmd::PLAYBACK_BY_TIME_STOP_V2,
            channel,
            replay_request(channel, stream, start, end, 1),
            timeout,
        )?;
        collect(&replay, span, timeout, true, &mut frames, &mut codec, progress)?;
    }

    let codec = codec.ok_or_else(|| Error::connection("the device sent no footage for that time"))?;
    progress(1.0);
    Ok(Footage { codec, frames })
}

/// Replay `start..end` and save it as an MP4 at `out`.
#[allow(clippy::too_many_arguments)]
pub fn fetch_to_mp4(
    reach: &Reach,
    username: &str,
    password: &str,
    channel: u32,
    stream: StreamType,
    start: NaiveDateTime,
    end: NaiveDateTime,
    out: &Path,
    timeout: Duration,
    progress: &mut dyn FnMut(f32),
) -> Result<()> {
    let footage = fetch(reach, username, password, channel, stream, start, end, timeout, progress)?;
    let span = (end - start).to_std().unwrap_or_default();
    let raw = out.with_extension(match footage.codec {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "hevc",
    });
    std::fs::write(&raw, footage.elementary()).map_err(|e| Error::connection(format!("{}: {e}", raw.display())))?;
    let result = crate::video::export::to_mp4(&raw.to_string_lossy(), Some(footage.frame_rate(span)), out);
    let _ = std::fs::remove_file(&raw);
    result.map_err(Error::connection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(h: u32, m: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(h, m, s).unwrap()
    }

    /// The shape the device accepted: the day in `durationList`, and the span as
    /// seconds of that day.
    #[test]
    fn the_request_names_the_day_and_the_seconds_of_it() {
        let body = replay_request(3, StreamType::Sub, at(0, 0, 40), at(0, 1, 5), 1);
        assert!(body.contains("<durationList>\n<year>2026</year>\n<month>10</month>\n<day>5</day>"));
        assert!(body.contains("<channelId>3</channelId>"));
        assert!(body.contains("<streamType>subStream</streamType>"));
        assert!(body.contains("<s>40</s>\n<e>65</e>"));
    }

    #[test]
    fn a_span_past_midnight_is_cut_at_the_end_of_its_day() {
        let next_day = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap().and_hms_opt(0, 10, 0).unwrap();
        let body = replay_request(0, StreamType::Main, at(23, 59, 0), next_day, 1);
        assert!(body.contains("<e>86399</e>"));
    }

    #[test]
    fn the_frame_rate_comes_from_the_timestamps() {
        let frames = (0..31u32).map(|i| (1_000_000 + i * 66_667, vec![0u8])).collect();
        let footage = Footage { codec: VideoCodec::H264, frames };
        let rate = footage.frame_rate(Duration::from_secs(2));
        assert!((rate - 15.0).abs() < 0.1, "{rate}");
    }

    #[test]
    fn the_download_request_names_channel_stream_and_span() {
        let body = download_request(2, StreamType::Main, at(0, 0, 40), at(0, 1, 5));
        assert!(body.contains("<FileInfoList version=\"1.1\">\n<FileInfo>"));
        assert!(body.contains("<channelId>2</channelId>"));
        assert!(body.contains("<streamType>mainStream</streamType>"));
        assert!(body.contains("<startTime><year>2026</year><month>10</month><day>5</day><hour>0</hour><minute>0</minute><second>40</second></startTime>"));
    }

    /// Against the real device: replay a span into an MP4. Ignored; set
    /// KESTREL_TEST_UID / KESTREL_TEST_PASS, and KESTREL_TEST_REPLAY to
    /// "YYYY-mm-dd HH:MM:SS+secs" and KESTREL_TEST_OUT to the file to write.
    #[test]
    #[ignore]
    fn replays_a_real_recording_into_an_mp4() {
        let (Ok(uid), Ok(password), Ok(when), Ok(out)) = (
            std::env::var("KESTREL_TEST_UID"),
            std::env::var("KESTREL_TEST_PASS"),
            std::env::var("KESTREL_TEST_REPLAY"),
            std::env::var("KESTREL_TEST_OUT"),
        ) else {
            eprintln!("KESTREL_TEST_UID / _PASS / _REPLAY / _OUT not set");
            return;
        };
        let (start, secs) = when.split_once('+').unwrap();
        let start = NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M:%S").unwrap();
        let end = start + chrono::Duration::seconds(secs.parse().unwrap());
        // With a tile streaming, as on the wall: the fetch must share its connection.
        let live = std::env::var("KESTREL_TEST_WITH_LIVE").is_ok().then(|| {
            let sub = super::super::hub::subscribe(&Reach::Uid(uid.clone()), "admin", &password, 0, StreamType::Sub, Duration::from_secs(10)).unwrap();
            sub.codec.recv_timeout(Duration::from_secs(60)).expect("live video first");
            println!("live tile streaming");
            sub
        });
        let began = Instant::now();
        let mut last = 0.0;
        fetch_to_mp4(
            &Reach::Uid(uid.clone()),
            "admin",
            &password,
            0,
            StreamType::Sub,
            start,
            end,
            Path::new(&out),
            Duration::from_secs(10),
            &mut |p| last = p,
        )
        .expect("replay");
        println!("wrote {out} in {:?}; progress reached {last}", began.elapsed());
        drop(live);
    }
}
