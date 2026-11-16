//! Weather from the National Weather Service, for somebody with no station.
//!
//! The weewx source reads one document off a machine on your own network. This
//! one reads api.weather.gov, which is the other half of the same picture: the
//! forecast weewx republishes comes from here in the first place, and what is
//! missing without a station is the *observation* — so this fetches that too,
//! from whichever airport or mesonet site the service has nearest the address.
//!
//! The whole point is that it takes a ZIP code and nothing else. The service is
//! addressed by coordinate, so the ZIP is turned into one at setup time out of
//! the table in [`super::zip`], and what is stored is the coordinate.
//!
//! What comes back is the model in [`super`], built the same way and read by
//! the same widgets. That is done by shaping the readings into the
//! `{ value, unit }` nodes weewx publishes and handing them to the formatters
//! already written for them — so the strip, the pane and the tiles do not know
//! or care which source filled them in, and there is one set of rules about
//! when a wind is calm or a "feels like" is worth the line.
//!
//! Three requests per poll: the forecast, the latest observation, and the
//! active alerts. Stations go quiet, so the observation walks the nearest few
//! until one has something in it, which is the only case that costs more. The
//! two lookups that turn a coordinate into those addresses are done once and
//! held by the poller, since a forecast office's grid does not move.

use serde_json::{json, Value};

use super::{
    alerts as parse_alerts, as_f64, as_string, clock_from_utc, feels_like, high_low, icon_for,
    periods as parse_periods, pressure, rain, round, unit_label, wind, Model, Reading,
};
use crate::api::http;

pub const ROOT: &str = "https://api.weather.gov";

/// The service asks that clients identify themselves, and answers 403 to some
/// generic agents.
///
/// They also ask for a contact address in here, so that a client behaving badly
/// can be told rather than blocked. There is nowhere to point yet; if this
/// project publishes a home, add it to this string.
pub fn headers() -> Vec<(String, String)> {
    vec![
        (
            "User-Agent".into(),
            format!("Kestrel/{}", env!("CARGO_PKG_VERSION")),
        ),
        ("Accept".into(), "application/geo+json".into()),
    ]
}

/// The same introduction, for the requests that want a picture back.
///
/// The radar used [`headers`], which asks for application/geo+json — a map
/// server being asked for JSON and then told off for not returning a PNG. It is
/// the forecast API's Accept header and it has no business on an image request:
/// every one of these services negotiates on it, and being wrong here is only
/// harmless for as long as they all choose to ignore it.
pub fn image_headers() -> Vec<(String, String)> {
    vec![
        (
            "User-Agent".into(),
            format!("Kestrel/{}", env!("CARGO_PKG_VERSION")),
        ),
        ("Accept".into(), "image/png,image/*".into()),
    ]
}

/// One GET, parsed.
///
/// An object, specifically. Everything below reads named fields off this, and a
/// captive portal answering with a page — or any other JSON that is not what
/// was asked for — has to be turned away here and not further down.
pub fn get(url: &str, timeout_seconds: u64) -> Result<Value, String> {
    // api.weather.gov has an ordinary public certificate, so unlike the LAN
    // servers this app talks to there is nothing to excuse here.
    let agent = http::agent(timeout_seconds, false);
    let response =
        http::request(&agent, "GET", url, None, &headers()).map_err(|err| err.to_string())?;
    if !response.ok() {
        return Err(format!("weather.gov answered {}", response.status));
    }
    match serde_json::from_str::<Value>(&response.body) {
        Ok(doc @ Value::Object(_)) => Ok(doc),
        _ => Err("weather.gov did not return JSON".into()),
    }
}

// ---------------------------------------------------------------- the site
//
// A coordinate is not what the API is keyed on. /points maps one onto a
// forecast office and a grid square, and that mapping is what the forecast and
// the list of nearby observation stations hang off. None of it changes, so it
// is resolved once and kept by the caller for as long as it is polling.

