//! Stills of cameras that sleep.
//!
//! A battery camera is asleep until something talks to it, and a live stream
//! is something talking to it without pause: on the wall it would be flat by
//! the end of the week. So a battery camera's tile shows a still, taken every
//! so often, and goes live only while someone is looking at it closely (see
//! `ui::app`). This is where the stills come from.
//!
//! Each still is the device's own snapshot (`Vendor::snapshot`), decoded and
//! cut down to at most [`MAX_WIDTH`] across: on a wall of tiles nothing more is
//! ever drawn, and a 4K JPEG decoded at full size is 33 MB of RGBA to hold for
//! a picture the size of a postcard.
//!
//! One fetch per device at a time. A camera behind a hub takes seconds to wake,
//! and the hub is the one link to all of them; a standalone camera has only the
//! one channel. Fetches are few — one per camera per interval — so each is a
//! thread of its own rather than a pool.
//!
//! The last still of each camera is kept on disk ([`cache_dir`]), so a restart
//! shows the picture it had rather than waking every battery camera at once to
//! find out. The interval counts from when that still was taken.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::stream::Frame;
use crate::api::models::{BatteryLevel, Charge};

/// A camera, as the wall keys its connection: device id and channel.
pub type Key = (String, u32);

/// Widest a still is kept. A tile on a 4K wall is rarely wider than this, and
/// an expanded battery camera goes live anyway.
pub const MAX_WIDTH: u32 = 1920;

/// Fetches a camera's JPEG. A closure rather than the device manager, so the
/// scheduling can be tested without a device.
pub type Fetcher = Arc<dyn Fn(&Key) -> Result<Shot, String> + Send + Sync>;

/// What one fetch brings back: the picture, and the battery's charge if the
/// camera said — asked in the same wake, so it costs the battery nothing more.
#[derive(Clone, Default)]
pub struct Shot {
    pub jpeg: Vec<u8>,
    pub battery: Option<BatteryLevel>,
}

/// Asks a camera's battery alone, without a picture. `None` where it does not
/// say.
pub type BatteryFetcher = Arc<dyn Fn(&Key) -> Result<Option<BatteryLevel>, String> + Send + Sync>;

impl From<Vec<u8>> for Shot {
    fn from(jpeg: Vec<u8>) -> Shot {
        Shot { jpeg, battery: None }
    }
}

/// What a tile needs to draw a camera's still.
#[derive(Clone, Default)]
pub struct StillView {
    pub frame: Option<Arc<Frame>>,
    /// When the picture was taken.
    pub taken: Option<SystemTime>,
    /// A fetch is under way: the camera is being woken.
    pub fetching: bool,
    /// Why the last fetch failed, if it did.
    pub error: Option<String>,
    /// The battery as of the last still that brought word of it.
    pub battery: Option<BatteryLevel>,
}

#[derive(Default)]
struct Entry {
    view: StillView,
    /// When a fetch last started, or when the cached still was taken. The
    /// interval counts from here whether that fetch worked or not: a camera
    /// that did not answer is not asked again any sooner, which would cost the
    /// battery more for a camera that is, if anything, already struggling.
    last_attempt: Option<SystemTime>,
    /// Ask the battery on its own at the next chance: the camera is new to
    /// the wall (its still may be a kept one, fresh enough to wait out its
    /// interval), or its live view just ended without a still being taken.
    battery_wanted: bool,
}

enum Outcome {
    Still(Result<(Arc<Frame>, Shot), String>),
    Battery(Result<Option<BatteryLevel>, String>),
}

struct Done {
    key: Key,
    outcome: Outcome,
}

pub struct Stills {
    fetch: Fetcher,
    battery: BatteryFetcher,
    entries: HashMap<Key, Entry>,
    /// Devices with a fetch under way.
    busy: HashSet<String>,
    asked_now: Vec<Key>,
    done_tx: Sender<Done>,
    done_rx: Receiver<Done>,
    cache: Option<PathBuf>,
}

/// Where the last still of each camera is kept between runs.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("kestrel")
        .join("stills")
}

/// Stills count from a high number so a tile switching between a still and a
/// stream never mistakes one picture for the other by its sequence.
static SEQUENCE: AtomicU64 = AtomicU64::new(1 << 62);

