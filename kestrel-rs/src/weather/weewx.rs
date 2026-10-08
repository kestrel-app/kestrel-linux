//! A weewx server on your own network.
//!
//! One HTTP GET against the JSON document weewx publishes for Home Assistant —
//! the same file read by a television in the Roku channel instead. There is no
//! session, no credentials and no vendor dispatch: unlike the camera systems,
//! every weewx install serves this the same way, so there is nothing here to
//! abstract over.

use serde_json::Value;

use super::{
    alerts, clock_from, condition_text, feels_like, high_low, icon_for, is_night, periods, pressure,
    rain, wind, Model, Reading,
};
use crate::api::http;

/// Blocking; call from the poller thread.
///
/// `allow_self_signed` covers a weewx server behind a private CA or a
/// self-signed certificate, which is the normal case for one on a LAN — the
/// same allowance the NVRs get, and for the same reason.
pub fn fetch(url: &str, allow_self_signed: bool, timeout_seconds: u64) -> Model {
    let url = url.trim();
    if url.is_empty() {
        return Model::failure("no weather server address is set");
    }

    let agent = http::agent(timeout_seconds, allow_self_signed);
    let response = match http::request(&agent, "GET", url, None, &[]) {
        Ok(response) => response,
        Err(err) => return Model::failure(err.to_string()),
    };
    if !response.ok() {
        return Model::failure(format!("the server answered {}", response.status));
    }

    let Ok(doc) = serde_json::from_str::<Value>(&response.body) else {
        return Model::failure("that address did not return JSON");
    };
    if doc.get("current").is_none() && doc.get("station").is_none() {
        return Model::failure("that address returned JSON, but not weewx data");
    }

    parse(&doc)
}