#[derive(Clone, Debug, Default)]
pub struct Site {
    pub ok: bool,
    pub error: String,
    pub forecast_url: String,
    pub stations: Vec<String>,
    pub place: String,
}

pub fn resolve(lat: &str, lon: &str, timeout_seconds: u64) -> Site {
    let mut site = Site::default();

    if lat.is_empty() || lon.is_empty() {
        site.error = "no ZIP code is set".into();
        return site;
    }

    let doc = match get(&format!("{ROOT}/points/{lat},{lon}"), timeout_seconds) {
        Ok(doc) => doc,
        Err(err) => {
            site.error = err;
            return site;
        }
    };

    let Some(properties) = doc.get("properties") else {
        site.error = "weather.gov has no forecast for that location".into();
        return site;
    };
    let Some(forecast) = properties.get("forecast").map(as_string) else {
        // What /points answers for a coordinate outside the country. The
        // service covers the United States and its territories and nowhere
        // else, which is worth saying plainly rather than as a 404.
        site.error = "weather.gov has no forecast for that location".into();
        return site;
    };

    site.forecast_url = forecast;
    site.place = place(properties);

    // The stations nearest the grid square, nearest first. More than one is
    // kept because the first is regularly an airport that stops reporting
    // overnight, and a strip with no temperature on it is the visible result.
    if let Some(url) = properties.get("observationStations").map(as_string) {
        if let Ok(listing) = get(&url, timeout_seconds) {
            if let Some(Value::Array(stations)) = listing.get("observationStations") {
                site.stations = stations.iter().take(4).map(as_string).collect();
            }
        }
    }

    site.ok = true;
    site
}

/// "Agawam Town, MA" — the service's own name for where the coordinate landed,
/// which is the honest label for a reading taken some miles from it.
pub fn place(properties: &Value) -> String {
    let Some(relative) = properties
        .get("relativeLocation")
        .and_then(|r| r.get("properties"))
    else {
        return String::new();
    };

    let city = relative.get("city").map(as_string).unwrap_or_default();
    let state = relative.get("state").map(as_string).unwrap_or_default();
    match (city.is_empty(), state.is_empty()) {
        (true, _) => state,
        (_, true) => city,
        _ => format!("{city}, {state}"),
    }
}

// ---------------------------------------------------------------- fetch

/// Blocking; call from the poller thread. `site` comes from [`resolve`].
pub fn fetch(site: &Site, lat: &str, lon: &str, metric: bool, timeout_seconds: u64) -> Model {
    if !site.ok {
        return Model::failure("weather.gov has not been reached yet");
    }

    // The forecast first, and fatally: it carries the periods the tiles show,
    // today's high and low, and — when no station nearby is reporting — the
    // temperature itself. Without it there is nothing worth putting on a wall.
    let mut url = site.forecast_url.clone();
    if metric {
        url.push_str("?units=si");
    }

    let doc = match get(&url, timeout_seconds) {
        Ok(doc) => doc,
        Err(err) => return Model::failure(err),
    };

    let periods = doc.get("properties").and_then(|p| p.get("periods"));
    let Some(Value::Array(periods)) = periods else {
        return Model::failure("weather.gov returned no forecast for that location");
    };
    if periods.is_empty() {
        return Model::failure("weather.gov returned no forecast for that location");
    }

    let mut model = Model {
        ok: true,
        station: site.place.clone(),
        periods: parse_periods(Some(&reshape_periods(periods))),
        ..Model::empty()
    };

    if let Some(first) = model.periods.first() {
        model.outlook_name = first.name.clone();
        model.outlook_text = first.short.clone();
    }

    // Not fatal, either of them. A grid square with no reporting station near
    // it still has a forecast worth showing, and an alerts request that fails
    // should not take the temperature off the wall with it.
    let observation = observation(site, timeout_seconds);
    model.alerts = parse_alerts(Some(&alerts(lat, lon, timeout_seconds)));

    apply_current(&mut model, observation.as_ref(), periods, metric);
    model
}

