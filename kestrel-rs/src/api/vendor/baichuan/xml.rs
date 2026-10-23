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

/// What a channel can do, out of a `GET_SUPPORT` reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Abilities {
    pub pan_tilt: bool,
    pub zoom: bool,
    pub presets: bool,
    pub floodlight: bool,
}

/// Each channel's abilities, by channel. `ptzType` says which motors there are —
/// reolink_aio reads 1, 2 and 5 as zoom, and 2, 3, 5, 6 and 7 as pan and tilt —
/// and presets need pan and tilt as well as `ptzPreset`. `ledCtrl` is a bit set:
/// a floodlight has bits 1 and 2 both. A sub-item (a second lens) is skipped; its
/// abilities are not the channel's.
pub fn channel_abilities(xml: &str) -> Vec<(u32, Abilities)> {
    xml.split("<item>")
        .skip(1)
        .filter_map(|item| {
            let item = item.split("<subItem>").next().unwrap_or(item);
            let number = |tag: &str| tag_text(item, tag).and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0);
            let channel = tag_text(item, "chnID")?.trim().parse().ok()?;
            let kind = number("ptzType");
            let pan_tilt = matches!(kind, 2 | 3 | 5 | 6 | 7);
            Some((
                channel,
                Abilities {
                    pan_tilt,
                    zoom: matches!(kind, 1 | 2 | 5),
                    presets: pan_tilt && number("ptzPreset") > 0,
                    floodlight: number("ledCtrl") & 0b110 == 0b110,
                },
            ))
        })
        .collect()
}

/// Switch the floodlight on or off by hand. `duration` is in seconds; reolink_aio
/// always sends 180, and what a device does when it runs out has not been seen.
pub fn floodlight_manual(channel: u32, on: bool) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><FloodlightManual version=\"1.1\"><channelId>{channel}</channelId>\
         <status>{}</status><duration>180</duration></FloodlightManual></body>\n",
        u8::from(on)
    )
}

/// The brightness in a `FloodlightTask` (`GET_FLOODLIGHT_TASK`'s reply).
pub fn floodlight_brightness(xml: &str) -> Option<i64> {
    tag_text(xml, "brightness_cur")?.trim().parse().ok()
}

/// A `FloodlightTask` with its brightness changed and everything else kept, to
/// write back with `SET_FLOODLIGHT_TASK` — the schedule and the detection
/// lighting ride in the same block.
pub fn with_floodlight_brightness(task: &str, bright: i64) -> Option<String> {
    let (open, close) = ("<brightness_cur>", "</brightness_cur>");
    let start = task.find(open)? + open.len();
    let end = start + task[start..].find(close)?;
    Some(format!("{}{bright}{}", &task[..start], &task[end..]))
}

/// The on/off state of each channel's floodlight in a pushed
/// `FloodlightStatusList`.
pub fn floodlight_states(xml: &str) -> Vec<(u32, bool)> {
    xml.split("<FloodlightStatus")
        .skip(1)
        .filter(|status| !status.starts_with("List"))
        .filter_map(|status| {
            let channel = tag_text(status, "channel").or_else(|| tag_text(status, "channelId"))?;
            let on = tag_text(status, "status")?.trim() != "0";
            Some((channel.trim().parse().ok()?, on))
        })
        .collect()
}

/// Start a move (or zoom, or focus) at `speed`, or `stop`.
pub fn ptz_control(channel: u32, command: &str, speed: i64) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><PtzControl version=\"1.1\"><channelId>{channel}</channelId>\
         <speed>{speed}</speed><command>{command}</command></PtzControl></body>\n"
    )
}

/// Go to a stored preset.
pub fn ptz_goto_preset(channel: u32, preset: i64) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><PtzPreset version=\"1.1\"><channelId>{channel}</channelId><presetList>\
         <preset><id>{preset}</id><command>toPos</command></preset></presetList></PtzPreset></body>\n"
    )
}

/// The stored presets in a `GET_PTZ_PRESET` reply. A slot marked disabled, or
/// with no name, is an empty one.
pub fn ptz_presets(xml: &str) -> Vec<(i64, String)> {
    xml.split("<preset>")
        .skip(1)
        .filter_map(|preset| {
            if tag_text(preset, "enable").is_some_and(|e| e.trim() == "0") {
                return None;
            }
            let id = tag_text(preset, "id")?.trim().parse().ok()?;
            let name = tag_text(preset, "name")?.trim().to_string();
            (!name.is_empty()).then_some((id, name))
        })
        .collect()
}

