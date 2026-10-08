//! Recording search and playback over the control channel.
//!
//! The HTTP/CGI Reolink vendor already searches and downloads recordings when the
//! device is reachable at an address (see [`super::super::reolink`]). This is the
//! same capability over the P2P transport, for a device addressed only by UID.
//!
//! **Verification status.** The result *parser* ([`parse_search_result`]) is pure
//! and tested below against a representative body. The playback-control opcodes
//! (open/seek/stop by time, and the calendar command) were recovered from the app
//! binary by disassembly — see [`cmd`] — so they are trusted; only the *search*
//! opcode is still a guess. The request path depends on an authenticated
//! [`Session`], which remains unverified for want of a device. See [`super`] and
//! `docs/untested.md`.

use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};

use super::session::{Calls, Session};
use super::{cmd, xml};
use crate::api::error::Result;
use crate::api::models::{Recording, StreamType};

/// Parse a `<StartTime>`/`<EndTime>` style block into a date-time.
fn parse_time_block(block: &str) -> Option<NaiveDateTime> {
    let field = |name: &str| xml::tag_text(block, name).and_then(|v| v.parse::<u32>().ok());
    let year = xml::tag_text(block, "year")?.parse::<i32>().ok()?;
    let date = NaiveDate::from_ymd_opt(year, field("month")?, field("day")?)?;
    date.and_hms_opt(
        field("hour").unwrap_or(0),
        field("minute").unwrap_or(0),
        field("second").unwrap_or(0),
    )
}

/// Extract each `<tag>…</tag>` block from `xml`, in order.
fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(s) = rest.find(&open) {
        let after = s + open.len();
        if let Some(e) = rest[after..].find(&close) {
            out.push(&rest[after..after + e]);
            rest = &rest[after + e + close.len()..];
        } else {
            break;
        }
    }
    out
}

/// One page of a search reply: its clips, and whether the device has no more.
///
/// Anchored to an RLN16-410's reply: `alarmVideoInfo` with `bFinished` and an
/// `alarmVideoList` of `alarmVideo`s, each with the event's own `startTime` and
/// `endTime`, its trigger in `alarmType` (`md`, `people`…), and a `fileName` of `01`
/// then the recording file's start, `YYYYMMDDhhmmss` — where playback by time
/// would begin.
pub fn parse_alarm_videos(channel: u32, stream: StreamType, body: &str) -> (Vec<Recording>, bool) {
    let finished = xml::tag_text(body, "bFinished").as_deref() != Some("0");
    let mut out = Vec::new();
    for video in blocks(body, "alarmVideo") {
        let start = blocks(video, "startTime").first().and_then(|b| parse_time_block(b));
        let end = blocks(video, "endTime").first().and_then(|b| parse_time_block(b));
        let (Some(start), Some(end)) = (start, end) else {
            continue;
        };
        // `01` + the recording file's start. Kept as the playback time; the name
        // itself is not an HTTP file name, so the recording carries none and is
        // only ever fetched by time.
        let file = xml::tag_text(video, "fileName").unwrap_or_default();
        let playback_time = file
            .get(2..)
            .and_then(|t| NaiveDateTime::parse_from_str(t, "%Y%m%d%H%M%S").ok());
        let name = String::new();
        out.push(Recording {
            channel,
            start,
            end,
            name,
            size: 0,
            stream_type: stream,
            width: 0,
            height: 0,
            frame_rate: 0,
            playback_time,
            triggers: {
                let mut kinds: Vec<crate::api::models::EventKind> = xml::tag_text(video, "alarmType")
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|k| crate::api::models::EventKind::from_alarm_type(k.trim()))
                    .collect();
                kinds.dedup();
                kinds
            },
        });
    }
    (out, finished)
}

/// The most pages one search will walk: 50 clips a page, so a busy day's worth.
const MAX_PAGES: usize = 40;

