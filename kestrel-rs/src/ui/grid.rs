//! Live-grid stream management: which cameras are on screen, and what happens
//! to their connections when they leave.
//!
//! Opening an RTSP stream is expensive — measured at 6.1s to first frame on an
//! RLN36, roughly half RTSP setup and half waiting for a keyframe. Everything
//! here exists to avoid paying that twice:
//!
//!   * streams are keyed by **camera**, not by tile, so a camera and every
//!     virtual camera cropped out of it share one connection and one decode,
//!   * a connection whose last tile leaves the screen is **parked** briefly,
//!     still streaming, in case the user comes straight back,
//!   * beyond that it hands its connection to the **warm pool**, which keeps
//!     demuxing without decoding (~0.8% CPU versus ~2.7%), and
//!   * expanding a camera **upgrades in place**: the live sub stream keeps
//!     playing while the main stream connects behind it.
//!
//! Ownership lives here rather than on the tiles. A tile holds a [`StreamView`]
//! — a reader's handle with no claim on the connection — and the set of tiles
//! the wall just built *is* the refcount: [`Streams::release`] parks whatever
//! nothing is looking at any more. That is what makes a second tile on the same
//! camera free, and it is why the arithmetic is done once per rebuild instead
//! of being threaded through every place a tile can come and go.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use log::debug;

use crate::api::{SourceId, StreamType};
use crate::api::vendor::StreamSource;
use crate::video::{Retirer, StreamView, StreamWorker};

use super::tile::Tile;

/// What a connection is keyed by: a device and one of its channels.
///
/// Deliberately *not* [`SourceId`]. A virtual camera is a rectangle drawn out
/// of a picture, not a picture of its own, so it adds nothing here — and
/// keeping the two apart is what stops the wall opening a second RTSP session
/// for a crop.
pub type Key = (String, u32);

/// How long a stream that leaves the screen keeps running.
const PARK: Duration = Duration::from_secs(20);
/// Upper bound on parked streams, so retained connections cannot pile up.
const MAX_PARKED: usize = 8;
/// How long a tile that leaves the screen keeps its framing and its texture.
///
/// Longer than the stream's grace period costs nothing — a tile is a rectangle
/// and a picture already decoded — and it is what lets a page turned away from
/// and back to return to the same crop rather than to the whole picture.
const PARK_TILE: Duration = Duration::from_secs(60);
/// Upper bound on parked tiles.
const MAX_PARKED_TILES: usize = 32;
/// Delay before warming starts, so it never competes with the tiles the user is
/// actually waiting for.
const WARM_DELAY: Duration = Duration::from_millis(2500);

pub struct Streams {
    pub retirer: Retirer,
    /// Connections feeding tiles on screen, and which stream each is pulling.
    hot: HashMap<Key, (StreamWorker, StreamType)>,
    /// Connections whose last tile just left, still running.
    parked: HashMap<Key, (StreamWorker, StreamType, Instant)>,
    /// Tiles whose cell just went away, kept for their framing and texture.
    parked_tiles: HashMap<SourceId, (Tile, Instant)>,
    warm: HashMap<Key, StreamWorker>,
    /// Main streams connecting behind a tile that still shows its sub stream.
    upgrades: HashMap<Key, StreamWorker>,
    warm_after: Option<Instant>,
    pub warm_enabled: bool,
    pub max_warm: usize,
}

impl Default for Streams {
    fn default() -> Self {
        Streams {
            retirer: Retirer::default(),
            hot: HashMap::new(),
            parked: HashMap::new(),
            parked_tiles: HashMap::new(),
            warm: HashMap::new(),
            upgrades: HashMap::new(),
            warm_after: None,
            warm_enabled: true,
            max_warm: 16,
        }
    }
}

impl Streams {
    pub fn warm_count(&self) -> usize {
        self.warm.len()
    }

    pub fn parked_count(&self) -> usize {
        self.parked.len()
    }

    /// How many connections are feeding the wall. One per camera, however many
    /// of its views are on screen.
    pub fn hot_count(&self) -> usize {
        self.hot.len()
    }