pub fn parse(doc: &Value) -> Model {
    const NOTHING: Value = Value::Null;
    let current = doc.get("current").unwrap_or(&NOTHING);
    let day = doc.get("day").unwrap_or(&NOTHING);
    let forecast = doc.get("forecast").unwrap_or(&NOTHING);

    let mut model = Model {
        ok: true,
        ..Model::empty()
    };

    if let Some(station) = doc.get("station") {
        model.station = super::pick(station, &["name"]);
    }

    // Taken from the server's own timestamp rather than converted from the
    // epoch: the station's clock is the one this reading belongs to, and a
    // machine set to a different timezone would otherwise relabel it.
    model.observed = doc
        .get("generated_at")
        .map(super::as_string)
        .and_then(|stamp| clock_from(&stamp));

    if let Some(temp) = Reading::read(current, "outTemp") {
        model.temp_big = format!("{}{}", super::round(temp.value), super::unit_label(&temp.unit));
    }

    model.humidity = Reading::read(current, "outHumidity")
        .map(|r| r.text(0))
        .unwrap_or_default();
    model.dew_text = Reading::read(current, "dewpoint")
        .map(|r| r.text(0))
        .unwrap_or_default();
    model.feels = feels_like(current);
    model.wind_text = wind(current);
    model.pressure_text = pressure(current, forecast);
    model.high_low = high_low(day);
    model.rain_text = rain(day, current);

    if let Some(uv) = Reading::read(current, "UV") {
        model.uv_text = format!("UV {}", super::round(uv.value));
    }

    if let Some(zambretti) = forecast.get("zambretti").and_then(|n| n.get("value")) {
        model.summary = super::as_string(zambretti);
    }
    if let Some(condition) = forecast.get("condition").and_then(|n| n.get("value")) {
        model.condition = super::as_string(condition);
        model.condition_text = condition_text(&model.condition);
    }
    if model.summary.is_empty() {
        model.summary = model.condition_text.clone();
    }

    if let Some(nws) = doc.get("nws") {
        model.periods = periods(nws.get("days"));
        model.alerts = alerts(nws.get("alerts"));
    }

    if let Some(first) = model.periods.first() {
        model.outlook_name = first.name.clone();
        model.outlook_text = first.short.clone();
    }

    // Whether it is dark out, taken from the name of the period the service
    // considers current — "Tonight" and "Tuesday Night" against "Today" and
    // "This Afternoon". The document carries no sunset, and this machine's own
    // clock is not the station's, so the forecast's own idea of now is the
    // better of the two guesses available.
    //
    // The condition rather than the summary: `summary` is the Zambretti
    // forecast, which says what is *coming* — "Rain at times, worse later"
    // under a clear sky would otherwise put a rain cloud on a clear evening.
    let night = model.periods.first().is_some_and(|p| is_night(&p.name));
    model.icon = icon_for(&model.condition, &model.condition_text, night);

    model
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "station": {"name": "Agawam"},
            "generated_at": "2026-08-16T22:47:03",
            "current": {
                "outTemp": {"value": 71.4, "unit": "degree_F"},
                "outHumidity": {"value": 64.0, "unit": "percent"},
                "dewpoint": {"value": 58.2, "unit": "degree_F"},
                "heatindex": {"value": 71.4, "unit": "degree_F"},
                "windchill": {"value": 71.4, "unit": "degree_F"},
                "windSpeed": {"value": 4.0, "unit": "mile_per_hour"},
                "windDir": {"value": 45.0, "unit": "degree_compass"},
                "barometer": {"value": 29.9854, "unit": "inHg"},
                "rainRate": {"value": 0.0, "unit": "inch_per_hour"},
                "UV": {"value": 2.4, "unit": "uv_index"}
            },
            "day": {
                "outTemp_max": {"value": 78.0, "unit": "degree_F"},
                "outTemp_min": {"value": 61.0, "unit": "degree_F"},
                "rain_sum": {"value": 0.12, "unit": "inch"}
            },
            "forecast": {
                "zambretti": {"value": "Fine weather"},
                "condition": {"value": "partlycloudy"},
                "pressure_trend": {"value": "rising"}
            },
            "nws": {
                "days": [{"name": "Tonight", "short": "Partly Cloudy", "low": 61, "unit": "F"}],
                "alerts": []
            }
        })
    }

    #[test]
    fn a_whole_document_becomes_the_shared_model() {
        let model = parse(&document());
        assert!(model.ok);
        assert_eq!(model.station, "Agawam");
        assert_eq!(model.temp_big, "71°F");
        assert_eq!(model.humidity, "64%");
        assert_eq!(model.dew_text, "58°F");
        assert_eq!(model.wind_text, "4 mph NE");
        assert_eq!(model.pressure_text, "29.99 inHg rising");
        assert_eq!(model.high_low, "H 78°   L 61°");
        assert_eq!(model.rain_text, "0.12 in today");
        assert_eq!(model.uv_text, "UV 2");
        assert_eq!(
            model.observed,
            Some(super::super::Clock { hours: 22, minutes: 47 })
        );
        assert_eq!(model.outlook_name, "Tonight");
    }

    /// The Zambretti forecast says what is coming, so the *condition* is what
    /// picks the glyph — otherwise "Rain at times, worse later" under a clear
    /// sky puts a rain cloud on a clear evening.
    #[test]
    fn the_glyph_follows_the_sky_and_not_the_forecast() {
        let mut doc = document();
        doc["forecast"]["zambretti"]["value"] = json!("Rain at times, worse later");
        doc["forecast"]["condition"]["value"] = json!("clear");
        doc["nws"]["days"][0]["name"] = json!("Tonight");

        let model = parse(&doc);
        assert_eq!(model.summary, "Rain at times, worse later");
        assert_eq!(model.icon, Some(super::super::Icon::ClearNight));
    }

    /// Not every install has every sensor, and a missing one is a blank line
    /// rather than a zero.
    #[test]
    fn a_station_without_a_sensor_simply_omits_it() {
        let doc = json!({
            "station": {"name": "Shed"},
            "current": {"outTemp": {"value": 55.0, "unit": "degree_F"}}
        });
        let model = parse(&doc);
        assert!(model.ok);
        assert_eq!(model.temp_big, "55°F");
        assert_eq!(model.humidity, "");
        assert_eq!(model.wind_text, "");
        assert_eq!(model.uv_text, "");
        assert!(model.periods.is_empty());
    }

    /// The Zambretti line is the summary when there is one; the condition
    /// stands in when there is not.
    #[test]
    fn the_condition_stands_in_for_a_missing_summary() {
        let mut doc = document();
        doc["forecast"].as_object_mut().unwrap().remove("zambretti");
        assert_eq!(parse(&doc).summary, "Partly cloudy");
    }

    #[test]
    fn an_empty_address_is_refused_before_any_request() {
        let model = fetch("   ", false, 5);
        assert!(!model.ok);
        assert_eq!(model.error, "no weather server address is set");
    }
}
