//! Detection polling and follow-motion.
//!
//! Reolink has no push channel over the documented HTTP API, so detections are
//! *level-triggered* flags that stay set while the condition holds. The poller
//! keeps the previous sample per channel and reports only rising edges, which
//! is what a person means by "an event".

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use log::debug;

use crate::api::EventKind;
use crate::manager::DeviceManager;

pub type Key = (String, u32);

#[derive(Debug, Clone)]
pub struct Event {
    pub key: Key,
    pub channel_name: String,
    pub kind: EventKind,
    pub at: DateTime<Local>,
}

/// Cap the feed so a busy camera cannot grow it without bound.
const MAX_EVENTS: usize = 500;

#[derive(Default)]
struct Shared {
    /// Newest first.
    feed: Mutex<Vec<Event>>,
    /// Events not yet seen by the UI, for notifications.
    pending: Mutex<Vec<Event>>,
    /// What each channel is detecting right now, by device key. Empty means
    /// nothing — kept per kind so following can be limited to some of them.
    active: Mutex<HashMap<Key, Vec<String>>>,
}

pub struct EventPoller {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl EventPoller {
    pub fn start(manager: DeviceManager, interval: Duration) -> Self {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));

        let join = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("event-poller".into())
                .spawn(move || run(manager, shared, stop, interval.max(Duration::from_millis(500))))
                .expect("failed to spawn the event poller")
        };

        EventPoller {
            shared,
            stop,
            join: Some(join),
        }
    }

    /// Events detected since the last call, oldest first.
    pub fn take_new(&self) -> Vec<Event> {
        std::mem::take(&mut *self.shared.pending.lock().unwrap())
    }

    pub fn feed(&self) -> Vec<Event> {
        self.shared.feed.lock().unwrap().clone()
    }

    pub fn clear_feed(&self) {
        self.shared.feed.lock().unwrap().clear();
    }

    /// Channels currently detecting one of `kinds`, by device key.
    ///
    /// Follow-motion needs the *level*, not just the rising edge: a camera with
    /// someone walking across it reports one edge and then stays active for as
    /// long as the motion lasts. Feeding only edges lets the dwell timer expire
    /// mid-event and drops the camera while it is still the interesting one.
    pub fn active_keys_for(&self, kinds: &[String]) -> Vec<Key> {
        self.shared
            .active
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, active)| active.iter().any(|kind| kinds.iter().any(|k| k == kind)))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// What a channel is detecting right now, for the tile badges.
    pub fn active_kinds(&self, key: &Key) -> Vec<EventKind> {
        self.shared
            .active
            .lock()
            .unwrap()
            .get(key)
            .map(|raw| {
                let mut kinds: Vec<EventKind> =
                    raw.iter().filter_map(|k| EventKind::from_device(k)).collect();
                // A stable order, so badges do not jump about as the poller
                // returns the same set in a different sequence.
                kinds.sort_by_key(|k| k.device_key());
                kinds.dedup();
                kinds
            })
            .unwrap_or_default()
    }

    /// Whether anything at all is being detected, for the status indicators.
    pub fn is_active(&self, key: &Key) -> bool {
        self.shared
            .active
            .lock()
            .unwrap()
            .get(key)
            .map(|kinds| !kinds.is_empty())
            .unwrap_or(false)
    }
}

impl Drop for EventPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(manager: DeviceManager, shared: Arc<Shared>, stop: Arc<AtomicBool>, interval: Duration) {
    // Previous sample per (device, channel, detection type), so only a
    // transition from idle to active counts as an event.
    let mut previous: HashMap<(String, u32, String), bool> = HashMap::new();
    // Log a device's failure once rather than every cycle.
    let mut complained: HashMap<String, bool> = HashMap::new();

    while !stop.load(Ordering::Relaxed) {
        for config in manager.configs() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let Some(client) = manager.client(&config.id) else { continue };
            let channels: Vec<u32> = client
                .channels()
                .iter()
                .filter(|c| c.online)
                .map(|c| c.index)
                .collect();
            if channels.is_empty() {
                continue;
            }

            // One call now, whatever the system: a vendor without detection
            // answers with an empty list rather than an error.
            let detections = match client.detections(&channels) {
                Ok(state) => state,
                Err(err) => {
                    if complained.insert(config.id.clone(), true).is_none() {
                        debug!("motion poll failed for {}: {err}", config.host);
                    }
                    continue;
                }
            };
            complained.remove(&config.id);

            let now = Local::now();
            for channel in client.channels() {
                if !channels.contains(&channel.index) {
                    continue;
                }
                let flags: Vec<(String, bool)> = detections
                    .iter()
                    .find(|(c, _)| *c == channel.index)
                    .map(|(_, flags)| flags.clone())
                    .unwrap_or_default();

                let key: Key = (config.id.clone(), channel.index);
                let mut active_kinds: Vec<String> = Vec::new();
                for (raw_kind, active) in flags {
                    if active {
                        active_kinds.push(raw_kind.clone());
                    }
                    let slot = (config.id.clone(), channel.index, raw_kind.clone());
                    let was = previous.insert(slot, active).unwrap_or(false);

                    if active && !was {
                        if let Some(kind) = EventKind::from_device(&raw_kind) {
                            let event = Event {
                                key: key.clone(),
                                channel_name: manager.channel_label(&config.id, channel),
                                kind,
                                at: now,
                            };
                            let mut feed = shared.feed.lock().unwrap();
                            feed.insert(0, event.clone());
                            feed.truncate(MAX_EVENTS);
                            shared.pending.lock().unwrap().push(event);
                        }
                    }
                }
                shared.active.lock().unwrap().insert(key, active_kinds);
            }
        }