impl Stills {
    pub fn new(fetch: Fetcher, battery: BatteryFetcher, cache: Option<PathBuf>) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        Stills {
            fetch,
            battery,
            entries: HashMap::new(),
            busy: HashSet::new(),
            asked_now: Vec::new(),
            done_tx,
            done_rx,
            cache,
        }
    }

    /// The still to draw for a camera, if it has one or is fetching one.
    pub fn view(&self, key: &Key) -> Option<StillView> {
        self.entries.get(key).map(|e| e.view.clone())
    }

    /// Keep a picture from somewhere else — the last frame of a live view that
    /// just ended — as this camera's still. It is the newest there is, and the
    /// camera was awake to send it, so the interval starts again from now.
    pub fn offer(&mut self, key: Key, frame: Arc<Frame>) {
        let now = SystemTime::now();
        let entry = self.entries.entry(key.clone()).or_default();
        entry.view.frame = Some(Arc::clone(&frame));
        entry.view.taken = Some(now);
        entry.view.error = None;
        entry.last_attempt = Some(now);
        // The camera is still awake from the live view: ask while it is.
        entry.battery_wanted = true;
        if let Some(dir) = self.cache.clone() {
            // Encoding a full frame takes a moment; not on the UI thread.
            std::thread::spawn(move || {
                if let Some(jpeg) = encode(&frame) {
                    save(&dir, &key, &jpeg);
                }
            });
        }
    }

    /// Take a still of this camera at the next [`Stills::tick`], whatever its
    /// interval says.
    pub fn take_now(&mut self, key: Key) {
        if !self.asked_now.contains(&key) {
            self.asked_now.push(key);
        }
    }

    /// Collect finished fetches and start the ones due. `cameras` is every
    /// battery camera on the wall, each with its interval (`None`: only when
    /// asked). Returns whether anything changed, for a repaint.
    pub fn tick(&mut self, cameras: &[(Key, Option<Duration>)], now: SystemTime) -> bool {
        let mut changed = false;
        while let Ok(done) = self.done_rx.try_recv() {
            self.busy.remove(&done.key.0);
            let entry = self.entries.entry(done.key.clone()).or_default();
            let result = match done.outcome {
                Outcome::Battery(result) => {
                    match result {
                        Ok(Some(level)) => {
                            changed |= entry.view.battery != Some(level);
                            entry.view.battery = Some(level);
                            if let Some(dir) = &self.cache {
                                save_battery(dir, &done.key, level);
                            }
                        }
                        Ok(None) => log::info!("battery camera {}/{}: the device gives no battery level", done.key.0, done.key.1),
                        Err(err) => log::info!("battery camera {}/{}: no battery level: {err}", done.key.0, done.key.1),
                    }
                    continue;
                }
                Outcome::Still(result) => result,
            };
            entry.view.fetching = false;
            match result {
                Ok((frame, shot)) => {
                    entry.view.frame = Some(frame);
                    entry.view.taken = Some(SystemTime::now());
                    entry.view.error = None;
                    // A camera that did not say this time keeps what it said last.
                    if shot.battery.is_some() {
                        entry.view.battery = shot.battery;
                    }
                    if let Some(dir) = &self.cache {
                        save(dir, &done.key, &shot.jpeg);
                        if let Some(level) = shot.battery {
                            save_battery(dir, &done.key, level);
                        }
                    }
                }
                Err(err) => {
                    log::info!("battery camera {}/{}: no still: {err}", done.key.0, done.key.1);
                    entry.view.error = Some(err);
                }
            }
            changed = true;
        }

        for (key, every) in cameras {
            if !self.entries.contains_key(key) {
                let mut entry = Entry { battery_wanted: true, ..Entry::default() };
                if let Some((frame, taken)) = self.cache.as_deref().and_then(|dir| load(dir, key)) {
                    entry.view.frame = Some(frame);
                    entry.view.taken = Some(taken);
                    entry.view.battery = self.cache.as_deref().and_then(|dir| load_battery(dir, key));
                    entry.last_attempt = Some(taken);
                    changed = true;
                }
                self.entries.insert(key.clone(), entry);
            }
            let asked = self.asked_now.contains(key);
            let entry = &self.entries[key];
            let (still_due, battery_wanted) = (asked || due(entry.last_attempt, *every, now), entry.battery_wanted);
            if still_due && self.start(key, now) {
                self.asked_now.retain(|k| k != key);
                changed = true;
            } else if battery_wanted && !still_due {
                self.start_battery(key);
            }
        }
        // Asked of a camera no longer on the wall: nothing to show it on.
        self.asked_now.retain(|k| cameras.iter().any(|(c, _)| c == k));
        changed
    }

    fn start(&mut self, key: &Key, now: SystemTime) -> bool {
        if self.busy.contains(&key.0) {
            return false;
        }
        self.busy.insert(key.0.clone());
        let entry = self.entries.entry(key.clone()).or_default();
        entry.last_attempt = Some(now);
        entry.view.fetching = true;
        // A still asks the battery as well.
        entry.battery_wanted = false;
        let (fetch, tx, key) = (Arc::clone(&self.fetch), self.done_tx.clone(), key.clone());
        std::thread::Builder::new()
            .name("still".into())
            .spawn(move || {
                let result = fetch(&key).and_then(|shot| decode(&shot.jpeg).map(|frame| (Arc::new(frame), shot)));
                let _ = tx.send(Done { key, outcome: Outcome::Still(result) });
            })
            .is_ok()
    }

    /// Ask a camera's battery alone, if its device is free. Shows nothing on
    /// the tile while it goes: no picture is coming.
    fn start_battery(&mut self, key: &Key) {
        if self.busy.contains(&key.0) {
            return;
        }
        self.busy.insert(key.0.clone());
        if let Some(entry) = self.entries.get_mut(key) {
            entry.battery_wanted = false;
        }
        let (battery, tx, job) = (Arc::clone(&self.battery), self.done_tx.clone(), key.clone());
        let spawned = std::thread::Builder::new().name("battery".into()).spawn(move || {
            let result = battery(&job);
            let _ = tx.send(Done { key: job, outcome: Outcome::Battery(result) });
        });
        if spawned.is_err() {
            self.busy.remove(&key.0);
        }
    }
}