    pub fn pending_upgrades(&self) -> usize {
        self.upgrades.len()
    }

    /// Ask for the warm pool to be topped up once the view has settled.
    pub fn schedule_warm(&mut self) {
        if self.warm_enabled && self.max_warm > 0 {
            self.warm_after = Some(Instant::now() + WARM_DELAY);
        }
    }

    // ------------------------------------------------------------- on screen

    /// The connection already feeding this camera's tiles, if there is one.
    ///
    /// The first thing every tile asks. A camera's second tile — its first
    /// virtual camera — is answered here and costs nothing further.
    pub fn hot_view(&self, key: &Key) -> Option<(StreamView, StreamType)> {
        self.hot.get(key).map(|(worker, kind)| (worker.view(), *kind))
    }

    /// Give this camera a connection pulling `kind`, as cheaply as possible.
    ///
    /// Reuses what is already running, then what was parked, then what is warm,
    /// and only opens a new one when none of those will do.
    pub fn bind(
        &mut self,
        key: Key,
        kind: StreamType,
        source: StreamSource,
        name: String,
    ) -> StreamView {
        if let Some((worker, running)) = self.hot.get(&key) {
            if *running == kind {
                return worker.view();
            }
        }
        // Wanted something else than what is running: the old connection has no
        // reader left the moment this returns.
        if let Some((previous, _)) = self.hot.remove(&key) {
            self.retirer.retire(previous);
        }

        if let Some((worker, parked_kind, _)) = self.parked.remove(&key) {
            if parked_kind == kind {
                worker.go_hot();
                let view = worker.view();
                self.hot.insert(key, (worker, kind));
                return view;
            }
            self.retirer.retire(worker);
        }

        if kind == StreamType::Sub {
            if let Some(worker) = self.warm.remove(&key) {
                worker.go_hot();
                let view = worker.view();
                self.hot.insert(key, (worker, kind));
                return view;
            }
        }

        let worker = StreamWorker::start(source, name, false);
        let view = worker.view();
        self.hot.insert(key, (worker, kind));
        view
    }

    /// Adopt a warm connection as this camera's live sub stream.
    ///
    /// Used on the way to the main stream: something live to look at while the
    /// main stream connects behind it beats a frozen frame.
    pub fn adopt_warm(&mut self, key: &Key) -> Option<StreamView> {
        if self.hot.contains_key(key) {
            return None;
        }
        let worker = self.warm.remove(key)?;
        worker.go_hot();
        let view = worker.view();
        self.hot.insert(key.clone(), (worker, StreamType::Sub));
        Some(view)
    }

    /// Park every connection no tile on screen is reading any more.
    ///
    /// Called once at the end of a rebuild with the cameras it just bound. The
    /// set of live keys is the refcount: a camera stays connected while any of
    /// its tiles — the camera's own or any crop of it — is on the wall.
    pub fn release(&mut self, live: &HashSet<Key>) {
        let leaving: Vec<Key> = self
            .hot
            .keys()
            .filter(|key| !live.contains(*key))
            .cloned()
            .collect();
        for key in leaving {
            self.cancel_upgrade(&key);
            let Some((worker, kind)) = self.hot.remove(&key) else { continue };
            if self.parked.len() < MAX_PARKED {
                self.parked.insert(key, (worker, kind, Instant::now() + PARK));
                continue;
            }
            // Past the parking limit the stream used to be thrown away, which
            // meant a full reconnect — and several seconds of "Waiting for
            // video…" — on the way back. Keep it warm instead.
            self.keep_or_retire(key, worker, kind);
        }
    }

    /// Point the sound at one camera, and silence the rest.
    ///
    /// Asked of the connections rather than of the tiles because that is where
    /// the answer lives: a camera on the wall twice is still one microphone.
    pub fn set_audio(&self, audible: Option<&Key>) {
        for (key, (worker, _)) in &self.hot {
            worker.set_audio(Some(key) == audible);
        }
    }