        // Sleep in slices so a stop request is honoured promptly.
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

// ==================================================================== follower

/// Cap on simultaneous cameras. Beyond this the most recently active win —
/// sixteen moving cameras is not a useful thing to look at.
const MAX_FOLLOWED: usize = 9;

/// Turns the poller's samples into a stable set of cameras worth showing.
pub struct Follower {
    pub enabled: bool,
    dwell: Duration,
    last_seen: HashMap<Key, Instant>,
    current: Vec<Key>,
}

impl Follower {
    pub fn new(dwell: Duration) -> Self {
        Follower {
            enabled: false,
            dwell: dwell.max(Duration::from_secs(1)),
            last_seen: HashMap::new(),
            current: Vec::new(),
        }
    }

    pub fn set_dwell(&mut self, dwell: Duration) {
        self.dwell = dwell.max(Duration::from_secs(1));
    }

    pub fn dwell(&self) -> Duration {
        self.dwell
    }

    pub fn active(&self) -> &[Key] {
        &self.current
    }

    /// Record a detection.
    ///
    /// Only activity is recorded; the dwell timer handles the tail, so an
    /// "inactive" sample is deliberately ignored rather than immediately
    /// dropping the camera and flicking the view away.
    pub fn note(&mut self, key: Key) {
        if self.enabled {
            self.last_seen.insert(key, Instant::now());
        }
    }

    pub fn clear(&mut self) {
        self.last_seen.clear();
        self.current.clear();
    }

    /// Drop one camera, without waiting for its dwell to run out.
    ///
    /// The dwell is a tail on activity, and a camera that has just been
    /// excluded from following has no business sitting out the rest of one: the
    /// answer to "stop showing me this" is not "in up to two minutes". The
    /// selection itself is left alone — [`Self::evaluate`] recomputes it from
    /// what is still here, and reports the change through the one path.
    pub fn forget(&mut self, key: &Key) {
        self.last_seen.remove(key);
    }

    /// Forget what was last handed out, so the next [`Self::evaluate`] reports
    /// its selection again even though nothing about the detections changed.
    ///
    /// For when something outside has moved the view off that selection: a
    /// camera deleted from under it, or a wall that rebuilt to nothing. The
    /// selection is only ever reported *on change*, which is what keeps a
    /// quiet scene free — and the cost of that is a selection nobody is showing
    /// any more looks identical to one already on screen, so following goes
    /// quiet until the detections happen to change on their own.
    ///
    /// Deliberately not [`Self::clear`]: what is still detecting is still
    /// detecting, and a camera should not lose the dwell it has earned because
    /// somebody deleted a crop of it.
    pub fn resend(&mut self) {
        self.current.clear();
    }