/// The latest reading from the nearest station that has one.
///
/// Walked rather than taken from the first entry: stations go quiet, and the
/// service answers for one that has not reported since yesterday with a record
/// full of nulls rather than an error. A temperature is the test of whether
/// there is anything in it worth having.
fn observation(site: &Site, timeout_seconds: u64) -> Option<Value> {
    for station in &site.stations {
        let Ok(doc) = get(&format!("{station}/observations/latest"), timeout_seconds) else {
            continue;
        };
        let Some(properties) = doc.get("properties") else {
            continue;
        };
        let has_temperature = properties
            .get("temperature")
            .and_then(|t| t.get("value"))
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if has_temperature {
            return Some(properties.clone());
        }
    }
    None
}

fn alerts(lat: &str, lon: &str, timeout_seconds: u64) -> Value {
    let Ok(doc) = get(&format!("{ROOT}/alerts/active?point={lat},{lon}"), timeout_seconds) else {
        return Value::Array(Vec::new());
    };
    let Some(Value::Array(features)) = doc.get("features") else {
        return Value::Array(Vec::new());
    };

    // The shared parser reads event, headline, severity and ends off each
    // entry, which are the names the service uses — so what it wants is the
    // properties of each GeoJSON feature rather than the feature.
    Value::Array(
        features
            .iter()
            .filter_map(|feature| feature.get("properties").cloned())
            .collect(),
    )
}

// ---------------------------------------------------------------- current

/// Fills in everything the strip's left and middle zones show, from the station
/// reading where there is one and the forecast where there is not.
fn apply_current(model: &mut Model, observation: Option<&Value>, periods: &[Value], metric: bool) {
    model.high_low = high_low(&day(periods, metric));

    // Whether it is dark out, from the service's own answer for the period it
    // considers current. It says so outright, which is better than reading it
    // out of the period's name the way the weewx document has to be read.
    let mut night = !is_daytime(periods, 0);

    let Some(observation) = observation else {
        // No station reporting nearby. The forecast still knows what it is
        // going to be, which is a good deal better than an empty strip — so the
        // period's own temperature and wording stand in, and the readings the
        // middle zone shows are simply not there.
        if let Some(first) = periods.first() {
            let unit = format!(
                "°{}",
                first.get("temperatureUnit").map(as_string).unwrap_or_default()
            );
            if let Some(temperature) = first.get("temperature").and_then(as_f64) {
                model.temp_big = format!("{}{unit}", round(temperature));
            }
            model.condition_text = first.get("shortForecast").map(as_string).unwrap_or_default();
            model.summary = model.condition_text.clone();
            model.wind_text = first.get("windSpeed").map(as_string).unwrap_or_default();
        }
        model.icon = icon_for("", &model.condition_text, night);
        return;
    };

    // The service reports observations in SI whatever the forecast is asked
    // for, so these are converted here rather than anywhere further down. What
    // comes out is the `{ value, unit }` shape weewx publishes, which means
    // every formatter written for that document works on this one unchanged.
    let current = json!({
        "outTemp":     node(temperature(observation.get("temperature"), metric)),
        "dewpoint":    node(temperature(observation.get("dewpoint"), metric)),
        "heatindex":   node(temperature(observation.get("heatIndex"), metric)),
        "windchill":   node(temperature(observation.get("windChill"), metric)),
        "outHumidity": node(percent(observation.get("relativeHumidity"))),
        "windSpeed":   node(speed(observation.get("windSpeed"), metric)),
        "windGust":    node(speed(observation.get("windGust"), metric)),
        "windDir":     node(angle(observation.get("windDirection"))),
        "barometer":   node(barometer(observation.get("barometricPressure"), metric)),

        // What fell in the last hour, read as the hour's average rate. The
        // service publishes no daily total and no instantaneous rate, and an
        // hour's accumulation is the nearer of the two to either.
        "rainRate":    node(depth_rate(observation.get("precipitationLastHour"), metric)),
    });

    if let Some(temp) = Reading::read(&current, "outTemp") {
        model.temp_big = format!("{}{}", round(temp.value), unit_label(&temp.unit));
    }
    model.humidity = Reading::read(&current, "outHumidity")
        .map(|r| r.text(0))
        .unwrap_or_default();
    model.dew_text = Reading::read(&current, "dewpoint")
        .map(|r| r.text(0))
        .unwrap_or_default();
    model.feels = feels_like(&current);
    model.wind_text = wind(&current);
    model.pressure_text = pressure(&current, &Value::Null);
    model.rain_text = rain(&Value::Null, &current);

    // The station's own words — "Mostly Cloudy", "Light Rain", "Fog/Mist". The
    // weewx source has a Zambretti forecast to show here instead; this one has
    // the sky as somebody's instrument last saw it, which is the more useful of
    // the two under a temperature.
    //
    // Not every station has any. The airports write one; the mesonet sites that
    // cover the country between them are thermometers and anemometers and
    // report the field empty. So the forecast's word for the period stands in,
    // which is what leaves a rural ZIP code with a temperature that has
    // something written under it and a glyph beside it rather than neither.
    model.condition_text = observation
        .get("textDescription")
        .map(as_string)
        .unwrap_or_default();
    if model.condition_text.is_empty() {
        if let Some(first) = periods.first() {
            model.condition_text = first.get("shortForecast").map(as_string).unwrap_or_default();
        }
    }
    model.summary = model.condition_text.clone();

    // The reading is stamped in UTC, unlike a weewx server's, which stamps in
    // its own local time. So this one is converted.
    model.observed = observation
        .get("timestamp")
        .map(as_string)
        .and_then(|stamp| clock_from_utc(&stamp));

    // The service labels its own icon day or night, which settles it for the
    // station rather than for the forecast period — the two disagree for the
    // hour either side of sunset, and it is the sky *now* that is being drawn.
    if let Some(icon) = observation.get("icon").map(as_string) {
        if !icon.is_empty() {
            night = icon.to_ascii_lowercase().contains("/night/");
        }
    }

    model.icon = icon_for("", &model.condition_text, night);
}

