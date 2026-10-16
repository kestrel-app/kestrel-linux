//! The small XML bodies the control commands carry.
//!
//! Baichuan bodies are terse XML. Only the handful of shapes Kestrel actually
//! sends are built here, and only the few fields it reads are parsed — the same
//! read-by-locating approach [`super::udp`] uses, for the same reason: a full
//! XML parser would be more than these fixed shapes earn, and the project already
//! takes this approach for QNAP's login response.
//!
//! **Verification status.** The *shapes* here (element names and nesting) follow
//! the official app and are the current best understanding; they have not been
//! confirmed against a device. The login digest formula in [`super::login`] is
//! the single most uncertain part and is marked there.

/// Pull the text of `<tag>…</tag>` out of a body, trimmed. `None` if absent.
pub fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

/// The login nonce out of the device's first reply (`<Encryption><nonce>…`).
pub fn login_nonce(xml: &str) -> Option<String> {
    tag_text(xml, "nonce")
}

/// The modern login body: the nonce-salted credential hashes.
///
/// Leg 1 is header-only (no XML), so there is no request builder for it; this is
/// the leg-2 body. `user_hash` and `pass_hash` are computed by [`super::login`].
pub fn login_auth(user_hash: &str, pass_hash: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><LoginUser version=\"1.1\"><userName>{user_hash}</userName>\
         <password>{pass_hash}</password><userVer>1</userVer></LoginUser>\
         <LoginNet version=\"1.1\"><type>LAN</type><udpPort>0</udpPort></LoginNet></body>\n"
    )
}

/// `<year>…<second>` for one moment, as the recording commands spell a time.
fn time_fields(when: chrono::NaiveDateTime) -> String {
    use chrono::{Datelike, Timelike};
    format!(
        "<year>{}</year><month>{}</month><day>{}</day><hour>{}</hour><minute>{}</minute><second>{}</second>",
        when.year(),
        when.month(),
        when.day(),
        when.hour(),
        when.minute(),
        when.second()
    )
}

/// `GET_RECFILEDATE`: which days in a range have recordings. The shape is the app's
/// own `DayRecords` builder; confirmed live (a month asked for, the recorded days
/// answered as offsets from its first day).
pub fn recorded_days_request(channel: u32, start: chrono::NaiveDateTime, end: chrono::NaiveDateTime) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<DayRecords version=\"1.1\">\n\
         <startTime>{}</startTime>\n<endTime>{}</endTime>\n\
         <DayRecordList>\n<DayRecord>\n<channelId>{channel}</channelId>\n</DayRecord>\n</DayRecordList>\n\
         </DayRecords>\n</body>\n",
        time_fields(start),
        time_fields(end)
    )
}

/// Open a recording search over a range: every kind of clip the NVR files. The
/// `uid` is left empty — on an NVR the channel decides, measured alike with the
/// NVR's UID, the channel's, and none.
pub fn find_alarm_video_open(
    channel: u32,
    stream: crate::api::models::StreamType,
    start: chrono::NaiveDateTime,
    end: chrono::NaiveDateTime,
) -> String {
    let stream = u8::from(stream == crate::api::models::StreamType::Sub);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<findAlarmVideo version=\"1.1\">\n\
         <channelId>{channel}</channelId>\n<uid></uid>\n<logicChnBitmap>255</logicChnBitmap>\n\
         <streamType>{stream}</streamType>\n<notSearchVideo>0</notSearchVideo>\n\
         <startTime>{}</startTime>\n<endTime>{}</endTime>\n\
         <alarmType>md, pir, io, people, face, vehicle, dog_cat, visitor, other, package, cry, crossline, intrusion, loitering, legacy, loss</alarmType>\n\
         </findAlarmVideo>\n</body>\n",
        time_fields(start),
        time_fields(end)
    )
}