    /// Recompute the selection. Returns it only when it actually changed, so a
    /// quiet scene costs nothing and a re-triggering camera does not rebuild
    /// the grid.
    pub fn evaluate(&mut self) -> Option<Vec<Key>> {
        if !self.enabled {
            if self.current.is_empty() {
                return None;
            }
            self.current.clear();
            return Some(Vec::new());
        }

        let cutoff = Instant::now() - self.dwell;
        self.last_seen.retain(|_, seen| *seen > cutoff);

        let mut ranked: Vec<(&Key, &Instant)> = self.last_seen.iter().collect();
        // Most recent first, so the cap keeps the freshest activity.
        ranked.sort_by(|a, b| b.1.cmp(a.1));
        let mut selection: Vec<Key> = ranked
            .into_iter()
            .take(MAX_FOLLOWED)
            .map(|(key, _)| key.clone())
            .collect();
        // Order by camera, not recency: a grid that reshuffles every time one
        // camera re-triggers is unwatchable.
        selection.sort();

        if selection == self.current {
            return None;
        }
        debug!("follow-motion: {:?} -> {:?}", self.current, selection);
        self.current = selection.clone();
        Some(selection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u32) -> Key {
        ("dev".to_string(), n)
    }

    /// The selection is reported only when it changes, which is what keeps a
    /// quiet scene free — and is exactly why something that moves the view off
    /// that selection has to say so.
    ///
    /// Deleting a virtual camera did not. The spotlight was edited in place,
    /// the follower still believed it had handed out that selection, and
    /// because the cameras detecting had not changed it never reported again —
    /// so following went quiet until the detections happened to change on
    /// their own.
    #[test]
    fn a_selection_nobody_is_showing_is_reported_again() {
        let mut follower = Follower::new(Duration::from_secs(30));
        follower.enabled = true;
        follower.note(key(1));
        follower.note(key(2));

        let first = follower.evaluate().expect("a fresh selection is reported");
        assert_eq!(first, vec![key(1), key(2)]);
        assert!(
            follower.evaluate().is_none(),
            "nothing changed, so there is nothing to say"
        );

        // Something outside took the view off that selection.
        follower.resend();
        let again = follower.evaluate().expect("it has to be reported again");
        assert_eq!(again, first, "and it is the same selection, not a lesser one");

        // Once said, it goes quiet again rather than rebuilding every frame.
        assert!(follower.evaluate().is_none());
    }

    /// Re-reporting must not cost a camera the dwell it has earned. Somebody
    /// deleting a crop of a camera has said nothing about the camera, which may
    /// well be the one still detecting.
    #[test]
    fn re_reporting_keeps_what_is_still_detecting() {
        let mut follower = Follower::new(Duration::from_secs(30));
        follower.enabled = true;
        follower.note(key(1));
        follower.evaluate().expect("a selection");

        follower.resend();
        assert_eq!(
            follower.evaluate().expect("reported again"),
            vec![key(1)],
            "the camera is still detecting and keeps its place"
        );

        // Where clearing really is meant, it still empties the whole thing.
        follower.clear();
        assert!(follower.active().is_empty());
        assert!(follower.evaluate().is_none(), "and there is nothing to report");
    }

    #[test]
    fn a_disabled_follower_records_nothing() {
        let mut follower = Follower::new(Duration::from_secs(10));
        follower.note(key(0));
        assert_eq!(follower.evaluate(), None);
    }

    #[test]
    fn emits_only_when_the_set_changes() {
        let mut follower = Follower::new(Duration::from_secs(10));
        follower.enabled = true;

        follower.note(key(0));
        assert_eq!(follower.evaluate(), Some(vec![key(0)]));
        // Re-triggering the same camera must not rebuild the view.
        follower.note(key(0));
        assert_eq!(follower.evaluate(), None);

        follower.note(key(3));
        assert_eq!(follower.evaluate(), Some(vec![key(0), key(3)]));
    }

    #[test]
    fn order_is_stable_rather_than_by_recency() {
        let mut follower = Follower::new(Duration::from_secs(10));
        follower.enabled = true;
        follower.note(key(5));
        follower.note(key(1));
        assert_eq!(follower.evaluate(), Some(vec![key(1), key(5)]));
    }

    #[test]
    fn dwell_expiry_releases_the_view() {
        let mut follower = Follower::new(Duration::from_secs(10));
        follower.enabled = true;
        follower.note(key(0));
        follower.evaluate();

        // Pretend the detection happened long ago.
        follower
            .last_seen
            .insert(key(0), Instant::now() - Duration::from_secs(60));
        assert_eq!(follower.evaluate(), Some(Vec::new()));
    }

    #[test]
    fn caps_simultaneous_cameras() {
        let mut follower = Follower::new(Duration::from_secs(10));
        follower.enabled = true;
        for channel in 0..20 {
            follower.note(key(channel));
        }
        assert_eq!(follower.evaluate().unwrap().len(), MAX_FOLLOWED);
    }

    /// A camera excluded while it is on screen leaves at once, rather than
    /// sitting out the rest of a dwell that may be two minutes long — which is
    /// the case that prompts somebody to exclude it in the first place.
    #[test]
    fn a_forgotten_camera_leaves_the_view_at_once() {
        let mut follower = Follower::new(Duration::from_secs(120));
        follower.enabled = true;
        follower.note(key(0));
        follower.note(key(1));
        assert_eq!(follower.evaluate(), Some(vec![key(0), key(1)]));

        follower.forget(&key(0));
        assert_eq!(follower.evaluate(), Some(vec![key(1)]));

        // And it stays gone until something notes it again.
        assert_eq!(follower.evaluate(), None);
    }

    #[test]
    fn disabling_hands_the_view_back() {
        let mut follower = Follower::new(Duration::from_secs(10));
        follower.enabled = true;
        follower.note(key(0));
        follower.evaluate();

        follower.enabled = false;
        assert_eq!(follower.evaluate(), Some(Vec::new()));
    }
}
