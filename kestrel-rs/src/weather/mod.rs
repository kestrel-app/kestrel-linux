//! Weather: the model every widget reads, and the sources that fill it.
//!
//! Ported from the Roku channel, which had the same two sources and the same
//! problem to solve. There are two places a reading can come from and one shape
//! they both produce, so nothing downstream knows or cares which it is looking
//! at and there is one set of rules about how a reading is written.
//!
//!   * [`weewx`] reads a weewx server on your own network — one HTTP GET
//!     against the JSON document weewx publishes for Home Assistant. No
//!     session, no credentials, no vendor dispatch: unlike the camera systems,
//!     every weewx install serves this the same way, so there is nothing here
//!     to abstract over.
//!   * [`nws`] reads api.weather.gov, for somebody with no station of their
//!     own. It builds this same model through the same formatters.
//!
//! Everything a station reports comes as `{ value, unit }`, with the unit
//! decided by the station's own configuration rather than by anything on this
//! end. So nothing here assumes Fahrenheit or inches — the reported unit is
//! formatted alongside the reading, and a metric station displays metric with
//! no setting to find.
//!
//! The parse happens on the poller thread and produces display strings rather
//! than numbers: a strip redrawing sixty times a second should not be rounding
//! floats and picking units.
//!
//! The one thing deliberately *not* formatted here is the clock. Whether a
//! household reads 22:47 or 10:47 PM is a preference, and baking it into the
//! model on a five-minute poll would mean changing the setting did nothing
//! until the next reading landed. So times are carried as [`Clock`] and written
//! out at draw time.

pub mod fill;
pub mod hazards;
pub mod nws;
pub mod poller;
pub mod radar;
pub mod reflectivity;
pub mod tiles;
pub mod warnings;
pub mod weewx;
pub mod zip;

use serde_json::Value;

// ---------------------------------------------------------------- the model

/// A time of day, kept apart from how it will be written.
///
/// Which timezone this is *in* is settled by whoever built it, and the two
/// sources answer differently: a weewx server stamps in its own local time and
/// that hour is left alone, because the station's clock is the one the reading
/// belongs to. The National Weather Service stamps in UTC, so that one is moved
/// to the machine's own zone — somebody is standing in front of this screen,
/// and a ZIP code three time zones away is a stranger case than a laptop on
/// holiday.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Clock {
    pub hours: u32,
    pub minutes: u32,
}

/// Watches and warnings, read defensively.
///
/// The field names come from the National Weather Service, by way of weewx in
/// one case and directly in the other. The list is empty except during the
/// weather that makes it matter — which is exactly when a wrong guess about its
/// shape would take the wall down. So each field is looked for under every name
/// it plausibly carries, and an entry that is a bare string rather than an
/// object is still shown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Alert {
    pub event: String,
    pub headline: String,
    pub severity: String,
    pub ends: String,
}

impl Alert {
    /// Whether this warrants the strongest treatment on screen.
    ///
    /// The severity vocabulary is fixed; anything outside it is treated as the
    /// milder case rather than assumed to be the worse one.
    pub fn is_severe(&self) -> bool {
        let severity = self.severity.to_ascii_lowercase();
        severity == "extreme" || severity == "severe"
    }

    /// The headline, with the event it opens by repeating taken off the front.
    ///
    /// Everywhere a headline is shown it sits beside the event it names, so
    /// what it adds is everything after that name.
    pub fn detail(&self) -> String {
        without_event(&self.headline, &self.event)
    }

    /// One line for a banner: the event, and the headline where it adds
    /// something.
    ///
    /// The headline usually repeats the event and then says by whom and until
    /// when. Repeating the event in the same strip wastes the width.
    pub fn line(&self) -> String {
        let said = self.detail();
        if said.is_empty() {
            self.event.clone()
        } else {
            format!("{}  ·  {}", self.event, said)
        }
    }
}

/// A headline with the hazard's own name taken off the front.
///
/// The service writes its headlines to stand alone — "Heat Advisory issued
/// September 2 at 1:00 PM CDT until September 3 at 8:00 PM CDT by NWS Fort
/// Worth TX" — but nothing on the wall shows one alone. It is always under or
/// beside a line that already says "Heat Advisory", and in a box four lines
/// tall the second telling costs a line that had something else to say. What
/// is left carries the whole of the difference: "Issued September 2 at ...".
///
/// Taken only off the front, only when it is the whole name, and only when a
/// word does not run on through it — a headline opening with anything else is
/// a headline saying something else, and is left as it was written. A headline
/// that is nothing but the name comes back empty, which is the honest answer:
/// the event line has already said all of it.
pub fn without_event(headline: &str, event: &str) -> String {
    let headline = headline.trim();
    let event = event.trim();
    if event.is_empty() {
        return headline.to_string();
    }
    match headline.get(..event.len()) {
        Some(lead) if lead.eq_ignore_ascii_case(event) => {}
        _ => return headline.to_string(),
    }

    let rest = &headline[event.len()..];
    // "Flood" against "Flooding": the same letters, a different word.
    if rest.starts_with(|c: char| c.is_alphanumeric()) {
        return headline.to_string();
    }

    let rest = rest
        .trim_start()
        .trim_start_matches([':', ';', ',', '-', '–', '—', '·'])
        .trim_start();
    // The sentence now starts where it used to continue, so it is capitalised
    // like one.
    let mut letters = rest.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

/// One forecast period, trimmed to what fits on a wall.
///
/// The detailed narrative is kept whole — the weather pane shows it for
/// whichever period is selected, and it is the only place the wording is worth
/// reading in full.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Period {
    pub name: String,
    pub short: String,
    pub detailed: String,
    pub temperature: String,
    pub precip: String,
    pub wind: String,
    pub condition: String,
    pub icon: Option<Icon>,
}