/// Whether a camera's next still is due: never tried, or tried `every` ago.
pub fn due(last_attempt: Option<SystemTime>, every: Option<Duration>, now: SystemTime) -> bool {
    let Some(every) = every else { return false };
    match last_attempt {
        None => true,
        // A clock set back makes `last` look like the future: take one now
        // rather than wait out however far it moved.
        Some(last) => now.duration_since(last).map_or(true, |age| age >= every),
    }
}

/// A JPEG as a frame for a tile, no wider than [`MAX_WIDTH`].
pub fn decode(jpeg: &[u8]) -> Result<Frame, String> {
    let image = image::load_from_memory(jpeg).map_err(|e| format!("not a picture: {e}"))?;
    let image = if image.width() > MAX_WIDTH {
        let height = (image.height() as u64 * MAX_WIDTH as u64 / image.width() as u64).max(1) as u32;
        image.resize_exact(MAX_WIDTH, height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Ok(Frame {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
        sequence: SEQUENCE.fetch_add(1, Ordering::Relaxed),
    })
}

fn encode(frame: &Frame) -> Option<Vec<u8>> {
    let rgba = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())?;
    let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
    let mut out = std::io::Cursor::new(Vec::new());
    rgb.write_to(&mut out, image::ImageFormat::Jpeg).ok()?;
    Some(out.into_inner())
}

fn file_for(dir: &Path, key: &Key) -> PathBuf {
    let device: String = key.0.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    dir.join(format!("{device}-{}.jpg", key.1))
}

/// The battery beside its still, as one line: percent and charge.
fn battery_file_for(dir: &Path, key: &Key) -> PathBuf {
    file_for(dir, key).with_extension("battery")
}

fn save_battery(dir: &Path, key: &Key, level: BatteryLevel) {
    let charge = match level.charge {
        Charge::Discharging => "discharging",
        Charge::Charging => "charging",
        Charge::Full => "chargecomplete",
    };
    let _ = std::fs::write(battery_file_for(dir, key), format!("{} {charge}\n", level.percent));
}

fn load_battery(dir: &Path, key: &Key) -> Option<BatteryLevel> {
    let text = std::fs::read_to_string(battery_file_for(dir, key)).ok()?;
    let mut words = text.split_whitespace();
    let percent: u8 = words.next()?.parse().ok()?;
    Some(BatteryLevel { percent: percent.min(100), charge: Charge::parse(words.next().unwrap_or("")) })
}

fn save(dir: &Path, key: &Key, jpeg: &[u8]) {
    if let Err(err) = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(file_for(dir, key), jpeg)) {
        log::debug!("could not keep a still for {}/{}: {err}", key.0, key.1);
    }
}