/// Fetch the next page of, or close, the search a handle names.
pub fn find_alarm_video_handle(channel: u32, handle: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<findAlarmVideo version=\"1.1\">\n\
         <channelId>{channel}</channelId>\n<fileHandle>{handle}</fileHandle>\n</findAlarmVideo>\n</body>\n"
    )
}

/// A `<StartTime>`-style time block, named `tag`, for the playback bodies.
fn time_block(tag: &str, when: chrono::NaiveDateTime) -> String {
    use chrono::{Datelike, Timelike};
    format!(
        "<{tag}><year>{y}</year><month>{mo}</month><day>{d}</day>\
         <hour>{h}</hour><minute>{mi}</minute><second>{s}</second></{tag}>",
        y = when.year(), mo = when.month(), d = when.day(),
        h = when.hour(), mi = when.minute(), s = when.second(),
    )
}

/// Open playback of the recording covering `start`, by time (the V2 shape).
pub fn playback_open_request(channel: u32, stream: &str, start: chrono::NaiveDateTime) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><Playback><channelId>{channel}</channelId><streamType>{stream}</streamType>\
         {start_block}</Playback></body>\n",
        start_block = time_block("StartTime", start),
    )
}

/// Seek an open playback to `to`.
pub fn playback_seek_request(channel: u32, stream: &str, to: chrono::NaiveDateTime) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><Playback><channelId>{channel}</channelId><streamType>{stream}</streamType>\
         {seek_block}</Playback></body>\n",
        seek_block = time_block("SeekTime", to),
    )
}

/// Stop an open playback on a channel.
pub fn playback_stop_request(channel: u32, stream: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><Playback><channelId>{channel}</channelId><streamType>{stream}</streamType>\
         </Playback></body>\n"
    )
}

/// The channels a `ChannelInfoList` reports as having a camera connected.
pub fn connected_channels(xml: &str) -> Vec<u32> {
    xml.split("<ChannelInfo>")
        .skip(1)
        .filter(|info| tag_text(info, "state").as_deref() == Some("connect"))
        .filter_map(|info| tag_text(info, "channelId")?.parse().ok())
        .collect()
}

/// One channel's entry in a pushed `AlarmEventList`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmEvent {
    pub channel: u32,
    pub motion: bool,
    /// What the AI sees, e.g. `people`; empty for `none`.
    pub ai: Vec<String>,
}

pub fn alarm_events(xml: &str) -> Vec<AlarmEvent> {
    xml.split("<AlarmEvent")
        .skip(1)
        .filter_map(|event| {
            let channel = tag_text(event, "channelId")?.parse().ok()?;
            let motion = tag_text(event, "status").as_deref() == Some("MD");
            let ai = tag_text(event, "AItype")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|kind| !kind.is_empty() && *kind != "none")
                .map(str::to_string)
                .collect();
            Some(AlarmEvent { channel, motion, ai })
        })
        .collect()
}

