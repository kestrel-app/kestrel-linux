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

use super::session::Session;
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

/// Parse a BC search reply body into recordings.
///
/// Each `<File>` block carries a start and end time and, on firmware that still
/// names clips, a `<name>`. Newer firmware indexes by time only, exactly as the
/// HTTP path already handles, so an absent name is not an error.
pub fn parse_search_result(channel: u32, stream: StreamType, body: &str) -> Vec<Recording> {
    let mut out = Vec::new();
    for file in blocks(body, "File") {
        let start = blocks(file, "StartTime")
            .first()
            .and_then(|b| parse_time_block(b));
        let end = blocks(file, "EndTime")
            .first()
            .and_then(|b| parse_time_block(b));
        let (Some(start), Some(end)) = (start, end) else {
            continue;
        };
        out.push(Recording {
            channel,
            start,
            end,
            name: xml::tag_text(file, "name").unwrap_or_default(),
            size: xml::tag_text(file, "size")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            stream_type: stream,
            width: 0,
            height: 0,
            frame_rate: 0,
            playback_time: blocks(file, "PlaybackTime")
                .first()
                .and_then(|b| parse_time_block(b)),
        });
    }
    out
}

/// Search the recording index over a time range.
///
/// The opcode here (`SEARCH_ALARM_VIDEOS`) is the one playback command still a
/// guess — the sending function takes it from a virtual caller, so it could not be
/// read from the binary. Everything else on this path is confirmed; a search that
/// comes back refused points at the opcode first.
pub fn search_recordings(
    session: &mut Session,
    channel: u32,
    start: NaiveDateTime,
    end: NaiveDateTime,
    stream: StreamType,
    timeout: Duration,
) -> Result<Vec<Recording>> {
    let reply = session.call(
        cmd::SEARCH_ALARM_VIDEOS,
        xml::search_request(channel, stream.as_str(), start, end),
        timeout,
    )?;
    Ok(parse_search_result(channel, stream, reply.xml()?))
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
///
/// Uses the APK-confirmed `GET_RECFILEDATE` opcode (142). The response *shape* —
/// the per-day status table parsed below — is still best-effort and is what a
/// capture would confirm.
pub fn recorded_days(
    session: &mut Session,
    channel: u32,
    month_of: NaiveDate,
    stream: StreamType,
    timeout: Duration,
) -> Result<Vec<u32>> {
    use chrono::Datelike;
    let reply = session.call(
        cmd::GET_RECFILEDATE,
        xml::recorded_days_request(channel, month_of.year(), month_of.month(), stream.as_str()),
        timeout,
    )?;
    // The status reply carries a per-day table; pull the days that are marked.
    let body = reply.xml()?;
    let mut days = Vec::new();
    for (i, status) in blocks(body, "table").first().unwrap_or(&"").chars().enumerate() {
        if status == '1' {
            days.push((i + 1) as u32);
        }
    }
    Ok(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_and_unnamed_recordings() {
        let body = "<body><SearchResult><channelId>0</channelId>\
            <File><name>Mp4Record/2026-10-02/x.mp4</name><size>1048576</size>\
             <StartTime><year>2026</year><month>10</month><day>2</day>\
              <hour>8</hour><minute>0</minute><second>0</second></StartTime>\
             <EndTime><year>2026</year><month>10</month><day>2</day>\
              <hour>8</hour><minute>5</minute><second>30</second></EndTime></File>\
            <File>\
             <StartTime><year>2026</year><month>10</month><day>2</day>\
              <hour>9</hour><minute>0</minute><second>0</second></StartTime>\
             <EndTime><year>2026</year><month>10</month><day>2</day>\
              <hour>9</hour><minute>1</minute><second>0</second></EndTime></File>\
            </SearchResult></body>";
        let recs = parse_search_result(0, StreamType::Main, body);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].name, "Mp4Record/2026-10-02/x.mp4");
        assert_eq!(recs[0].size, 1048576);
        assert!(recs[0].is_fetchable());
        assert_eq!(recs[0].duration_seconds(), 330);
        // The second has no name — indexed by time, like newer NVR firmware.
        assert!(!recs[1].is_fetchable());
    }

    #[test]
    fn a_file_missing_its_times_is_skipped_not_fatal() {
        let body = "<body><File><name>broken</name></File></body>";
        assert!(parse_search_result(0, StreamType::Main, body).is_empty());
    }

    #[test]
    fn parses_an_empty_result() {
        assert!(parse_search_result(0, StreamType::Sub, "<body><SearchResult/></body>").is_empty());
    }
}