/// Search the recording index over a time range.
///
/// Opens a search, fetches its page, closes it, and — while the device says there
/// is more — opens the next from the last clip's start, as reolink_aio does.
/// Clips seen twice across a page boundary are kept once.
pub fn search_recordings(
    session: &mut dyn Calls,
    channel: u32,
    start: NaiveDateTime,
    end: NaiveDateTime,
    stream: StreamType,
    timeout: Duration,
) -> Result<Vec<Recording>> {
    let mut all: Vec<Recording> = Vec::new();
    let mut from = start;
    for _ in 0..MAX_PAGES {
        let open = session.call(
            cmd::FIND_ALARM_VIDEO_OPEN,
            xml::find_alarm_video_open(channel, stream, from, end),
            timeout,
        )?;
        let Some(handle) = xml::tag_text(open.xml()?, "fileHandle") else {
            break;
        };
        let handle_body = xml::find_alarm_video_handle(channel, &handle);
        let page = session.call(cmd::FIND_ALARM_VIDEO_NEXT, handle_body.clone(), timeout);
        let _ = session.call(cmd::FIND_ALARM_VIDEO_CLOSE, handle_body, timeout);
        let (clips, finished) = parse_alarm_videos(channel, stream, page?.xml()?);

        let last = clips.last().map(|c| c.start);
        for clip in clips {
            if !all.iter().any(|c| c.start == clip.start && c.end == clip.end) {
                all.push(clip);
            }
        }
        match last {
            Some(last) if !finished && last > from => from = last,
            _ => break,
        }
    }
    all.sort_by_key(|c| c.start);
    Ok(all)
}

/// Days with recordings in a `DayRecords` reply, as days of the month. Each
/// `dayType`'s `index` counts from the first day asked about.
pub fn parse_recorded_days(body: &str, first: NaiveDate) -> Vec<u32> {
    use chrono::Datelike;
    let mut days: Vec<u32> = blocks(body, "dayType")
        .iter()
        .filter_map(|d| xml::tag_text(d, "index")?.parse::<i64>().ok())
        .filter_map(|i| first.checked_add_signed(chrono::Duration::days(i)))
        .filter(|d| d.month() == first.month())
        .map(|d| d.day())
        .collect();
    days.sort_unstable();
    days.dedup();
    days
}

/// An open playback stream on a channel, used to seek and stop it.
///
/// Playback over P2P is stateful — open, then seek/stop the same stream — unlike
/// the HTTP path where each clip is a fresh fetch. This carries the little state a
/// caller needs to drive it.
#[derive(Debug, Clone, Copy)]
pub struct PlaybackHandle {
    pub channel: u32,
    pub stream: StreamType,
}

/// Open playback of the recording covering `start`, by time (V2).
///
/// Uses the APK-confirmed `PLAYBACK_BY_TIME_V2` opcode. The returned handle seeks
/// and stops the same stream.
pub fn open_playback(
    session: &mut Session,
    channel: u32,
    stream: StreamType,
    start: NaiveDateTime,
    timeout: Duration,
) -> Result<PlaybackHandle> {
    session.call(
        cmd::PLAYBACK_BY_TIME_V2,
        xml::playback_open_request(channel, stream.as_str(), start),
        timeout,
    )?;
    Ok(PlaybackHandle { channel, stream })
}

/// Seek an open playback to a new time (V2).
pub fn seek_playback(
    session: &mut Session,
    handle: PlaybackHandle,
    to: NaiveDateTime,
    timeout: Duration,
) -> Result<()> {
    session.call(
        cmd::PLAYBACK_BY_TIME_SEEK_V2,
        xml::playback_seek_request(handle.channel, handle.stream.as_str(), to),
        timeout,
    )?;
    Ok(())
}

/// Stop an open playback (V2).
pub fn stop_playback(
    session: &mut Session,
    handle: PlaybackHandle,
    timeout: Duration,
) -> Result<()> {
    session.call(
        cmd::PLAYBACK_BY_TIME_STOP_V2,
        xml::playback_stop_request(handle.channel, handle.stream.as_str()),
        timeout,
    )?;
    Ok(())
}