/// The channel's name out of a `GetOsd` reply, if it has one.
pub fn osd_channel_name(xml: &str) -> Option<String> {
    let block = xml.split("<OsdChannelName").nth(1)?;
    tag_text(block, "name").map(|n| n.trim().to_string()).filter(|n| !n.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_tag_and_the_login_nonce() {
        let xml = "<body><Encryption><type>md5</type><nonce>abc123</nonce></Encryption></body>";
        assert_eq!(tag_text(xml, "type").as_deref(), Some("md5"));
        assert_eq!(login_nonce(xml).as_deref(), Some("abc123"));
        assert_eq!(tag_text(xml, "missing"), None);
    }

    #[test]
    fn login_body_carries_the_hashes() {
        let auth = login_auth("USERHASH", "PASSHASH");
        assert!(auth.contains("<userName>USERHASH</userName>"));
        assert!(auth.contains("<password>PASSHASH</password>"));
        assert!(auth.contains("<LoginNet"));
    }

    #[test]
    fn search_body_places_both_endpoints() {
        let start = chrono::NaiveDate::from_ymd_opt(2026, 10, 2)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2026, 10, 2)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap();
        let body = find_alarm_video_open(1, crate::api::models::StreamType::Sub, start, end);
        assert!(body.contains("<channelId>1</channelId>"));
        assert!(body.contains("<streamType>1</streamType>"), "sub is 1");
        assert!(body.contains("<startTime><year>2026</year><month>10</month><day>2</day><hour>0</hour>"));
        assert!(body.contains("<endTime><year>2026</year><month>10</month><day>2</day><hour>23</hour>"));
        let days = recorded_days_request(1, start, end);
        assert!(days.contains("<DayRecord>\n<channelId>1</channelId>"));
    }

    #[test]
    fn playback_open_seek_and_stop_bodies() {
        let t = chrono::NaiveDate::from_ymd_opt(2026, 10, 2)
            .unwrap()
            .and_hms_opt(8, 30, 15)
            .unwrap();
        let open = playback_open_request(1, "main", t);
        assert!(open.contains("<channelId>1</channelId>"));
        assert!(open.contains("<StartTime>"));
        assert!(open.contains("<hour>8</hour>"));

        let seek = playback_seek_request(1, "main", t);
        assert!(seek.contains("<SeekTime>"));
        assert!(seek.contains("<second>15</second>"));

        let stop = playback_stop_request(2, "sub");
        assert!(stop.contains("<channelId>2</channelId>"));
        assert!(stop.contains("<streamType>sub</streamType>"));
        assert!(!stop.contains("Time>"), "stop carries no time block");
    }


    /// Anchored to the shape an RLN16-410 pushed: seven cameras on twenty-four
    /// channels, the empty ones reported as `none`.
    #[test]
    fn reads_which_channels_have_a_camera() {
        let xml = "<body><ChannelInfoList version=\"1.1\">\
            <ChannelInfo><channelId>0</channelId><state>connect</state><uid></uid></ChannelInfo>\
            <ChannelInfo><channelId>6</channelId><state>connect</state><uid>9527000TESTUID00</uid></ChannelInfo>\
            <ChannelInfo><channelId>7</channelId><state>none</state></ChannelInfo>\
            </ChannelInfoList></body>";
        assert_eq!(connected_channels(xml), vec![0, 6]);
    }

    /// Anchored to what an RLN16-410 pushed: a person in view on channel 3, and
    /// motion on channel 1.
    #[test]
    fn reads_motion_and_ai_per_channel() {
        let xml = "<body><AlarmEventList version=\"1.1\">\
            <AlarmEvent version=\"1.1\"><channelId>0</channelId><status>none</status><AItype>none</AItype><recording>0</recording></AlarmEvent>\
            <AlarmEvent version=\"1.1\"><channelId>1</channelId><status>MD</status><AItype>none</AItype></AlarmEvent>\
            <AlarmEvent version=\"1.1\"><channelId>3</channelId><status>none</status><AItype>people,vehicle</AItype></AlarmEvent>\
            </AlarmEventList></body>";
        let events = alarm_events(xml);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], AlarmEvent { channel: 0, motion: false, ai: vec![] });
        assert!(events[1].motion);
        assert_eq!(events[2].ai, ["people", "vehicle"]);
    }

    /// Anchored to an RLN16-410's reply: the date block comes first and has no
    /// name, the channel-name block carries it.
    #[test]
    fn reads_the_channel_name_from_the_osd() {
        let xml = "<body><OsdDatetime version=\"1.1\"><channelId>3</channelId><enable>1</enable></OsdDatetime>\
            <OsdChannelName version=\"1.1\"><channelId>3</channelId><name>Back Gate</name><enable>1</enable></OsdChannelName></body>";
        assert_eq!(osd_channel_name(xml).as_deref(), Some("Back Gate"));
        assert_eq!(osd_channel_name("<body><OsdDatetime/></body>"), None);
    }
}
