//! The polling loops.
//!
//! Nothing like the camera pollers: a station reports every few minutes at best
//! and a service observation is hourly, so these fetch on a slow cycle and
//! spend the rest of their lives asleep. One reading serves every consumer,
//! which is why there is a single poller for the app rather than one per
//! widget.
//!
//! Both weather sources are polled from here. They differ in what a poll costs
//! — weewx is one request against one document, the service is three against
//! three endpoints — and in nothing else the loop cares about, so the retry and
//! the backoff are shared.
//!
//! The wait is a channel receive rather than a sleep. A five-minute nap is the
//! normal state of these threads, and a process that has been asked to quit
//! should not spend five minutes doing it — dropping the poller closes the
//! channel, which wakes the thread at once.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::debug;

use super::{nws, radar, weewx, Clock, Model};

/// Where a reading comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// The National Weather Service, read over the internet from a ZIP code.
    Nws,
    /// A weewx server on your own network.
    Weewx,
}

impl Source {
    pub fn from_key(key: &str) -> Source {
        // Anything else stored is read as the service, which is the one that
        // works with no address to type.
        if key == "weewx" {
            Source::Weewx
        } else {
            Source::Nws
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Source::Nws => "nws",
            Source::Weewx => "weewx",
        }
    }
}

/// Everything a poll needs, lifted out of preferences so the loop never reads
/// them.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub source: Source,
    /// weewx: the address of the document to read.
    pub url: String,
    pub allow_self_signed: bool,
    /// weather.gov: what the ZIP code resolved to. The service is addressed by
    /// coordinate and knows nothing about ZIP codes.
    pub lat: String,
    pub lon: String,
    /// Only the weather.gov source has anything to decide here: a weewx server
    /// reports in whatever units it is configured for.
    pub metric: bool,
    pub interval: Duration,
}

impl Settings {
    /// Whether there is anything to poll. Switched on with no address is a loop
    /// that would ask nothing forever, and a strip that would never fill in.
    pub fn usable(&self) -> bool {
        match self.source {
            Source::Weewx => !self.url.trim().is_empty(),
            Source::Nws => !self.lat.is_empty() && !self.lon.is_empty(),
        }
    }
}

/// How long a request may take. Generous, because the loop is slow anyway and
/// a station on a congested LAN is more common than one that is gone.
const REQUEST_TIMEOUT: u64 = 10;

/// A weewx archive record is minutes apart. Anything quicker is asking the same
/// numbers again, so the floor is a minute regardless of the setting.
const MINIMUM_INTERVAL: Duration = Duration::from_secs(60);

/// A background loop with a latest reading anyone can look at.
pub struct WeatherPoller {
    latest: Arc<Mutex<Arc<Model>>>,
    stop: mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
    settings: Settings,
}

impl WeatherPoller {
    pub fn start(settings: Settings) -> WeatherPoller {
        let latest = Arc::new(Mutex::new(Arc::new(Model::empty())));
        let (stop, wake) = mpsc::channel();

        let handle = std::thread::Builder::new()
            .name("weather".into())
            .spawn({
                let latest = Arc::clone(&latest);
                let settings = settings.clone();
                move || run(settings, latest, wake)
            })
            .ok();

        WeatherPoller {
            latest,
            stop,
            handle,
            settings,
        }
    }

    /// The last reading, whatever it was.
    ///
    /// Shared rather than handed out behind the lock: the UI reads this every
    /// frame and must never be able to hold the poller up. Behind an `Arc`
    /// rather than cloned, so reading it sixty times a second costs a refcount
    /// bump instead of twenty string allocations.
    pub fn latest(&self) -> Arc<Model> {
        Arc::clone(&self.latest.lock().unwrap())
    }

    /// What this poller was started with, so the app can tell whether an edit
    /// to preferences actually changed anything worth restarting for.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }
}

impl Drop for WeatherPoller {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(settings: Settings, latest: Arc<Mutex<Arc<Model>>>, wake: mpsc::Receiver<()>) {
    let interval = settings.interval.max(MINIMUM_INTERVAL);

    // Which forecast office and which nearby stations a coordinate belongs to.
    // Resolved on the first poll and kept: it takes two requests of its own,
    // and a grid square does not move. A failed resolve leaves it not ok, and
    // the next poll tries again.
    let mut site = nws::Site::default();
    let mut failures: u32 = 0;