    /// The connection behind one camera, for recording and reconnecting.
    pub fn hot_worker(&self, key: &Key) -> Option<&StreamWorker> {
        self.hot.get(key).map(|(worker, _)| worker)
    }

    /// Drop one camera's connection so the next rebuild opens a fresh one.
    pub fn drop_hot(&mut self, key: &Key) {
        self.cancel_upgrade(key);
        self.parked.remove(key);
        if let Some(worker) = self.warm.remove(key) {
            self.retirer.retire(worker);
        }
        if let Some((worker, _)) = self.hot.remove(key) {
            self.retirer.retire(worker);
        }
    }

    // ------------------------------------------------------------- leaving

    /// Keep a tile's framing and its texture for the way back.
    ///
    /// The stream is not this tile's to give up — [`Streams::release`] decides
    /// what happens to connections. What is kept here is the part that would
    /// otherwise be rebuilt from nothing: a virtual camera's current crop, and
    /// a decoded picture to draw the moment the cell comes back.
    pub fn park_tile(&mut self, mut tile: Tile) {
        tile.stream = None;
        // A half-placed box on a picture nobody is looking at is not a thing to
        // hand back later: whoever left the page has moved on from it.
        tile.cancel_framing();
        if self.parked_tiles.len() >= MAX_PARKED_TILES {
            return;
        }
        self.parked_tiles
            .insert(tile.key(), (tile, Instant::now() + PARK_TILE));
    }

    pub fn unpark_tile(&mut self, id: &SourceId) -> Option<Tile> {
        self.parked_tiles.remove(id).map(|(tile, _)| tile)
    }

    /// Move a connection into the warm pool rather than closing it.
    fn keep_or_retire_inner(&mut self, key: Key, worker: StreamWorker, kind: StreamType) {
        if !self.warm_enabled
            || self.warm.contains_key(&key)
            || self.warm.len() >= self.max_warm
            || kind != StreamType::Sub
        {
            self.retirer.retire(worker);
            return;
        }
        worker.go_warm();
        self.warm.insert(key, worker);
    }

    /// Give a swapped-out sub stream to the warm pool, or retire it.
    pub fn keep_or_retire(&mut self, key: Key, worker: StreamWorker, kind: StreamType) {
        self.keep_or_retire_inner(key, worker, kind);
    }

    /// Drop parked streams and tiles whose grace period has run out, keeping
    /// the streams warm where there is room.
    pub fn expire_parked(&mut self) {
        let now = Instant::now();
        let expired: Vec<Key> = self
            .parked
            .iter()
            .filter(|(_, (_, _, deadline))| *deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            if let Some((worker, kind, _)) = self.parked.remove(&key) {
                self.keep_or_retire_inner(key, worker, kind);
            }
        }

        self.parked_tiles.retain(|_, (_, deadline)| *deadline > now);
    }

    // ------------------------------------------------------------- upgrades

    /// Open the main stream behind a tile that keeps showing its sub stream.
    ///
    /// Switching straight to main would leave the viewer on a frozen frame for
    /// the seconds it takes to connect, so the swap waits until the main stream
    /// has actually decoded a picture.
    pub fn begin_upgrade(&mut self, key: Key, source: StreamSource, name: String) {
        if self.upgrades.contains_key(&key) {
            return;
        }
        debug!("upgrading {name} to the main stream behind its sub stream");
        self.upgrades.insert(key, StreamWorker::start(source, name, false));
    }

    pub fn cancel_upgrade(&mut self, key: &Key) {
        if let Some(worker) = self.upgrades.remove(key) {
            self.retirer.retire(worker);
        }
    }

    /// Any upgrade that has produced a frame, ready to be swapped in.
    pub fn ready_upgrades(&mut self) -> Vec<(Key, StreamWorker)> {
        let ready: Vec<Key> = self
            .upgrades
            .iter()
            .filter(|(_, worker)| worker.latest_frame().is_some())
            .map(|(key, _)| key.clone())
            .collect();
        ready
            .into_iter()
            .filter_map(|key| self.upgrades.remove_entry(&key))
            .collect()
    }