/// Which days of a month hold recordings, for the calendar dots.
pub fn recorded_days(
    session: &mut dyn Calls,
    channel: u32,
    month_of: NaiveDate,
    _stream: StreamType,
    timeout: Duration,
) -> Result<Vec<u32>> {
    use chrono::Datelike;
    let first = month_of.with_day(1).unwrap_or(month_of);
    let next = first.checked_add_months(chrono::Months::new(1)).unwrap_or(first);
    let last = next.pred_opt().unwrap_or(first);
    let reply = session.call(
        cmd::GET_RECFILEDATE,
        xml::recorded_days_request(
            channel,
            first.and_hms_opt(0, 0, 0).unwrap_or_default(),
            last.and_hms_opt(23, 59, 59).unwrap_or_default(),
        ),
        timeout,
    )?;
    Ok(parse_recorded_days(reply.xml()?, first))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like an RLN16-410's reply to a search fetch.
    const PAGE: &str = "<body><alarmVideoInfo version=\"1.1\"><channelId>0</channelId><bFinished>0</bFinished><alarmVideoList>\
        <alarmVideo><fileName>0120261004200000</fileName><bHasRecFile>1</bHasRecFile><alarmType>md</alarmType>\
         <startTime><year>2026</year><month>10</month><day>4</day><hour>20</hour><minute>6</minute><second>23</second></startTime>\
         <endTime><year>2026</year><month>10</month><day>4</day><hour>20</hour><minute>6</minute><second>34</second></endTime></alarmVideo>\
        <alarmVideo><fileName>0120261004200000</fileName><alarmType>people</alarmType>\
         <startTime><year>2026</year><month>10</month><day>4</day><hour>20</hour><minute>13</minute><second>5</second></startTime>\
         <endTime><year>2026</year><month>10</month><day>4</day><hour>20</hour><minute>13</minute><second>40</second></endTime></alarmVideo>\
        </alarmVideoList></alarmVideoInfo></body>";

    #[test]
    fn reads_a_page_of_clips() {
        let (clips, finished) = parse_alarm_videos(0, StreamType::Main, PAGE);
        assert!(!finished, "bFinished 0: there is more");
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].duration_seconds(), 11);
        let file_start = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap().and_hms_opt(20, 0, 0).unwrap();
        assert_eq!(clips[0].playback_time, Some(file_start), "the file's start, from its name");
        assert_eq!(clips[1].start.format("%H:%M:%S").to_string(), "20:13:05");
        use crate::api::models::EventKind;
        assert_eq!(clips[0].triggers, [EventKind::Motion], "md is motion");
        assert_eq!(clips[1].triggers, [EventKind::Person]);
    }

    #[test]
    fn a_finished_empty_page_ends_the_search() {
        let body = "<body><alarmVideoInfo><bFinished>1</bFinished><alarmVideoList></alarmVideoList></alarmVideoInfo></body>";
        let (clips, finished) = parse_alarm_videos(0, StreamType::Sub, body);
        assert!(clips.is_empty());
        assert!(finished);
    }

    /// Anchored to an RLN16-410's reply for October: offsets 0-4 from the 1st.
    #[test]
    fn reads_recorded_days_as_offsets_from_the_first() {
        let body = "<body><DayRecords version=\"1.1\"><DayRecordList><DayRecord><index>0</index><channelId>0</channelId><dayTypeList>\
            <dayType><index>0</index><type>normal</type></dayType><dayType><index>1</index><type>normal</type></dayType>\
            <dayType><index>4</index><type>alarm</type></dayType></dayTypeList></DayRecord></DayRecordList></DayRecords></body>";
        let first = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        assert_eq!(parse_recorded_days(body, first), vec![1, 2, 5]);
    }
}