/// Today's high and tonight's low, in the shape [`high_low`] reads.
///
/// The service does not publish either as a number: each period carries one
/// temperature and whether it is a daytime period, so the pair is the first of
/// each. Late in the evening that makes the "high" tomorrow's, which is the
/// same answer the forecast itself gives at that hour.
fn day(periods: &[Value], metric: bool) -> Value {
    let unit = if metric { "degree_C" } else { "degree_F" };
    let mut high: Option<Value> = None;
    let mut low: Option<Value> = None;

    // Only the first few periods are looked at. Further out than that the pair
    // stops being "today" by any reading of the word — a station that has just
    // gone quiet should not put Thursday's high under a Monday temperature.
    for period in periods.iter().take(4) {
        let Some(temperature) = period.get("temperature").and_then(as_f64) else {
            continue;
        };
        let node = json!({"value": temperature, "unit": unit});
        if period.get("isDaytime") == Some(&Value::Bool(true)) {
            high.get_or_insert(node);
        } else {
            low.get_or_insert(node);
        }
    }

    json!({"outTemp_max": high, "outTemp_min": low})
}

fn is_daytime(periods: &[Value], index: usize) -> bool {
    match periods.get(index) {
        Some(period) => period.get("isDaytime") == Some(&Value::Bool(true)),
        None => true,
    }
}

// ---------------------------------------------------------------- periods