/// The shape every consumer reads.
///
/// [`Model::empty`] is what a widget built before the first reply lands
/// renders, so nothing has to branch on an absent reading.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    pub ok: bool,
    pub error: String,
    pub station: String,
    pub observed: Option<Clock>,
    pub temp_big: String,
    pub summary: String,
    pub condition_text: String,
    pub condition: String,
    pub feels: String,
    pub humidity: String,
    pub wind_text: String,
    pub pressure_text: String,
    pub high_low: String,
    pub rain_text: String,
    pub uv_text: String,
    pub dew_text: String,
    pub outlook_name: String,
    pub outlook_text: String,
    pub icon: Option<Icon>,
    pub periods: Vec<Period>,
    pub alerts: Vec<Alert>,
}

impl Model {
    pub fn empty() -> Self {
        Model::default()
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Model {
            ok: false,
            error: message.into(),
            ..Model::default()
        }
    }

    /// Every alert, end to end, for a banner that scrolls.
    ///
    /// This used to be the first alert and a count of the others — "(+2 more)"
    /// — which is the shape a fixed-width line forces on you and is very nearly
    /// the least useful thing to say. Two watches out at once is exactly when
    /// *which* two matters, and a running banner has room to say so: the width
    /// stops being the constraint the moment the text moves.
    pub fn alerts_line(&self, twenty_four: bool) -> String {
        let mut out = String::new();
        for alert in &self.alerts {
            let line = alert.line();
            if line.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push_str("        ·        ");
            }
            out.push_str(&line);
            let ends = ends_text(&alert.ends, twenty_four);
            if !ends.is_empty() {
                out.push_str("  ·  until ");
                out.push_str(&ends);
            }
        }
        out
    }

    /// Whether any of them is severe, which is what colours a banner carrying
    /// all of them. The strongest wins: a banner coloured for the mildest alert
    /// on it would be understating the worst.
    pub fn alerts_severe(&self) -> bool {
        self.alerts.iter().any(Alert::is_severe)
    }

    /// The readings the strip's middle column shows, in the order it shows
    /// them, skipping the sensors this station does not have.
    pub fn stats(&self) -> Vec<(&'static str, &str)> {
        [
            ("Feels like", self.feels.as_str()),
            ("Humidity", self.humidity.as_str()),
            ("Wind", self.wind_text.as_str()),
            ("Today", self.high_low.as_str()),
            ("Rain", self.rain_text.as_str()),
            ("Pressure", self.pressure_text.as_str()),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .collect()
    }

    /// The fuller list the weather pane shows, which has room for the two the
    /// strip leaves out.
    pub fn detailed_stats(&self) -> Vec<(&'static str, &str)> {
        [
            ("Humidity", self.humidity.as_str()),
            ("Dew point", self.dew_text.as_str()),
            ("Wind", self.wind_text.as_str()),
            ("Pressure", self.pressure_text.as_str()),
            ("Today", self.high_low.as_str()),
            ("Rain", self.rain_text.as_str()),
            ("UV index", self.uv_text.as_str()),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .collect()
    }
}

// ---------------------------------------------------------------- icons

/// Which glyph goes with a condition.
///
/// Drawn rather than shipped: the Roku channel carries a folder of PNGs because
/// a television draws Posters, and this one has a painter, so the same twelve
/// glyphs are strokes and arcs that stay sharp at any tile size. See
/// [`crate::ui::weather::paint_icon`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Storm,
    Snow,
    Sleet,
    Showers,
    Rain,
    Fog,
    Wind,
    PartlyDay,
    PartlyNight,
    Cloudy,
    ClearDay,
    ClearNight,
}

/// Which glyph a condition maps to, or `None` when nothing in the text is
/// recognisable and no picture is better than a wrong one.
///
/// Matched on substrings rather than equality, and against the forecast's own
/// words as well as the station's condition code, because the two vocabularies
/// come from different places: weewx sets `condition` from its own table, while
/// the National Weather Service writes English. "Patchy fog then sunny" has to
/// land on fog, and it does, because the order below is by what would matter
/// most if both were true — a thunderstorm outranks the cloud it arrives in.
pub fn icon_for(condition: &str, text: &str, night: bool) -> Option<Icon> {
    let key = format!("{} {}", condition.to_ascii_lowercase(), text.to_ascii_lowercase());
    if key.trim().is_empty() {
        return None;
    }
    let has = |needle: &str| key.contains(needle);

    if has("thunder") || has("storm") || has("lightning") {
        return Some(Icon::Storm);
    }
    if has("snow") || has("flurr") || has("blizzard") {
        return Some(Icon::Snow);
    }
    if has("sleet") || has("freezing") || has("hail") || has("ice pellets") {
        return Some(Icon::Sleet);
    }
    if has("shower") || has("drizzle") {
        return Some(Icon::Showers);
    }
    if has("rain") {
        return Some(Icon::Rain);
    }
    if has("fog") || has("mist") || has("haze") {
        return Some(Icon::Fog);
    }
    if has("wind") || has("breezy") || has("blustery") {
        return Some(Icon::Wind);
    }

    // Before the plain cloud tests: "partly cloudy" contains "cloudy".
    if has("partly") || has("mostly sunny") || has("mostly clear") {
        return Some(if night { Icon::PartlyNight } else { Icon::PartlyDay });
    }
    if has("cloud") || has("overcast") {
        return Some(Icon::Cloudy);
    }
    if has("clear") || has("sunny") || has("fair") {
        return Some(if night { Icon::ClearNight } else { Icon::ClearDay });
    }

    None
}

