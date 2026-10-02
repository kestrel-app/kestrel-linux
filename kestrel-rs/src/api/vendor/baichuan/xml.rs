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

/// Ask the device which days of a month hold recordings for a channel.
pub fn recorded_days_request(channel: u32, year: i32, month: u32, stream: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><Search><channelId>{channel}</channelId><streamType>{stream}</streamType>\
         <onlyStatus>1</onlyStatus><StartTime><year>{year}</year><month>{month}</month>\
         <day>1</day></StartTime></Search></body>\n"
    )
}

/// Search the recording index over a time range.
#[allow(clippy::too_many_arguments)]
pub fn search_request(
    channel: u32,
    stream: &str,
    start: chrono::NaiveDateTime,
    end: chrono::NaiveDateTime,
) -> String {
    use chrono::{Datelike, Timelike};
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
         <body><Search><channelId>{channel}</channelId><streamType>{stream}</streamType>\
         <onlyStatus>0</onlyStatus>\
         <StartTime><year>{sy}</year><month>{smo}</month><day>{sd}</day>\
         <hour>{sh}</hour><minute>{smi}</minute><second>{ss}</second></StartTime>\
         <EndTime><year>{ey}</year><month>{emo}</month><day>{ed}</day>\
         <hour>{eh}</hour><minute>{emi}</minute><second>{es}</second></EndTime>\
         </Search></body>\n",
        sy = start.year(), smo = start.month(), sd = start.day(),
        sh = start.hour(), smi = start.minute(), ss = start.second(),
        ey = end.year(), emo = end.month(), ed = end.day(),
        eh = end.hour(), emi = end.minute(), es = end.second(),
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
        let body = search_request(1, "main", start, end);
        assert!(body.contains("<channelId>1</channelId>"));
        assert!(body.contains("<year>2026</year>"));
        assert!(body.contains("<hour>23</hour>"));
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

    #[test]
    fn recorded_days_request_is_status_only() {
        let body = recorded_days_request(2, 2026, 10, "sub");
        assert!(body.contains("<onlyStatus>1</onlyStatus>"));
        assert!(body.contains("<streamType>sub</streamType>"));
        assert!(body.contains("<month>10</month>"));
    }
}