/// The service's periods in the shape the shared parser reads — so the tiles,
/// the weather pane and the glyph mapping are the ones already written.
///
/// The service gives one temperature per period and says which end of the day
/// it is; weewx gives a high or a low. `night` is carried across outright
/// rather than left to be read out of the period's name, since the service
/// knows.
fn reshape_periods(periods: &[Value]) -> Value {
    Value::Array(
        periods
            .iter()
            .map(|period| {
                let daytime = period.get("isDaytime") == Some(&Value::Bool(true));
                let temperature = period.get("temperature").and_then(as_f64);

                let mut entry = json!({
                    "name": period.get("name").map(as_string).unwrap_or_default(),
                    "short": period.get("shortForecast").map(as_string).unwrap_or_default(),
                    "detailed": period.get("detailedForecast").map(as_string).unwrap_or_default(),
                    "unit": period.get("temperatureUnit").map(as_string).unwrap_or_default(),
                    "wind": wind_phrase(period),
                    "condition": "",
                    "night": !daytime,
                });

                if let Some(temperature) = temperature {
                    entry[if daytime { "high" } else { "low" }] = json!(temperature);
                }
                if let Some(chance) = period
                    .get("probabilityOfPrecipitation")
                    .and_then(|p| p.get("value"))
                    .and_then(as_f64)
                {
                    entry["precipitation"] = json!(chance);
                }
                entry
            })
            .collect::<Vec<_>>(),
    )
}

/// "5 to 10 mph SW". The service writes the speed as a phrase with its unit
/// already in it and the direction as a separate compass point.
fn wind_phrase(period: &Value) -> String {
    let speed = period.get("windSpeed").map(as_string).unwrap_or_default();
    if speed.is_empty() {
        return String::new();
    }
    match period.get("windDirection").map(as_string) {
        Some(direction) if !direction.is_empty() => format!("{speed} {direction}"),
        _ => speed,
    }
}

// ---------------------------------------------------------------- units
//
// Observations come back in SI regardless of what the forecast was asked for,
// so US customary is a conversion rather than a request. Each of these returns
// the `{ value, unit }` node weewx would have published, or `None` where the
// station does not report that reading — which is what the shared formatters
// already expect of a sensor that is not there.

/// A node, or JSON null — which the shared reader treats as an absent sensor.
fn node(reading: Option<Reading>) -> Value {
    match reading {
        Some(reading) => json!({"value": reading.value, "unit": reading.unit}),
        None => Value::Null,
    }
}

/// The value out of a service reading, or `None` when it did not report one.
fn value_of(node: Option<&Value>) -> Option<f64> {
    node?.get("value").and_then(as_f64)
}

fn temperature(node: Option<&Value>, metric: bool) -> Option<Reading> {
    let celsius = value_of(node)?;
    Some(if metric {
        Reading::new(celsius, "degree_C")
    } else {
        Reading::new(celsius * 1.8 + 32.0, "degree_F")
    })
}

fn speed(node: Option<&Value>, metric: bool) -> Option<Reading> {
    let kph = value_of(node)?;
    Some(if metric {
        Reading::new(kph, "km_per_hour")
    } else {
        Reading::new(kph * 0.621_371, "mile_per_hour")
    })
}

fn barometer(node: Option<&Value>, metric: bool) -> Option<Reading> {
    let pascals = value_of(node)?;
    Some(if metric {
        Reading::new(pascals / 100.0, "mbar")
    } else {
        Reading::new(pascals / 3386.389, "inHg")
    })
}

fn depth_rate(node: Option<&Value>, metric: bool) -> Option<Reading> {
    let mm = value_of(node)?;
    Some(if metric {
        Reading::new(mm, "mm_per_hour")
    } else {
        Reading::new(mm / 25.4, "inch_per_hour")
    })
}

fn percent(node: Option<&Value>) -> Option<Reading> {
    Some(Reading::new(value_of(node)?, "percent"))
}