/// Whether a forecast period's name says it is dark out.
pub fn is_night(period_name: &str) -> bool {
    period_name.to_ascii_lowercase().contains("night")
}

// ---------------------------------------------------------------- readings

/// One `{ value, unit }` node as both sources publish it.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub value: f64,
    pub unit: String,
}

impl Reading {
    pub fn new(value: f64, unit: &str) -> Self {
        Reading {
            value,
            unit: unit.to_string(),
        }
    }

    /// Read one out of a document, or `None` when the station does not report
    /// it — not every weewx install has every sensor, and the service answers
    /// for a quiet station with a record full of nulls rather than an error.
    pub fn read(parent: &Value, key: &str) -> Option<Reading> {
        let node = parent.get(key)?;
        let value = as_f64(node.get("value")?)?;
        Some(Reading {
            value,
            unit: node
                .get("unit")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    /// This reading formatted with its own unit.
    pub fn text(&self, places: usize) -> String {
        format!("{}{}", fixed(self.value, places), unit_label(&self.unit))
    }
}

/// A number, however the document happens to spell it. weewx publishes floats;
/// a hand-written proxy in front of one has been known to publish strings.
pub fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The first of several field names that carries anything to show.
///
/// Trimmed, and a field of nothing but spaces counts as absent. The warnings
/// service pads its fields — an alert with no end time carries `Ends` as a
/// couple of spaces rather than as null — and untrimmed that reads as a value,
/// which put "until   ·  " under a warning that simply had no expiry.
pub fn pick(node: &Value, names: &[&str]) -> String {
    for name in names {
        if let Some(text) = node.get(*name).map(as_string) {
            let text = text.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

/// A JSON value as something to put on screen. A null, an object or an array is
/// nothing rather than its debug spelling.
pub fn as_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(whole) => whole.to_string(),
            None => fixed(n.as_f64().unwrap_or(0.0), 2),
        },
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- formatting

/// A mile, in kilometres.
///
/// The map is metric the whole way down and there is no changing that: Web
/// Mercator is in metres, the mosaic's bounding boxes are in metres, and the
/// span a view covers falls out of the grid in kilometres. So US customary is a
/// conversion at the last moment — on the dial and on the caption — rather than
/// a second set of numbers carried through the code, and what gets *stored* is
/// kilometres whichever way the setting is left.
pub const KM_PER_MILE: f64 = 1.609_344;

/// How far something reaches, in the units that were asked for.
///
/// Rounded to whole units on purpose. This is the width of a view rather than a
/// measurement of anything, and "137 mi across" is a caption while "136.8 mi
/// across" is a claim about where the edge of the screen is.
pub fn span_text(km: f64, metric: bool) -> String {
    if metric {
        format!("{km:.0} km")
    } else {
        format!("{:.0} mi", km / KM_PER_MILE)
    }
}

/// weewx names units in full — "degree_F", "mile_per_hour" — which is right for
/// a data feed and wrong for a tile. Anything unrecognised formats as the bare
/// number rather than showing the raw name.
pub fn unit_label(unit: &str) -> &'static str {
    match unit {
        "degree_F" => "°F",
        "degree_C" => "°C",
        "degree_K" => "K",
        "percent" => "%",
        "mile_per_hour" => " mph",
        "km_per_hour" => " km/h",
        "meter_per_second" => " m/s",
        "knot" => " kn",
        "inHg" => " inHg",
        "mbar" => " mbar",
        "hPa" => " hPa",
        "mmHg" => " mmHg",
        "inch" => " in",
        "cm" => " cm",
        "mm" => " mm",
        "inch_per_hour" => " in/h",
        "mm_per_hour" => " mm/h",
        "cm_per_hour" => " cm/h",
        "foot" => " ft",
        "meter" => " m",
        _ => "",
    }
}

/// The forecast condition keyword as something to read. The vocabulary is the
/// server's, so an unfamiliar one is capitalised and shown rather than dropped —
/// a word from the station beats a blank line.
pub fn condition_text(condition: &str) -> String {
    let key = condition.to_ascii_lowercase();
    let known = match key.as_str() {
        "" => return String::new(),
        "sunny" => "Sunny",
        "clear" => "Clear",
        "partly" | "partlycloudy" => "Partly cloudy",
        "mostlycloudy" => "Mostly cloudy",
        "cloudy" | "overcast" => "Cloudy",
        "fog" => "Fog",
        "rain" | "rainy" => "Rain",
        "showers" => "Showers",
        "storm" | "thunderstorm" | "lightning" => "Storms",
        "snow" | "snowy" => "Snow",
        "sleet" => "Sleet",
        "hail" => "Hail",
        "wind" | "windy" => "Windy",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    let mut chars = condition.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Fixed-point, guarded so a value that rounds to zero is not shown as
/// "-0.00" — which is what a barometer trending down by a thousandth produces.
pub fn fixed(value: f64, places: usize) -> String {
    let out = format!("{value:.places$}");
    if out.starts_with('-') && out[1..].chars().all(|c| c == '0' || c == '.') {
        return out[1..].to_string();
    }
    out
}

/// Round half away from zero, the way a temperature is read aloud.
pub fn round(value: f64) -> i64 {
    if value < 0.0 {
        (value - 0.5) as i64
    } else {
        (value + 0.5) as i64
    }
}

/// A compass point from a bearing. `None` for a direction the station did not
/// report, which is what it says when the wind is too light to have one.
pub fn compass(reading: Option<&Reading>) -> Option<&'static str> {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let degrees = reading?.value;
    if degrees < 0.0 {
        return None;
    }
    Some(POINTS[(((degrees + 11.25) / 22.5) as usize) % 16])
}

/// "Feels like 94°F", or nothing.
///
/// Heat index and wind chill are always present, and are equal to the air
/// temperature whenever neither applies. So this is only worth the line when
/// one of them has actually diverged from it.
pub fn feels_like(current: &Value) -> String {
    let Some(temp) = Reading::read(current, "outTemp") else {
        return String::new();
    };

    if let Some(heat) = Reading::read(current, "heatindex") {
        if heat.value >= temp.value + 1.0 {
            return format!("Feels like {}{}", round(heat.value), unit_label(&heat.unit));
        }
    }
    if let Some(chill) = Reading::read(current, "windchill") {
        if chill.value <= temp.value - 1.0 {
            return format!("Feels like {}{}", round(chill.value), unit_label(&chill.unit));
        }
    }
    String::new()
}

/// Speed, direction and gust as one phrase.
///
/// A still anemometer reports a direction of null rather than a heading, so the
/// direction is only added when there is one — "0 mph N" would be inventing a
/// bearing out of no wind.
pub fn wind(current: &Value) -> String {
    let Some(speed) = Reading::read(current, "windSpeed") else {
        return String::new();
    };
    if speed.value < 0.5 {
        return "Calm".to_string();
    }

    let unit = unit_label(&speed.unit);
    let mut out = format!("{}{}", fixed(speed.value, 0), unit);

    let direction = Reading::read(current, "windDir");
    if let Some(heading) = compass(direction.as_ref()) {
        out.push(' ');
        out.push_str(heading);
    }

    if let Some(gust) = Reading::read(current, "windGust") {
        if gust.value >= speed.value + 3.0 {
            out.push_str(&format!(", gusting {}{}", fixed(gust.value, 0), unit));
        }
    }
    out
}

/// Pressure with the direction it is moving, which is the part that forecasts
/// anything — the reading alone says very little without knowing the altitude.
pub fn pressure(current: &Value, forecast: &Value) -> String {
    let node = Reading::read(current, "barometer").or_else(|| Reading::read(forecast, "pressure"));
    let Some(node) = node else {
        return String::new();
    };

    let places = if node.unit == "inHg" { 2 } else { 0 };
    let mut out = node.text(places);

    let trend = forecast
        .get("pressure_trend")
        .and_then(|n| n.get("value"))
        .map(as_string)
        .unwrap_or_default();
    if !trend.is_empty() && trend != "steady" {
        out.push(' ');
        out.push_str(&trend);
    }
    out
}

/// "H 87°   L 72°".
///
/// Bare degrees rather than the full unit twice over: which scale this is has
/// already been said by the temperature above it, and "H 87°F   L 72°F" spends
/// most of a narrow column saying it twice more.
pub fn high_low(day: &Value) -> String {
    let (Some(high), Some(low)) = (
        Reading::read(day, "outTemp_max"),
        Reading::read(day, "outTemp_min"),
    ) else {
        return String::new();
    };

    let unit = unit_label(&high.unit);
    let unit = if unit.starts_with('°') { "°" } else { unit };
    format!(
        "H {}{unit}   L {}{unit}",
        round(high.value),
        round(low.value)
    )
}

/// Rain falling now takes precedence over rain that has already fallen: a rate
/// is what somebody looking at the wall wants to know, and the daily total is
/// the answer when it is dry.
pub fn rain(day: &Value, current: &Value) -> String {
    if let Some(rate) = Reading::read(current, "rainRate") {
        if rate.value > 0.0 {
            return format!("{} now", rate.text(2));
        }
    }
    match Reading::read(day, "rain_sum") {
        Some(total) => format!("{} today", total.text(2)),
        None => String::new(),
    }
}

// ---------------------------------------------------------------- periods

/// The forecast periods, shared by both sources.
///
/// weewx republishes the service's periods inside its own document; the
/// weather.gov source builds the same entries out of the service's reply
/// directly, so the glyph mapping and the formatting are decided once. An entry
/// may carry `night` outright, which the service states and the weewx document
/// does not — where it is missing the period's name is read instead.
pub fn periods(days: Option<&Value>) -> Vec<Period> {
    let Some(Value::Array(days)) = days else {
        return Vec::new();
    };

    days.iter()
        .map(|entry| {
            let unit = entry
                .get("unit")
                .map(as_string)
                .filter(|u| !u.is_empty())
                .map(|u| format!("°{u}"))
                .unwrap_or_default();

            let temperature = match (
                entry.get("high").and_then(as_f64),
                entry.get("low").and_then(as_f64),
            ) {
                (Some(high), _) => format!("High {}{unit}", round(high)),
                (None, Some(low)) => format!("Low {}{unit}", round(low)),
                (None, None) => String::new(),
            };

            let precip = entry
                .get("precipitation")
                .and_then(as_f64)
                .map(round)
                .filter(|chance| *chance > 0)
                .map(|chance| format!("{chance}% rain"))
                .unwrap_or_default();

            let name = entry.get("name").map(as_string).unwrap_or_default();
            let short = entry.get("short").map(as_string).unwrap_or_default();
            let condition = entry.get("condition").map(as_string).unwrap_or_default();

            let night = match entry.get("night").and_then(Value::as_bool) {
                Some(stated) => stated,
                None => is_night(&name),
            };

            Period {
                icon: icon_for(&condition, &short, night),
                detailed: entry.get("detailed").map(as_string).unwrap_or_default(),
                wind: entry.get("wind").map(as_string).unwrap_or_default(),
                name,
                short,
                condition,
                temperature,
                precip,
            }
        })
        .collect()
}

/// Watches and warnings out of whatever shape the document carries them in.
pub fn alerts(alerts: Option<&Value>) -> Vec<Alert> {
    let Some(Value::Array(alerts)) = alerts else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for raw in alerts {
        match raw {
            Value::String(text) => out.push(Alert {
                event: text.clone(),
                ..Alert::default()
            }),
            Value::Object(_) => {
                let event = pick(raw, &["event", "title", "headline"]);
                if event.is_empty() {
                    continue;
                }
                out.push(Alert {
                    event,
                    headline: pick(raw, &["headline", "description", "instruction"]),
                    severity: pick(raw, &["severity"]),
                    ends: pick(raw, &["ends", "expires", "end"]),
                });
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------- the clock

/// The time out of an ISO timestamp, in the hours the server wrote it in.
///
/// The hour is not converted. The station's clock is the one this reading
/// belongs to, and a machine set to another timezone would otherwise relabel
/// it.
pub fn clock_from(stamp: &str) -> Option<Clock> {
    let bytes = stamp.as_bytes();
    if bytes.len() < 16 || bytes[10] != b'T' {
        return None;
    }
    Some(Clock {
        hours: stamp.get(11..13)?.parse().ok()?,
        minutes: stamp.get(14..16)?.parse().ok()?,
    })
}

/// The same, for a timestamp in UTC — which is how the National Weather Service
/// stamps an observation, and the one case where the hour does have to be
/// moved.
///
/// The offset is dropped and a Z put in its place: the service writes "+00:00"
/// where it means Z, and some stations stamp with neither.
pub fn clock_from_utc(stamp: &str) -> Option<Clock> {
    use chrono::{TimeZone, Timelike};

    let bytes = stamp.as_bytes();
    if bytes.len() < 19 || bytes[10] != b'T' {
        return None;
    }
    let naive =
        chrono::NaiveDateTime::parse_from_str(stamp.get(..19)?, "%Y-%m-%dT%H:%M:%S").ok()?;
    let utc = chrono::Utc.from_utc_datetime(&naive);
    let local = utc.with_timezone(&chrono::Local);
    Some(Clock {
        hours: local.hour(),
        minutes: local.minute(),
    })
}

/// The time out of a stamp that carries its own offset, in local hours.
///
/// The warnings service writes "2026-08-16T17:30:00-10:00" — the time where the
/// weather is, with the offset spelled out. Converted rather than read off,
/// because a warning that expires at half past five in Hawaii should not say
/// half past five to somebody watching from Massachusetts.
pub fn clock_from_offset(stamp: &str) -> Option<Clock> {
    use chrono::Timelike;
    let parsed = chrono::DateTime::parse_from_rfc3339(stamp.trim()).ok()?;
    let local = parsed.with_timezone(&chrono::Local);
    Some(Clock {
        hours: local.hour(),
        minutes: local.minute(),
    })
}

/// 22:47 or 10:47 PM, whichever this household reads.
/// When an alert runs out, as something to read on a wall.
///
/// weewx hands this over already written for a person — "9:00 PM" — and
/// api.weather.gov hands over `2026-08-30T20:45:00-07:00`, which is correct,
/// unambiguous, and no use at all across a room. Until the banner scrolled, the
/// difference did not show: the timestamp sat past the ellipsis on every line
/// long enough to carry one. Now it is on screen, so it gets written out.
///
/// Anything that does not parse is passed through untouched rather than
/// dropped. The whole [`Alert`] parser is built to keep showing whatever it was
/// given when the shape is not what was expected, and the end of a warning is
/// not the field to start guessing on.
pub fn ends_text(raw: &str, twenty_four: bool) -> String {
    let raw = raw.trim();
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) else {
        return raw.to_string();
    };
    // Shown in the clock the wall is keeping, not the one the office that
    // issued it keeps. A warning for here that ends "10:45 PM MST" is a warning
    // somebody has to do arithmetic on before it means anything.
    let local = parsed.with_timezone(&chrono::Local);
    let clock = Clock {
        hours: chrono::Timelike::hour(&local),
        minutes: chrono::Timelike::minute(&local),
    };
    let time = clock_text(clock, twenty_four);

    // The day, but only when it is not today: almost every alert ends within
    // hours, and "until Sunday 11:00 PM" on a Sunday evening is a word that
    // earns nothing.
    if local.date_naive() == chrono::Local::now().date_naive() {
        time
    } else {
        const DAYS: [&str; 7] = [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ];
        let day = DAYS[chrono::Datelike::weekday(&local).num_days_from_monday() as usize];
        format!("{day} {time}")
    }
}

pub fn clock_text(clock: Clock, twenty_four: bool) -> String {
    if twenty_four {
        return format!("{:02}:{:02}", clock.hours, clock.minutes);
    }
    let (hour, suffix) = match clock.hours {
        0 => (12, "AM"),
        1..=11 => (clock.hours, "AM"),
        12 => (12, "PM"),
        _ => (clock.hours - 12, "PM"),
    };
    format!("{hour}:{:02} {suffix}", clock.minutes)
}

/// Whether to write the clock as 24-hour, when the user has not said.
///
/// The Roku channel asks the television, which knows. Nothing on a Linux
/// desktop reports it as plainly, so this reads the locale — which is the
/// setting that decides it for every other program on the machine. The list is
/// the English-speaking locales that write 12-hour clocks; everywhere else,
/// including the C locale, reads 24.
pub fn locale_prefers_24_hour() -> bool {
    let locale = ["LC_TIME", "LC_ALL", "LANG"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .unwrap_or_default();

    const TWELVE: [&str; 8] = [
        "en_US", "en_CA", "en_AU", "en_NZ", "en_PH", "en_IN", "en_PK", "en_MY",
    ];
    !TWELVE.iter().any(|prefix| locale.starts_with(prefix))
}

/// The day a strip carries: the weekday, the date, and the month by name.
///
/// Named rather than numbered because 9/8 and 8/9 are the same day to different
/// halves of the world and a wall does not have room to say which it means.
pub fn date_text(now: chrono::DateTime<chrono::Local>) -> String {
    use chrono::Datelike;
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ];
    const DAYS: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    format!(
        "{} {} {}",
        DAYS[now.weekday().num_days_from_monday() as usize],
        now.day(),
        MONTHS[now.month0() as usize]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_thunderstorm_outranks_the_cloud_it_arrives_in() {
        assert_eq!(
            icon_for("cloudy", "Chance Thunderstorms", false),
            Some(Icon::Storm)
        );
        // The order matters the other way too: "patchy fog then sunny" is fog.
        assert_eq!(icon_for("", "Patchy Fog then Sunny", false), Some(Icon::Fog));
    }

    /// "partly cloudy" contains "cloudy", so the partly tests have to come
    /// first — this is the case that catches a reordering.
    #[test]
    fn partly_cloudy_is_not_merely_cloudy() {
        assert_eq!(icon_for("partly", "Partly Cloudy", false), Some(Icon::PartlyDay));
        assert_eq!(icon_for("partly", "Partly Cloudy", true), Some(Icon::PartlyNight));
        assert_eq!(icon_for("cloudy", "Cloudy", false), Some(Icon::Cloudy));
    }

    #[test]
    fn nothing_recognisable_draws_nothing() {
        assert_eq!(icon_for("", "", false), None);
        assert_eq!(icon_for("", "   ", false), None);
        assert_eq!(icon_for("", "Volcanic Ash", false), None);
    }

    #[test]
    fn day_and_night_differ_only_where_the_sky_is_visible() {
        assert_eq!(icon_for("clear", "Clear", true), Some(Icon::ClearNight));
        assert_eq!(icon_for("clear", "Clear", false), Some(Icon::ClearDay));
        // Rain looks the same at midnight.
        assert_eq!(icon_for("rain", "Rain", true), Some(Icon::Rain));
    }

    /// A barometer trending down by a thousandth of an inch must not read as a
    /// negative pressure.
    #[test]
    fn a_value_that_rounds_to_zero_keeps_its_sign_to_itself() {
        assert_eq!(fixed(-0.001, 2), "0.00");
        assert_eq!(fixed(-0.6, 0), "-1");
        assert_eq!(fixed(29.9854, 2), "29.99");
        assert_eq!(fixed(0.0, 2), "0.00");
    }

    #[test]
    fn rounding_goes_away_from_zero() {
        assert_eq!(round(71.5), 72);
        assert_eq!(round(71.4), 71);
        assert_eq!(round(-4.5), -5);
        assert_eq!(round(-4.4), -4);
    }

    /// A still anemometer reports no direction; inventing one would be worse
    /// than leaving it out.
    #[test]
    fn calm_air_gets_no_bearing() {
        let current = json!({"windSpeed": {"value": 0.2, "unit": "mile_per_hour"}});
        assert_eq!(wind(&current), "Calm");

        let current = json!({
            "windSpeed": {"value": 4.0, "unit": "mile_per_hour"},
            "windDir": {"value": null, "unit": ""}
        });
        assert_eq!(wind(&current), "4 mph");
    }

    #[test]
    fn a_gust_is_only_mentioned_when_it_is_one() {
        let with_gust = json!({
            "windSpeed": {"value": 4.0, "unit": "mile_per_hour"},
            "windDir": {"value": 45.0, "unit": "degree_compass"},
            "windGust": {"value": 22.0, "unit": "mile_per_hour"}
        });
        assert_eq!(wind(&with_gust), "4 mph NE, gusting 22 mph");

        // Within 3mph of the average is not a gust, it is the wind.
        let steady = json!({
            "windSpeed": {"value": 4.0, "unit": "mile_per_hour"},
            "windGust": {"value": 5.0, "unit": "mile_per_hour"}
        });
        assert_eq!(wind(&steady), "4 mph");
    }

    #[test]
    fn compass_points_cover_the_circle() {
        let at = |degrees: f64| compass(Some(&Reading::new(degrees, "")));
        assert_eq!(at(0.0), Some("N"));
        assert_eq!(at(90.0), Some("E"));
        assert_eq!(at(180.0), Some("S"));
        assert_eq!(at(270.0), Some("W"));
        // The wrap at the top of the dial is the one that can panic.
        assert_eq!(at(359.9), Some("N"));
        assert_eq!(at(348.75), Some("N"));
        assert_eq!(at(348.74), Some("NNW"));
        assert_eq!(compass(None), None);
        assert_eq!(at(-1.0), None);
    }

    /// Only worth a line when one of them has actually diverged.
    #[test]
    fn feels_like_stays_quiet_when_it_feels_like_the_temperature() {
        let same = json!({
            "outTemp": {"value": 70.0, "unit": "degree_F"},
            "heatindex": {"value": 70.0, "unit": "degree_F"},
            "windchill": {"value": 70.0, "unit": "degree_F"}
        });
        assert_eq!(feels_like(&same), "");

        let hot = json!({
            "outTemp": {"value": 88.0, "unit": "degree_F"},
            "heatindex": {"value": 94.0, "unit": "degree_F"}
        });
        assert_eq!(feels_like(&hot), "Feels like 94°F");

        let cold = json!({
            "outTemp": {"value": 20.0, "unit": "degree_F"},
            "windchill": {"value": 8.0, "unit": "degree_F"}
        });
        assert_eq!(feels_like(&cold), "Feels like 8°F");
    }

    /// The scale is said once by the temperature above it.
    #[test]
    fn high_and_low_do_not_repeat_the_scale() {
        let day = json!({
            "outTemp_max": {"value": 87.0, "unit": "degree_F"},
            "outTemp_min": {"value": 72.0, "unit": "degree_F"}
        });
        assert_eq!(high_low(&day), "H 87°   L 72°");

        // Half a pair is not a pair.
        let partial = json!({"outTemp_max": {"value": 87.0, "unit": "degree_F"}});
        assert_eq!(high_low(&partial), "");
    }

    #[test]
    fn rain_now_outranks_rain_today() {
        let day = json!({"rain_sum": {"value": 0.42, "unit": "inch"}});
        let falling = json!({"rainRate": {"value": 0.15, "unit": "inch_per_hour"}});
        assert_eq!(rain(&day, &falling), "0.15 in/h now");

        let dry = json!({"rainRate": {"value": 0.0, "unit": "inch_per_hour"}});
        assert_eq!(rain(&day, &dry), "0.42 in today");

        assert_eq!(rain(&json!({}), &json!({})), "");
    }

    /// The map measures in kilometres and says so in whichever unit was asked
    /// for — and a span set in one unit has to survive being read back in the
    /// other, because that is all the setting does when somebody switches.
    #[test]
    fn how_far_a_view_reaches_is_said_in_the_units_asked_for() {
        assert_eq!(span_text(200.0, true), "200 km");
        assert_eq!(span_text(200.0, false), "124 mi");
        assert_eq!(span_text(1609.344, false), "1000 mi");

        // Whole miles through kilometres and back, across the range the dial
        // offers. A setting that drifted a mile every time the pane was opened
        // would be a setting that wandered.
        for miles in [20u32, 45, 124, 300, 750] {
            let km = (miles as f64 * KM_PER_MILE).round();
            assert_eq!(
                span_text(km, false),
                format!("{miles} mi"),
                "{miles} mi stored as {km} km"
            );
        }
    }

    /// A metric station must display metric with no setting to find.
    #[test]
    fn the_stations_own_units_are_the_ones_shown() {
        let metric = json!({"windSpeed": {"value": 12.0, "unit": "km_per_hour"}});
        assert_eq!(wind(&metric), "12 km/h");

        let day = json!({
            "outTemp_max": {"value": 30.0, "unit": "degree_C"},
            "outTemp_min": {"value": 18.0, "unit": "degree_C"}
        });
        assert_eq!(high_low(&day), "H 30°   L 18°");
    }

    #[test]
    fn an_unfamiliar_condition_is_shown_rather_than_dropped() {
        assert_eq!(condition_text("partlycloudy"), "Partly cloudy");
        assert_eq!(condition_text("dust devils"), "Dust devils");
        assert_eq!(condition_text(""), "");
    }

    #[test]
    fn a_weewx_timestamp_is_read_in_the_stations_own_hours() {
        let clock = clock_from("2026-08-16T22:47:03").expect("should parse");
        assert_eq!(clock, Clock { hours: 22, minutes: 47 });
        assert_eq!(clock_text(clock, true), "22:47");
        assert_eq!(clock_text(clock, false), "10:47 PM");

        assert_eq!(clock_from("not a timestamp"), None);
        assert_eq!(clock_from("2026-08-16 22:47:03"), None);
    }

    #[test]
    fn midnight_and_noon_are_the_two_that_go_wrong() {
        assert_eq!(clock_text(Clock { hours: 0, minutes: 5 }, false), "12:05 AM");
        assert_eq!(clock_text(Clock { hours: 12, minutes: 5 }, false), "12:05 PM");
        assert_eq!(clock_text(Clock { hours: 0, minutes: 5 }, true), "00:05");
        assert_eq!(clock_text(Clock { hours: 13, minutes: 5 }, false), "1:05 PM");
    }

    /// An unparseable stamp must come back as no reading, not as midnight.
    #[test]
    fn a_broken_utc_stamp_is_not_a_reading_taken_at_midnight() {
        assert_eq!(clock_from_utc("2026-13-45T99:99:99+00:00"), None);
        assert_eq!(clock_from_utc(""), None);
        assert!(clock_from_utc("2026-08-16T12:30:00+00:00").is_some());
    }

    #[test]
    fn alerts_survive_being_written_as_bare_strings() {
        let list = json!(["Tornado Warning", {"event": "Flood Watch", "severity": "Severe"}]);
        let parsed = alerts(Some(&list));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].event, "Tornado Warning");
        assert!(!parsed[0].is_severe());
        assert!(parsed[1].is_severe());
    }

    /// weewx writes this for a person already. Anything that is not a
    /// timestamp is its own best spelling and is left alone.
    #[test]
    fn an_end_time_that_is_already_readable_is_left_alone() {
        assert_eq!(ends_text("9:00 PM", false), "9:00 PM");
        assert_eq!(ends_text("later today", false), "later today");
        assert_eq!(ends_text("", false), "");
        assert_eq!(ends_text("  9:00 PM  ", false), "9:00 PM");
    }

    /// api.weather.gov writes it for a machine. Asserted by shape rather than
    /// by value: the answer is in the wall's timezone, and the machine running
    /// the tests is in whichever one it is in.
    #[test]
    fn an_iso_end_time_is_written_out_for_a_person() {
        let raw = "2026-08-30T20:45:00-07:00";

        let twelve = ends_text(raw, false);
        assert!(!twelve.contains('T'), "{twelve} still reads as a timestamp");
        assert!(
            twelve.contains("AM") || twelve.contains("PM"),
            "{twelve} is missing the half of the day"
        );

        let twenty_four = ends_text(raw, true);
        assert!(
            !twenty_four.contains("AM") && !twenty_four.contains("PM"),
            "{twenty_four} ignores the 24-hour clock"
        );
        assert!(twenty_four.contains(':'), "{twenty_four} is not a time");
    }

    /// A warning that runs past midnight says which day it ends on; one that
    /// ends today does not, because today is not worth a word.
    #[test]
    fn an_end_time_names_the_day_only_when_it_is_not_today() {
        const DAYS: [&str; 7] = [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ];

        let long_ago = ends_text("2020-06-15T12:00:00+00:00", false);
        assert!(
            DAYS.iter().any(|day| long_ago.starts_with(day)),
            "{long_ago} does not say which day"
        );

        let today = chrono::Local::now()
            .with_timezone(&chrono::Local)
            .to_rfc3339();
        let today = ends_text(&today, false);
        assert!(
            !DAYS.iter().any(|day| today.starts_with(day)),
            "{today} names today, which the reader already knows"
        );
    }

    /// The banner carries every alert, not the first and a count.
    #[test]
    fn every_alert_reaches_the_banner() {
        let model = Model {
            alerts: vec![
                Alert {
                    event: "Tornado Watch".into(),
                    ends: "9:00 PM".into(),
                    ..Alert::default()
                },
                Alert {
                    event: "Flood Warning".into(),
                    headline: "Flooding in low-lying areas".into(),
                    severity: "Extreme".into(),
                    ..Alert::default()
                },
            ],
            ..Model::default()
        };
        let line = model.alerts_line(false);
        assert!(line.contains("Tornado Watch"));
        assert!(line.contains("until 9:00 PM"));
        assert!(line.contains("Flooding in low-lying areas"));
        assert!(model.alerts_severe(), "the strongest alert colours the banner");
    }

    /// The headline usually repeats the event; saying it twice wastes the width.
    #[test]
    fn a_headline_that_repeats_the_event_is_not_repeated() {
        let alert = Alert {
            event: "Flood Watch".into(),
            headline: "Flood Watch".into(),
            ..Alert::default()
        };
        assert_eq!(alert.line(), "Flood Watch");
    }

    /// The usual shape: the name, then everything the name did not say.
    #[test]
    fn a_headline_opening_with_the_event_keeps_only_the_rest() {
        let alert = Alert {
            event: "Heat Advisory".into(),
            headline: "Heat Advisory issued September 2 at 1:00 PM CDT by NWS Fort Worth TX"
                .into(),
            ..Alert::default()
        };
        assert_eq!(
            alert.detail(),
            "Issued September 2 at 1:00 PM CDT by NWS Fort Worth TX"
        );
        assert_eq!(
            alert.line(),
            "Heat Advisory  ·  Issued September 2 at 1:00 PM CDT by NWS Fort Worth TX"
        );
    }

    /// A headline saying something else keeps every word of it.
    #[test]
    fn a_headline_that_says_something_else_is_left_whole() {
        assert_eq!(
            without_event("Flooding in low-lying areas", "Flood Warning"),
            "Flooding in low-lying areas"
        );
        // The same letters, a different word: the name is not the opening.
        assert_eq!(
            without_event("Flooding in low-lying areas", "Flood"),
            "Flooding in low-lying areas"
        );
    }

    /// The service's own punctuation goes with the name it separated.
    #[test]
    fn the_separator_after_the_event_goes_too() {
        assert_eq!(
            without_event("Wind Advisory - until 9 PM", "wind advisory"),
            "Until 9 PM"
        );
    }

    #[test]
    fn a_period_carries_its_own_idea_of_night() {
        let days = json!([
            {"name": "Tonight", "short": "Clear", "condition": "clear", "low": 48, "unit": "F"},
            {"name": "Thursday", "short": "Sunny", "condition": "clear", "high": 71,
             "unit": "F", "night": false, "precipitation": 20}
        ]);
        let parsed = periods(Some(&days));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].icon, Some(Icon::ClearNight));
        assert_eq!(parsed[0].temperature, "Low 48°F");
        assert_eq!(parsed[1].icon, Some(Icon::ClearDay));
        assert_eq!(parsed[1].temperature, "High 71°F");
        assert_eq!(parsed[1].precip, "20% rain");
        // A zero chance of rain is not worth the line.
        assert_eq!(parsed[0].precip, "");
    }

    #[test]
    fn a_station_with_no_sensors_shows_no_rows() {
        assert!(Model::empty().stats().is_empty());
        assert!(Model::empty().detailed_stats().is_empty());
    }
}