/// Go to the guard position, keeping its settings as `current` (the
/// `GET_GUARD` reply) has them, as reolink_aio does.
pub fn ptz_to_guard(channel: u32, current: &str) -> String {
    let enable = tag_text(current, "benable").unwrap_or_else(|| "1".into());
    let timeout = tag_text(current, "timeout").unwrap_or_else(|| "60".into());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><PtzGuard version=\"1.1\"><channelId>{channel}</channelId>\
         <benable>{enable}</benable><command>toGrd</command><timeout>{timeout}</timeout>\
         <needSetPos>0</needSetPos><imageName></imageName></PtzGuard></body>\n",
        enable = enable.trim(),
        timeout = timeout.trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_abilities_per_channel() {
        let xml = "<body><SupportAbility version=\"1.1\">\
            <item><chnID>0</chnID><ptzType>0</ptzType><ptzPreset>0</ptzPreset><ledCtrl>6</ledCtrl></item>\
            <item><chnID>1</chnID><ptzType>2</ptzType><ptzPreset>1</ptzPreset><ledCtrl>1</ledCtrl></item>\
            <item><chnID>2</chnID><ptzType>3</ptzType><ptzPreset>0</ptzPreset></item>\
            <item><chnID>3</chnID><ptzType>1</ptzType>\
              <subItem><chnID>0</chnID><ptzType>2</ptzType><ledCtrl>6</ledCtrl></subItem></item>\
            </SupportAbility></body>";
        let none = Abilities::default();
        assert_eq!(
            channel_abilities(xml),
            vec![
                (0, Abilities { floodlight: true, ..none }),
                (1, Abilities { pan_tilt: true, zoom: true, presets: true, ..none }),
                (2, Abilities { pan_tilt: true, ..none }),
                (3, Abilities { zoom: true, ..none }),
            ]
        );
    }

    #[test]
    fn floodlight_brightness_is_changed_in_place() {
        let task = "<body><FloodlightTask version=\"1.1\"><channel>0</channel><alarmMode>1</alarmMode>\
            <brightness_cur>40</brightness_cur><duration>300</duration></FloodlightTask></body>";
        assert_eq!(floodlight_brightness(task), Some(40));
        let changed = with_floodlight_brightness(task, 85).unwrap();
        assert_eq!(changed, task.replace(">40<", ">85<"), "nothing else moves");
        assert_eq!(with_floodlight_brightness("<body/>", 85), None);
        assert!(floodlight_manual(3, true).contains("<channelId>3</channelId><status>1</status>"));
    }

    #[test]
    fn reads_pushed_floodlight_states() {
        let xml = "<body><FloodlightStatusList version=\"1.1\">\
            <FloodlightStatus><channel>0</channel><status>1</status></FloodlightStatus>\
            <FloodlightStatus version=\"1.1\"><channelId>2</channelId><status>0</status></FloodlightStatus>\
            </FloodlightStatusList></body>";
        let mut states = floodlight_states(xml);
        states.sort();
        assert_eq!(states, vec![(0, true), (2, false)]);
    }

    #[test]
    fn ptz_requests_use_the_sdk_words() {
        let body = ptz_control(4, "leftUp", 32);
        assert!(body.contains("<channelId>4</channelId>"));
        assert!(body.contains("<command>leftUp</command>"));
        assert!(body.contains("<speed>32</speed>"));
        let body = ptz_goto_preset(4, 7);
        assert!(body.contains("<id>7</id><command>toPos</command>"));
    }

    #[test]
    fn reads_only_the_stored_presets() {
        let xml = "<body><PtzPreset version=\"1.1\"><channelId>0</channelId><presetList>\
            <preset><id>1</id><name>Gate</name></preset>\
            <preset><id>2</id><name></name></preset>\
            <preset><id>3</id><enable>0</enable><name>Old</name></preset>\
            <preset><id>4</id><enable>1</enable><name>Drive</name></preset>\
            </presetList></PtzPreset></body>";
        assert_eq!(ptz_presets(xml), vec![(1, "Gate".to_string()), (4, "Drive".to_string())]);
    }

    #[test]
    fn going_home_keeps_the_guard_settings() {
        let body = ptz_to_guard(2, "<PtzGuard><benable>0</benable><timeout>15</timeout></PtzGuard>");
        assert!(body.contains("<benable>0</benable><command>toGrd</command><timeout>15</timeout>"));
        let body = ptz_to_guard(2, "");
        assert!(body.contains("<benable>1</benable>"), "defaults when the guard could not be read");
    }

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