    loop {
        let result = match settings.source {
            Source::Weewx => weewx::fetch(&settings.url, settings.allow_self_signed, REQUEST_TIMEOUT),
            Source::Nws => {
                if !site.ok {
                    site = nws::resolve(&settings.lat, &settings.lon, REQUEST_TIMEOUT);
                }
                if site.ok {
                    nws::fetch(
                        &site,
                        &settings.lat,
                        &settings.lon,
                        settings.metric,
                        REQUEST_TIMEOUT,
                    )
                } else {
                    Model::failure(site.error.clone())
                }
            }
        };

        let succeeded = result.ok;
        if !succeeded {
            debug!("weather poll failed: {}", result.error);
        }
        *latest.lock().unwrap() = Arc::new(result);

        // A failure is retried sooner than the next scheduled reading, since
        // the usual cause is the server restarting or a moment of no network,
        // and waiting five minutes to find that out leaves a stale wall. The
        // backoff grows so a server that is genuinely gone is not polled all
        // night at speed.
        let pause = if succeeded {
            failures = 0;
            interval
        } else {
            failures = failures.saturating_add(1);
            let retry = Duration::from_secs(15).saturating_mul(failures);
            retry.min(Duration::from_secs(300)).min(interval)
        };

        match wake.recv_timeout(pause) {
            Err(RecvTimeoutError::Timeout) => {}
            // Asked to stop, or the poller was dropped.
            _ => return,
        }
    }
}

// ---------------------------------------------------------------- radar

/// A mosaic is published about every two minutes. Asking inside that window
/// gets the same list back.
const MINIMUM_RADAR_INTERVAL: Duration = Duration::from_secs(120);

/// A radar request budget. These are small documents, but the map services are
/// slower than api.weather.gov.
const RADAR_TIMEOUT: u64 = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct RadarSettings {
    pub lat: String,
    pub lon: String,
    /// How many sweeps to keep in the loop.
    pub frames: usize,
    pub interval: Duration,
}

impl RadarSettings {
    pub fn usable(&self) -> bool {
        !self.lat.is_empty() && !self.lon.is_empty()
    }
}

/// What the map needs that is not a tile.
///
/// This is all the radar poller does now. The pictures themselves are fetched
/// per tile, on demand, by [`super::tiles::TileStore`] — so what is left is the
/// handful of facts a tile URL cannot be built without: which regional mosaic
/// covers this place, what the place is called, and which sweeps exist.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RadarInfo {
    pub ok: bool,
    pub error: String,
    pub region: String,
    pub place: String,
    /// Oldest first, so the list reads in the order it plays.
    pub times: Vec<String>,
    /// The same, as clocks, worked out here rather than on the UI thread.
    pub clocks: Vec<Option<Clock>>,
    /// Counts up once per completed pass, so the view can tell a new list from
    /// the same one without comparing it.
    pub serial: u64,
}

/// Keeps the loop's timestamps fresh.
///
/// Far cheaper than what it replaces: three small requests on a slow cycle
/// rather than a megabyte of imagery, because the imagery is now the tile
/// pool's business.
pub struct RadarPoller {
    latest: Arc<Mutex<Arc<RadarInfo>>>,
    stop: mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
    settings: RadarSettings,
}

impl RadarPoller {
    pub fn start(settings: RadarSettings) -> RadarPoller {
        let latest = Arc::new(Mutex::new(Arc::new(RadarInfo::default())));
        let (stop, wake) = mpsc::channel();

        let handle = std::thread::Builder::new()
            .name("radar".into())
            .spawn({
                let latest = Arc::clone(&latest);
                let settings = settings.clone();
                move || run_radar(settings, latest, wake)
            })
            .ok();

        RadarPoller {
            latest,
            stop,
            handle,
            settings,
        }
    }

    pub fn latest(&self) -> Arc<RadarInfo> {
        Arc::clone(&self.latest.lock().unwrap())
    }

    pub fn settings(&self) -> &RadarSettings {
        &self.settings
    }
}