fn angle(node: Option<&Value>) -> Option<Reading> {
    Some(Reading::new(value_of(node)?, ""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weather::Icon;

    fn forecast_periods() -> Vec<Value> {
        vec![
            json!({
                "name": "Tonight", "isDaytime": false, "temperature": 15,
                "temperatureUnit": "C", "shortForecast": "Mostly Clear",
                "detailedForecast": "Mostly clear, with a low around 15.",
                "windSpeed": "5 to 10 km/h", "windDirection": "SW",
                "probabilityOfPrecipitation": {"value": null}
            }),
            json!({
                "name": "Thursday", "isDaytime": true, "temperature": 26,
                "temperatureUnit": "C", "shortForecast": "Chance Showers And Thunderstorms",
                "detailedForecast": "A chance of showers.",
                "windSpeed": "10 km/h", "windDirection": "W",
                "probabilityOfPrecipitation": {"value": 40}
            }),
        ]
    }

    fn observation() -> Value {
        json!({
            "temperature": {"value": 21.7, "unitCode": "wmoUnit:degC"},
            "dewpoint": {"value": 14.4},
            "relativeHumidity": {"value": 63.5},
            "windSpeed": {"value": 6.4},
            "windDirection": {"value": 45.0},
            "windGust": {"value": null},
            "barometricPressure": {"value": 101_540.0},
            "precipitationLastHour": {"value": null},
            "heatIndex": {"value": null},
            "windChill": {"value": null},
            "textDescription": "Mostly Cloudy",
            "timestamp": "2026-08-16T18:53:00+00:00",
            "icon": "https://api.weather.gov/icons/land/day/bkn?size=medium"
        })
    }

    /// SI in, US customary out — and through the same formatters the weewx
    /// document uses, so the wording is identical.
    #[test]
    fn a_service_observation_becomes_the_same_model() {
        let mut model = Model {
            ok: true,
            ..Model::empty()
        };
        apply_current(&mut model, Some(&observation()), &forecast_periods(), false);

        assert_eq!(model.temp_big, "71°F");
        assert_eq!(model.humidity, "64%");
        assert_eq!(model.dew_text, "58°F");
        assert_eq!(model.wind_text, "4 mph NE");
        // 101,540 Pa is 29.98 inHg — the conversion, not a rounded guess.
        assert_eq!(model.pressure_text, "29.98 inHg");
        assert_eq!(model.condition_text, "Mostly Cloudy");
        // The service's own icon settles day or night for the station.
        assert_eq!(model.icon, Some(Icon::Cloudy));
    }

    #[test]
    fn asking_for_metric_leaves_the_readings_in_si() {
        let mut model = Model {
            ok: true,
            ..Model::empty()
        };
        apply_current(&mut model, Some(&observation()), &forecast_periods(), true);
        assert_eq!(model.temp_big, "22°C");
        assert_eq!(model.wind_text, "6 km/h NE");
        assert_eq!(model.pressure_text, "1015 mbar");
    }

    /// A grid square with no reporting station still has a forecast worth
    /// showing — the period stands in rather than leaving an empty strip.
    #[test]
    fn a_quiet_station_falls_back_to_the_forecast() {
        let mut model = Model {
            ok: true,
            ..Model::empty()
        };
        apply_current(&mut model, None, &forecast_periods(), true);

        assert_eq!(model.temp_big, "15°C");
        assert_eq!(model.condition_text, "Mostly Clear");
        assert_eq!(model.wind_text, "5 to 10 km/h");
        // Tonight is not a daytime period, so the sky is drawn dark — and
        // "Mostly Clear" is a partly-cloudy sky rather than a clear one.
        assert_eq!(model.icon, Some(Icon::PartlyNight));
        // Readings that only a station can give are simply absent.
        assert_eq!(model.humidity, "");
        assert_eq!(model.pressure_text, "");
    }

    /// One temperature per period plus a daytime flag; the pair is the first of
    /// each.
    #[test]
    fn todays_high_and_tonights_low_come_from_the_periods() {
        let day = day(&forecast_periods(), true);
        assert_eq!(high_low(&day), "H 26°   L 15°");
    }

    #[test]
    fn periods_reshape_into_the_shared_form() {
        let reshaped = reshape_periods(&forecast_periods());
        let parsed = parse_periods(Some(&reshaped));

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "Tonight");
        assert_eq!(parsed[0].temperature, "Low 15°C");
        assert_eq!(parsed[0].wind, "5 to 10 km/h SW");
        assert_eq!(parsed[0].icon, Some(Icon::PartlyNight));
        assert_eq!(parsed[0].precip, "", "a null chance is not a chance");

        assert_eq!(parsed[1].temperature, "High 26°C");
        assert_eq!(parsed[1].precip, "40% rain");
        assert_eq!(parsed[1].icon, Some(Icon::Storm));
        assert!(parsed[1].detailed.contains("chance of showers"));
    }

    /// The name is the service's own for where the coordinate landed.
    #[test]
    fn a_place_name_survives_a_missing_half() {
        let both = json!({"relativeLocation": {"properties": {"city": "Agawam Town", "state": "MA"}}});
        assert_eq!(place(&both), "Agawam Town, MA");

        let city_only = json!({"relativeLocation": {"properties": {"city": "Agawam Town"}}});
        assert_eq!(place(&city_only), "Agawam Town");

        assert_eq!(place(&json!({})), "");
    }

    #[test]
    fn a_coordinate_is_needed_before_anything_is_asked_for() {
        let site = resolve("", "", 5);
        assert!(!site.ok);
        assert_eq!(site.error, "no ZIP code is set");
    }

    #[test]
    fn an_unresolved_site_is_not_polled() {
        let model = fetch(&Site::default(), "42.0", "-72.0", false, 5);
        assert!(!model.ok);
        assert_eq!(model.error, "weather.gov has not been reached yet");
    }

    /// The whole path against the real service. Ignored by default, since it
    /// needs the internet and the answer changes with the weather:
    ///   cargo test -- --ignored --nocapture live_weather
    #[test]
    #[ignore]
    fn live_weather_dot_gov() {
        let zip = std::env::var("KESTREL_TEST_ZIP").unwrap_or_else(|_| "01001".into());
        let found = crate::weather::zip::lookup(&zip).expect("a ZIP in the gazetteer");
        println!("  {zip} -> {}, {}", found.lat, found.lon);

        let site = resolve(&found.lat, &found.lon, 15);
        assert!(site.ok, "resolve failed: {}", site.error);
        println!("  place     {}", site.place);
        println!("  stations  {}", site.stations.len());
        assert!(!site.forecast_url.is_empty());

        let model = fetch(&site, &found.lat, &found.lon, false, 15);
        assert!(model.ok, "fetch failed: {}", model.error);
        println!("  now       {} {}", model.temp_big, model.condition_text);
        println!("  feels     {:?}", model.feels);
        println!("  wind      {:?}", model.wind_text);
        println!("  pressure  {:?}", model.pressure_text);
        println!("  today     {:?}", model.high_low);
        println!("  observed  {:?}", model.observed);
        println!("  icon      {:?}", model.icon);
        println!("  alerts    {}", model.alerts.len());
        for period in model.periods.iter().take(4) {
            println!(
                "    {:<16} {:<12} {:<10} {}",
                period.name, period.temperature, period.precip, period.short
            );
        }

        // The forecast is the fatal half; without it there is nothing to show.
        assert!(!model.periods.is_empty(), "no forecast periods came back");
        assert!(!model.temp_big.is_empty(), "no temperature came back");
    }

    /// The alerts request wants the properties of each GeoJSON feature, not the
    /// feature — getting this wrong shows empty alerts during a tornado watch.
    #[test]
    fn alert_features_are_unwrapped_to_their_properties() {
        let features = json!([
            {"properties": {"event": "Tornado Watch", "severity": "Severe"}},
            {"properties": {"event": "Flood Advisory", "severity": "Minor"}},
        ]);
        let unwrapped = Value::Array(
            features
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|f| f.get("properties").cloned())
                .collect(),
        );
        let parsed = parse_alerts(Some(&unwrapped));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].event, "Tornado Watch");
        assert!(parsed[0].is_severe());
        assert!(!parsed[1].is_severe());
    }
}