    /// Put a connected main stream in place of the sub stream it grew behind,
    /// and hand back the view every tile of that camera should now read.
    ///
    /// The sub stream it replaces goes to the warm pool where there is room, so
    /// the way back to the grid is instant.
    pub fn promote(&mut self, key: Key, worker: StreamWorker) -> StreamView {
        let view = worker.view();
        let previous = self.hot.insert(key.clone(), (worker, StreamType::Main));
        if let Some((previous, kind)) = previous {
            self.keep_or_retire_inner(key, previous, kind);
        }
        view
    }

    // ------------------------------------------------------------- warming

    /// Bring background cameras online if the delay has elapsed.
    ///
    /// A camera already decoding its sub stream on screen has no use for a warm
    /// copy of it. One shown on *main* does: its sub stream stays warm so
    /// returning to the grid is instant.
    pub fn top_up_warm(
        &mut self,
        candidates: &[(Key, String)],
        source_for: impl Fn(&Key) -> Option<StreamSource>,
    ) {
        if !self.warm_enabled || self.max_warm == 0 {
            return;
        }
        match self.warm_after {
            Some(when) if Instant::now() >= when => self.warm_after = None,
            Some(_) => return,
            None => return,
        }

        let on_screen_sub: Vec<Key> = self
            .hot
            .iter()
            .filter(|(_, (_, kind))| *kind == StreamType::Sub)
            .map(|(key, _)| key.clone())
            .collect();

        let live: Vec<Key> = candidates.iter().map(|(key, _)| key.clone()).collect();
        let stale: Vec<Key> = self
            .warm
            .keys()
            .filter(|key| !live.contains(key) || on_screen_sub.contains(key))
            .cloned()
            .collect();
        for key in stale {
            if let Some(worker) = self.warm.remove(&key) {
                self.retirer.retire(worker);
            }
        }

        for (key, name) in candidates {
            if self.warm.len() >= self.max_warm {
                break;
            }
            if self.warm.contains_key(key)
                || on_screen_sub.contains(key)
                || self.parked.contains_key(key)
            {
                continue;
            }
            if let Some(source) = source_for(key) {
                self.warm
                    .insert(key.clone(), StreamWorker::start(source, format!("{name} (warm)"), true));
            }
        }
        if !self.warm.is_empty() {
            debug!("warm pool: {} stream(s)", self.warm.len());
        }
    }

    pub fn clear_warm(&mut self) {
        for (_, worker) in self.warm.drain() {
            self.retirer.retire(worker);
        }
    }