fn load(dir: &Path, key: &Key) -> Option<(Arc<Frame>, SystemTime)> {
    let path = file_for(dir, key);
    let taken = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
    let frame = decode(&std::fs::read(&path).ok()?).ok()?;
    Some((Arc::new(frame), taken))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    const MINUTE: Duration = Duration::from_secs(60);

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 40, 40]));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    /// Ticks until nothing is fetching any more, or a few seconds pass.
    fn settle(stills: &mut Stills, cameras: &[(Key, Option<Duration>)], now: SystemTime) {
        let began = Instant::now();
        while began.elapsed() < Duration::from_secs(5) {
            stills.tick(cameras, now);
            let asked = stills.asked_now.iter().any(|k| cameras.iter().any(|(c, _)| c == k));
            if stills.busy.is_empty() && !asked {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("fetches did not finish");
    }

    fn no_battery() -> BatteryFetcher {
        Arc::new(|_: &Key| Ok(None))
    }

    fn counting(picture: Vec<u8>) -> (Fetcher, Arc<Mutex<Vec<Key>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);
        let fetch: Fetcher = Arc::new(move |key: &Key| {
            log.lock().unwrap().push(key.clone());
            Ok(Shot { jpeg: picture.clone(), battery: Some(BatteryLevel { percent: 64, charge: Charge::Discharging }) })
        });
        (fetch, asked)
    }

    #[test]
    fn a_still_is_due_once_per_interval() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(due(None, Some(MINUTE), t0), "never taken: take one");
        assert!(!due(Some(t0), Some(MINUTE), t0 + MINUTE / 2));
        assert!(due(Some(t0), Some(MINUTE), t0 + MINUTE));
        assert!(!due(None, None, t0), "off: only when asked");
        assert!(due(Some(t0 + MINUTE), Some(MINUTE * 15), t0), "a clock set back does not stall it");
    }

    #[test]
    fn a_camera_is_asked_once_per_interval_and_its_picture_kept() {
        let (fetch, asked) = counting(jpeg(64, 36));
        let mut stills = Stills::new(fetch, no_battery(), None);
        let cam: Key = ("hub".into(), 2);
        let cameras = [(cam.clone(), Some(MINUTE * 15))];
        let t0 = SystemTime::now();

        settle(&mut stills, &cameras, t0);
        let view = stills.view(&cam).unwrap();
        let frame = view.frame.expect("a picture");
        assert_eq!((frame.width, frame.height), (64, 36));
        assert!(view.error.is_none() && view.taken.is_some());

        settle(&mut stills, &cameras, t0 + MINUTE * 14);
        assert_eq!(asked.lock().unwrap().len(), 1, "not again inside the interval");
        settle(&mut stills, &cameras, t0 + MINUTE * 15);
        assert_eq!(asked.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_camera_that_does_not_say_keeps_its_last_battery() {
        let said = Arc::new(Mutex::new(true));
        let say = Arc::clone(&said);
        let picture = jpeg(8, 8);
        let fetch: Fetcher = Arc::new(move |_: &Key| {
            let battery = (*say.lock().unwrap()).then_some(BatteryLevel { percent: 81, charge: Charge::Charging });
            Ok(Shot { jpeg: picture.clone(), battery })
        });
        let mut stills = Stills::new(fetch, no_battery(), None);
        let cam: Key = ("cam".into(), 0);
        let cameras = [(cam.clone(), Some(MINUTE))];
        let t0 = SystemTime::now();
        settle(&mut stills, &cameras, t0);
        assert_eq!(stills.view(&cam).unwrap().battery, Some(BatteryLevel { percent: 81, charge: Charge::Charging }));
        *said.lock().unwrap() = false;
        settle(&mut stills, &cameras, t0 + MINUTE);
        assert_eq!(stills.view(&cam).unwrap().battery.map(|b| b.percent), Some(81));
    }

    #[test]
    fn a_failure_waits_out_the_interval_too() {
        let tries = Arc::new(Mutex::new(0));
        let count = Arc::clone(&tries);
        let fetch: Fetcher = Arc::new(move |_: &Key| {
            *count.lock().unwrap() += 1;
            Err("asleep".to_string())
        });
        let mut stills = Stills::new(fetch, no_battery(), None);
        let cam: Key = ("cam".into(), 0);
        let cameras = [(cam.clone(), Some(MINUTE * 5))];
        let t0 = SystemTime::now();
        settle(&mut stills, &cameras, t0);
        assert_eq!(stills.view(&cam).unwrap().error.as_deref(), Some("asleep"));
        settle(&mut stills, &cameras, t0 + MINUTE);
        assert_eq!(*tries.lock().unwrap(), 1, "a camera that did not answer is not pestered");
        settle(&mut stills, &cameras, t0 + MINUTE * 5);
        assert_eq!(*tries.lock().unwrap(), 2);
    }

    #[test]
    fn stills_off_takes_one_only_when_asked() {
        let (fetch, asked) = counting(jpeg(8, 8));
        let mut stills = Stills::new(fetch, no_battery(), None);
        let cam: Key = ("cam".into(), 0);
        let cameras = [(cam.clone(), None)];
        let t0 = SystemTime::now();
        settle(&mut stills, &cameras, t0);
        assert!(asked.lock().unwrap().is_empty());
        stills.take_now(cam.clone());
        settle(&mut stills, &cameras, t0);
        assert_eq!(asked.lock().unwrap().len(), 1);
    }

    #[test]
    fn one_fetch_per_device_at_a_time() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let (inner, asked) = counting(jpeg(8, 8));
        let wait = Arc::clone(&gate);
        let fetch: Fetcher = Arc::new(move |key: &Key| {
            drop(wait.lock().unwrap());
            inner(key)
        });
        let mut stills = Stills::new(fetch, no_battery(), None);
        let cameras = [
            (("hub".to_string(), 0), Some(MINUTE)),
            (("hub".to_string(), 1), Some(MINUTE)),
            (("other".to_string(), 0), Some(MINUTE)),
        ];
        let t0 = SystemTime::now();
        stills.tick(&cameras, t0);
        let fetching = |s: &Stills| cameras.iter().filter(|(k, _)| s.view(k).is_some_and(|v| v.fetching)).count();
        assert_eq!(fetching(&stills), 2, "the hub's second camera waits; another device does not");
        drop(held);
        settle(&mut stills, &cameras, t0);
        settle(&mut stills, &cameras, t0);
        assert_eq!(asked.lock().unwrap().len(), 3, "the waiting one goes once the first is done");
    }

    #[test]
    fn a_restart_shows_the_kept_still_without_waking_the_camera() {
        let dir = std::env::temp_dir().join(format!("kestrel-stills-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cam: Key = ("hub/1".into(), 3);
        let cameras = [(cam.clone(), Some(MINUTE * 15))];

        let (fetch, _) = counting(jpeg(32, 18));
        let mut first = Stills::new(fetch, no_battery(), Some(dir.clone()));
        settle(&mut first, &cameras, SystemTime::now());
        assert!(dir.join("hub_1-3.jpg").exists());

        let (fetch, asked) = counting(jpeg(32, 18));
        let battery: BatteryFetcher = Arc::new(|_: &Key| Ok(Some(BatteryLevel { percent: 55, charge: Charge::Discharging })));
        let mut second = Stills::new(fetch, battery, Some(dir.clone()));
        settle(&mut second, &cameras, SystemTime::now());
        let view = second.view(&cam).unwrap();
        assert_eq!(view.frame.map(|f| f.width), Some(32));
        assert!(asked.lock().unwrap().is_empty(), "the kept still is fresh enough");
        // Its battery is asked on its own, without a picture.
        let began = Instant::now();
        while second.view(&cam).unwrap().battery.map(|b| b.percent) != Some(55) {
            assert!(began.elapsed() < Duration::from_secs(5), "the battery was not asked");
            second.tick(&cameras, SystemTime::now());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(asked.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_large_still_is_cut_down() {
        let frame = decode(&jpeg(3840, 2160)).unwrap();
        assert_eq!((frame.width, frame.height), (MAX_WIDTH, 1080));
        assert_eq!(frame.rgba.len(), (MAX_WIDTH * 1080 * 4) as usize);
        assert!(decode(b"{\"error\": 1}").is_err());
    }
}