impl Drop for RadarPoller {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The longest the radar poll will wait between attempts once it is being
/// refused.
///
/// A service that is turning requests away is not helped by being asked again
/// on the usual schedule, and neither is the client: an edge that rate-limits
/// keeps the block alive while the requests keep coming. Backing off is how a
/// refusal ends rather than persists.
const RADAR_BACKOFF_MAX: Duration = Duration::from_secs(20 * 60);

/// What to show after a poll, given what came back and what is already held.
///
/// An empty answer is a failure, not news. The capabilities document is a
/// couple of hundred kilobytes over somebody else's CDN, and it can be refused,
/// time out, or come back unparseable — while the loop on screen is perfectly
/// good. Publishing the empty list threw that loop away, which stopped the
/// animation and took the timeline with it.
///
/// Returns the times to show and whether this attempt failed.
fn hold_times(fresh: Vec<String>, known: &[String]) -> (Vec<String>, bool) {
    if !fresh.is_empty() {
        return (fresh, false);
    }
    (known.to_vec(), true)
}

/// How long to wait before asking again, after this many failures in a row.
///
/// Doubling, from the ordinary interval up to a ceiling. The first failure is
/// usually nothing and costs one extra interval; a run of them is a service
/// saying no, and the answer to that is to stop asking so often.
fn radar_backoff(interval: Duration, refused: u32) -> Duration {
    if refused == 0 {
        return interval;
    }
    let factor = 1u32 << refused.min(5);
    interval
        .checked_mul(factor)
        .unwrap_or(RADAR_BACKOFF_MAX)
        .min(RADAR_BACKOFF_MAX)
}

fn run_radar(
    settings: RadarSettings,
    latest: Arc<Mutex<Arc<RadarInfo>>>,
    wake: mpsc::Receiver<()>,
) {
    let interval = settings.interval.max(MINIMUM_RADAR_INTERVAL);
    let mut place = String::new();
    let mut serial = 0u64;
    // The loop last known to be good, kept so a refused refresh does not take
    // the one on screen with it.
    let mut known: Vec<String> = Vec::new();
    let mut refused = 0u32;

    loop {
        let mut info = RadarInfo {
            serial: serial + 1,
            ..RadarInfo::default()
        };
        serial = info.serial;

        match (
            settings.lat.parse::<f64>(),
            settings.lon.parse::<f64>(),
        ) {
            (Ok(lat), Ok(lon)) => {
                info.region = radar::region(lat, lon).to_string();

                // The name of the place is asked for once and kept: a forecast
                // office's idea of where a coordinate is does not change, and
                // it costs a request of its own.
                if place.is_empty() {
                    place = radar::place(&settings.lat, &settings.lon, RADAR_TIMEOUT);
                }
                info.place = place.clone();

                let fresh = radar::times(&info.region, settings.frames, RADAR_TIMEOUT);
                let (times, failed) = hold_times(fresh, &known);
                info.times = times;
                if failed {
                    refused = refused.saturating_add(1);
                    info.error = if info.times.is_empty() {
                        // Nothing came back and nothing is held: the map can
                        // still draw the current sweep, which is what an empty
                        // list asks for.
                        "no sweep times available".into()
                    } else {
                        // Something is held, so the loop on screen keeps
                        // playing while this sorts itself out.
                        "sweep times unavailable — holding the loop already \
                         fetched"
                            .into()
                    };
                } else {
                    refused = 0;
                    known = info.times.clone();
                }
                // After the substitution, so a held loop keeps its own clocks.
                info.clocks = info
                    .times
                    .iter()
                    .map(|stamp| super::clock_from_utc(stamp))
                    .collect();
                info.ok = true;
            }
            _ => info.error = "no ZIP code is set".into(),
        }

        if !info.error.is_empty() {
            debug!("radar: {}", info.error);
        }
        *latest.lock().unwrap() = Arc::new(info);

        match wake.recv_timeout(radar_backoff(interval, refused)) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nws_settings() -> Settings {
        Settings {
            source: Source::Nws,
            url: String::new(),
            allow_self_signed: false,
            lat: "42.062".into(),
            lon: "-72.626".into(),
            metric: false,
            interval: Duration::from_secs(300),
        }
    }

    /// A refused refresh must not take the loop on screen with it.
    ///
    /// The capabilities document is a couple of hundred kilobytes over somebody
    /// else's CDN, and it can be refused, time out or come back unparseable
    /// while the loop already fetched is perfectly good. Publishing the empty
    /// list threw that loop away: the animation stopped and the timeline went
    /// with it, because a timeline with no times in it is nothing to draw.
    #[test]
    fn a_refused_refresh_keeps_the_loop_already_fetched() {
        let held = vec!["18:52".to_string(), "18:54".to_string()];

        // Nothing came back: keep what is on screen, and say it failed.
        let (times, failed) = hold_times(Vec::new(), &held);
        assert_eq!(times, held, "the loop on screen survives");
        assert!(failed);

        // Something came back: that is the new truth.
        let fresh = vec!["18:54".to_string(), "18:56".to_string()];
        let (times, failed) = hold_times(fresh.clone(), &held);
        assert_eq!(times, fresh);
        assert!(!failed);

        // Nothing came back and nothing is held: there is genuinely nothing.
        let (times, failed) = hold_times(Vec::new(), &[]);
        assert!(times.is_empty());
        assert!(failed, "and it is still a failure worth backing off from");
    }

    /// A service turning requests away is not helped by being asked again on
    /// the usual schedule, and neither is the client: an edge that rate-limits
    /// keeps the block alive while the requests keep arriving.
    #[test]
    fn being_refused_makes_it_ask_less_often() {
        let interval = Duration::from_secs(150);

        assert_eq!(radar_backoff(interval, 0), interval, "no failures, no change");

        // Doubling, and never going backwards.
        let mut last = radar_backoff(interval, 0);
        for refused in 1..=8 {
            let now = radar_backoff(interval, refused);
            assert!(now >= last, "backoff went backwards at {refused}");
            assert!(now <= RADAR_BACKOFF_MAX, "past the ceiling at {refused}");
            last = now;
        }

        assert!(radar_backoff(interval, 1) > interval, "one failure already waits longer");
        assert_eq!(
            radar_backoff(interval, 40),
            RADAR_BACKOFF_MAX,
            "a service saying no for ever is asked at the slowest rate, not faster"
        );
    }

    /// Switched on with nowhere to point is a loop that asks nothing forever.
    #[test]
    fn a_source_with_no_address_is_not_worth_polling() {
        let mut settings = nws_settings();
        assert!(settings.usable());

        settings.lat.clear();
        assert!(!settings.usable(), "weather.gov needs a coordinate");

        let weewx = Settings {
            source: Source::Weewx,
            url: "   ".into(),
            ..nws_settings()
        };
        assert!(!weewx.usable(), "weewx needs an address");

        let weewx = Settings {
            url: "http://weewx.local/weather.json".into(),
            ..weewx
        };
        assert!(weewx.usable());
    }

    /// A weewx source does not need a ZIP code, and the service does not need
    /// an address — each is judged on what it actually uses.
    #[test]
    fn each_source_is_judged_on_what_it_needs() {
        let weewx = Settings {
            source: Source::Weewx,
            url: "http://weewx.local/weather.json".into(),
            lat: String::new(),
            lon: String::new(),
            ..nws_settings()
        };
        assert!(weewx.usable());
    }

    #[test]
    fn an_unknown_stored_source_reads_as_the_service() {
        assert_eq!(Source::from_key("weewx"), Source::Weewx);
        assert_eq!(Source::from_key("nws"), Source::Nws);
        assert_eq!(Source::from_key(""), Source::Nws);
        assert_eq!(Source::from_key("wunderground"), Source::Nws);
        // And round-trips, so a config file survives a save.
        for source in [Source::Nws, Source::Weewx] {
            assert_eq!(Source::from_key(source.key()), source);
        }
    }

    /// A poller with nothing to reach still has to start, stop and hand out a
    /// model — the strip is built before the first reply either way.
    #[test]
    fn a_poller_starts_empty_and_stops_promptly() {
        let poller = WeatherPoller::start(Settings {
            source: Source::Weewx,
            url: "http://127.0.0.1:1/nothing.json".into(),
            interval: Duration::from_secs(600),
            ..nws_settings()
        });
        assert!(!poller.latest().ok, "nothing has arrived yet");
        assert_eq!(poller.settings().source, Source::Weewx);

        // Dropping must not wait out the interval.
        let started = std::time::Instant::now();
        drop(poller);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "dropping a sleeping poller should be immediate"
        );
    }

    #[test]
    fn a_radar_poller_needs_somewhere_to_point() {
        let mut settings = RadarSettings {
            lat: "42.062".into(),
            lon: "-72.626".into(),
            frames: 10,
            interval: Duration::from_secs(180),
        };
        assert!(settings.usable());
        settings.lon.clear();
        assert!(!settings.usable());
    }
}