    /// Release everything, for shutdown.
    pub fn shutdown(&mut self) {
        for key in self.upgrades.keys().cloned().collect::<Vec<_>>() {
            self.cancel_upgrade(&key);
        }
        self.clear_warm();
        self.parked_tiles.clear();
        for (_, (worker, _, _)) in self.parked.drain().collect::<Vec<_>>() {
            self.retirer.retire(worker);
        }
        for (_, (worker, _)) in self.hot.drain().collect::<Vec<_>>() {
            self.retirer.retire(worker);
        }
        self.retirer.drain(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A URL that refuses immediately, so a worker can be started and stopped
    /// without a camera and without waiting on a network timeout.
    fn nowhere() -> StreamSource {
        StreamSource {
            url: "rtsp://127.0.0.1:1/nothing".into(),
            headers: Vec::new(),
        }
    }

    /// The claim the whole design rests on: a camera on the wall three times —
    /// itself and two crops of it — is one RTSP session and one decode.
    #[test]
    fn one_camera_is_one_connection_however_many_views_it_has() {
        let mut streams = Streams::default();
        streams.warm_enabled = false;
        let key = ("nvr".to_string(), 0);

        let camera = streams.bind(key.clone(), StreamType::Sub, nowhere(), "Driveway".into());
        assert_eq!(streams.hot_count(), 1);

        // Each crop asks the same question and is answered out of what is
        // already running.
        for name in ["Gate", "Porch"] {
            assert!(
                streams.hot_view(&key).is_some(),
                "{name} finds the camera already connected"
            );
            let _ = streams.bind(key.clone(), StreamType::Sub, nowhere(), name.into());
        }
        assert_eq!(streams.hot_count(), 1, "still one connection");

        // And it is the same one the camera itself is reading: both report the
        // same stream state because there is only one stream.
        let crop = streams.hot_view(&key).unwrap().0;
        assert_eq!(camera.state().0, crop.state().0);

        streams.shutdown();
    }

    /// The set of tiles on the wall is the refcount. A camera keeps its
    /// connection while any of its views is on screen, and parks only when the
    /// last one goes.
    #[test]
    fn a_connection_lives_while_any_view_of_it_is_on_the_wall() {
        let mut streams = Streams::default();
        streams.warm_enabled = false;
        let key = ("nvr".to_string(), 0);
        let other = ("nvr".to_string(), 1);

        let _ = streams.bind(key.clone(), StreamType::Sub, nowhere(), "Driveway".into());
        let _ = streams.bind(other.clone(), StreamType::Sub, nowhere(), "Side".into());
        assert_eq!(streams.hot_count(), 2);

        // The camera's own tile leaves, but a crop of it stays: the wall still
        // lists the camera, so nothing is released.
        let live: HashSet<Key> = [key.clone()].into_iter().collect();
        streams.release(&live);
        assert_eq!(streams.hot_count(), 1, "the crop keeps it connected");
        assert_eq!(streams.parked_count(), 1, "the other camera parked");
        assert!(streams.hot_view(&key).is_some());

        // Now the last view goes too.
        streams.release(&HashSet::new());
        assert_eq!(streams.hot_count(), 0);
        assert!(streams.hot_view(&key).is_none());
        assert_eq!(streams.parked_count(), 2);

        // And coming straight back adopts the parked connection rather than
        // opening a new one.
        let _ = streams.bind(key.clone(), StreamType::Sub, nowhere(), "Driveway".into());
        assert_eq!(streams.hot_count(), 1);
        assert_eq!(streams.parked_count(), 1, "it came out of the park");

        streams.shutdown();
    }

    /// Wanting a different stream than the one running replaces it, because a
    /// camera and its crops cannot read different halves of one connection.
    #[test]
    fn asking_for_the_other_stream_replaces_the_connection() {
        let mut streams = Streams::default();
        streams.warm_enabled = false;
        let key = ("nvr".to_string(), 0);

        let _ = streams.bind(key.clone(), StreamType::Sub, nowhere(), "Driveway".into());
        assert_eq!(streams.hot_view(&key).unwrap().1, StreamType::Sub);

        let _ = streams.bind(key.clone(), StreamType::Main, nowhere(), "Driveway".into());
        assert_eq!(streams.hot_view(&key).unwrap().1, StreamType::Main);
        assert_eq!(streams.hot_count(), 1, "not two");

        streams.shutdown();
    }

    /// A tile is parked for its framing and its picture, separately from the
    /// connection under it — so a page turned away from and back to returns to
    /// the same crop rather than to the whole camera.
    #[test]
    fn a_tile_keeps_its_framing_across_a_page_turn() {
        use crate::api::{Channel, SourceId};

        let mut streams = Streams::default();
        let id = SourceId::virtual_camera("nvr", 0, "gate");
        let mut tile = Tile::cropped(id.clone(), Channel::new(0), "Gate".into(), 2.5, (0.6, 0.4));
        tile.zoom_to_for_test(5.0, (0.2, 0.8));

        streams.park_tile(tile);
        assert!(streams.unpark_tile(&SourceId::camera("nvr", 0)).is_none(), "a different tile");

        let back = streams.unpark_tile(&id).expect("the tile comes back");
        assert_eq!(back.framing().0, 5.0, "where it was left, not where it was saved");
        assert!(!back.is_streaming(), "and it asks for its picture again");
        // Its saved framing came back with it.
        let mut back = back;
        back.reset_zoom();
        assert_eq!(back.framing(), (2.5, (0.6, 0.4)));
    }
}
