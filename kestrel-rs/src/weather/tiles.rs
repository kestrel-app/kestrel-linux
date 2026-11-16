//! The Web Mercator tile grid, and the pool that fetches tiles.
//!
//! The radar used to be three fixed pictures — a basemap, a reflectivity layer
//! and the warning polygons — all requested for one bounding box and stacked.
//! That is a screenshot of a map rather than a map: every pan or zoom threw
//! away a megabyte and asked for another, so moving around meant waiting, and
//! the picture could only ever be the shape it was requested at.
//!
//! This is the arrangement every slippy map uses instead. The world is cut into
//! 256-pixel tiles on a fixed grid, doubling each zoom level; a view asks only
//! for the tiles it can see, keeps them, and reuses them the moment they come
//! back into frame. Panning costs the few tiles at the leading edge. Zooming
//! reuses the level above until the new one arrives. Nothing is letterboxed,
//! because the grid has no aspect of its own — it fills whatever rectangle it
//! is drawn into.
//!
//! The tiles come from two services, which speak different dialects, which is
//! why the URLs live in [`super::radar`] rather than here: this file knows
//! about a grid and a queue, and nothing about weather. The warning polygons
//! were a third until they stopped being pictures — they are geometry now, and
//! [`super::warnings`] has them.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use log::debug;

use super::radar::TileError;

/// Tiles are 256 pixels square, which is what every service here serves and
/// what the zoom levels are defined against.
pub const TILE_PX: u32 = 256;

/// The world, in pixels, at a zoom level.
pub fn world_px(level: u32) -> f64 {
    TILE_PX as f64 * (1u64 << level) as f64
}

/// Longitude to a horizontal pixel on the world grid.
pub fn lon_to_px(lon: f64, level: u32) -> f64 {
    (lon + 180.0) / 360.0 * world_px(level)
}

/// Latitude to a vertical pixel. The Mercator stretch lives here.
pub fn lat_to_px(lat: f64, level: u32) -> f64 {
    let radians = lat.clamp(-85.05112878, 85.05112878).to_radians();
    let y = (1.0 - (radians.tan() + 1.0 / radians.cos()).ln() / std::f64::consts::PI) / 2.0;
    y * world_px(level)
}

pub fn px_to_lon(px: f64, level: u32) -> f64 {
    px / world_px(level) * 360.0 - 180.0
}

pub fn px_to_lat(px: f64, level: u32) -> f64 {
    let y = 0.5 - px / world_px(level);
    (std::f64::consts::PI * 2.0 * y).sinh().atan().to_degrees()
}

/// Half the world in Web Mercator metres — the projection's own edge, which is
/// where the grid's zero and its width both come from.
pub const EDGE: f64 = 20_037_508.342_789_244;

/// One tile's bounding box in Web Mercator metres, which is what the services
/// that take a box want.
pub fn tile_bbox(level: u32, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let span = EDGE * 2.0 / (1u64 << level) as f64;
    let left = -EDGE + x as f64 * span;
    let top = EDGE - y as f64 * span;
    (left, top - span, left + span, top)
}

// ---------------------------------------------------------------- the store

/// Which picture a tile belongs to. Kept small and copyable: it is half of
/// every cache key, and there is one lookup per tile per layer per frame.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Layer {
    /// The ground. One per basemap style.
    Base,
    /// Place names, where the style serves them separately.
    Labels,
    /// Reflectivity, at one of the loop's timestamps.
    ///
    /// The only layer here that has to be a picture. The watch and warning
    /// polygons used to be two more, and are now fetched as geometry and drawn
    /// as shapes - see [`super::warnings`]. Reflectivity is a raster product
    /// and there is no vector form of it to fetch: GeoServer will answer a
    /// request for GeoJSON on the mosaic, and what comes back is one feature
    /// holding the coverage's bounding rectangle.
    Radar(usize),
}

impl Layer {
    /// Which set of URLs a layer is built from.
    ///
    /// The sweep list is refreshed every couple of minutes, and only the
    /// reflectivity is keyed by it — the ground and its lettering are keyed by
    /// *place*, and a tile of them already on the wire is still the right
    /// pixels afterwards. Grouping them says which is which, so a refresh
    /// throws away the one and not the other.
    fn group(self) -> usize {
        match self {
            Layer::Radar(_) => SWEEPS,
            _ => GROUND,
        }
    }
}

const GROUND: usize = 0;
const SWEEPS: usize = 1;

/// What re-pointing the pool invalidates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Invalidates {
    /// A different basemap or a different mosaic: nothing in flight survives.
    Everything,
    /// A fresh sweep list, which is most re-pointings. The ground is untouched.
    OnlyTheSweeps,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TileId {
    pub layer: Layer,
    pub level: u32,
    pub x: u32,
    pub y: u32,
}

/// A decoded tile, waiting to be uploaded.
pub struct Decoded {
    pub id: TileId,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Set instead of `rgba` for the reflectivity, which is traced into the
    /// bands it encloses rather than kept as a picture — see
    /// [`super::reflectivity::contour`]. Every other layer is a picture and
    /// leaves this empty.
    pub shapes: Option<super::reflectivity::Shapes>,
    /// Whether anything at all is painted on it. A fully transparent radar tile
    /// is a clear sky, and is worth knowing about rather than re-requesting.
    pub opaque: bool,
}

/// What a caller has to supply to turn a tile into a URL. Passed in rather than
/// reached for, so this file stays ignorant of weather services.
/// What a caller has to supply to turn a tile into a URL. Passed in rather than
/// reached for, so this file stays ignorant of weather services.
pub type UrlFor = Arc<dyn Fn(&TileId) -> Option<String> + Send + Sync>;

struct Queue {
    /// Tiles for what is on screen this instant.
    now: VecDeque<TileId>,
    /// The rest of the loop, taken only when nothing on screen is waiting.
    ///
    /// Two queues rather than one because they are wanted on completely
    /// different timescales, and with one queue the urgent work went behind the
    /// speculative work. A large window on a dense screen sees over a hundred
    /// tiles, and ten sweeps of those is several hundred requests; with a
    /// single line, every basemap tile needed after a pan, a zoom or a sweep
    /// refresh queued behind all of them and the map sat black for as long as
    /// that took to drain. Dragging the map appeared to fix it because dropping
    /// the queue is exactly what a drag does.
    soon: VecDeque<TileId>,
    /// What is in the two queues, for deciding whether a tile is already asked
    /// for.
    ///
    /// A set rather than a walk of the queue. This is asked once per tile per
    /// layer per frame — hundreds of times at sixty frames a second — and the
    /// walk was done holding the one lock the workers need to make any progress
    /// at all, so the queue got longer roughly as fast as it was served.
    queued: HashSet<TileId>,
    inflight: HashSet<TileId>,
    /// Bumped when a service is re-pointed, so a tile that lands afterwards can
    /// be recognised as belonging to a map nobody is looking at any more.
    ///
    /// One per [`Layer::group`], because a sweep list refreshing every couple
    /// of minutes must not throw away the ground.
    epochs: [u64; 2],
    url_for: Option<UrlFor>,
    stop: bool,
}

impl Queue {
    /// Add a tile, unless it is already asked for.
    fn add(&mut self, id: TileId, urgent: bool) -> bool {
        if self.inflight.contains(&id) || !self.queued.insert(id.clone()) {
            return false;
        }
        if urgent {
            self.now.push_back(id);
        } else {
            self.soon.push_back(id);
        }
        true
    }

    /// The next tile to fetch: what is on screen first, always.
    ///
    /// The rest of the loop is only worth a connection once nothing visible is
    /// waiting on one.
    fn take(&mut self) -> Option<TileId> {
        let id = self.now.pop_front().or_else(|| self.soon.pop_front())?;
        self.queued.remove(&id);
        Some(id)
    }

    /// Drop everything queued that a re-pointing invalidated.
    fn discard(&mut self, stale: impl Fn(&TileId) -> bool) {
        self.now.retain(|id| !stale(id));
        self.soon.retain(|id| !stale(id));
        self.queued.retain(|id| !stale(id));
    }
}

/// How long a tile that failed for a reason that might pass is left alone.
const RETRY_AFTER: Duration = Duration::from_secs(4);
/// How many times such a tile is retried before it is given up on.
const RETRIES: u32 = 3;

/// Tiles that are not coming, and why.
#[derive(Default)]
struct Failures {
    /// The service will not serve these. Asking again is a wasted request.
    refused: HashSet<TileId>,
    /// These were in the way of something — how many times, and when last.
    blocked: HashMap<TileId, (u32, Instant)>,
}

impl Failures {
    /// Whether to leave a tile alone rather than ask for it again.
    ///
    /// Clears the record when a blocked tile has cooled off, so the caller that
    /// gets `false` is the one that goes on to make the request.
    fn skip(&mut self, id: &TileId) -> bool {
        if self.refused.contains(id) {
            return true;
        }
        match self.blocked.get(id) {
            Some((_, at)) if at.elapsed() < RETRY_AFTER => true,
            Some(_) => {
                self.blocked.remove(id);
                false
            }
            None => false,
        }
    }

    /// Record a tile that did not arrive.
    ///
    /// A run of blocked attempts eventually counts as a refusal: a host that is
    /// down should not be asked for five hundred tiles every few seconds for as
    /// long as the map is open.
    fn record(&mut self, id: TileId, error: &TileError) {
        match error {
            TileError::Refused(_) => {
                self.refused.insert(id);
            }
            TileError::Blocked(_) => {
                let attempt = self.blocked.entry(id.clone()).or_insert((0, Instant::now()));
                attempt.0 += 1;
                attempt.1 = Instant::now();
                if attempt.0 >= RETRIES {
                    self.blocked.remove(&id);
                    self.refused.insert(id);
                }
            }
        }
    }
}

/// Fetches tiles on a small pool of threads and hands back decoded pixels.
///
/// The pool is deliberately small. These are public services and a slippy map
/// can ask for a lot at once; six at a time keeps a pan responsive without
/// opening a connection per tile on screen.
pub struct TileStore {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    done: Arc<Mutex<Vec<Decoded>>>,
    failed: Arc<Mutex<Failures>>,
    stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

pub const WORKERS: usize = 6;

impl TileStore {
    pub fn new() -> TileStore {
        let queue = Arc::new((
            Mutex::new(Queue {
                now: VecDeque::new(),
                soon: VecDeque::new(),
                queued: HashSet::new(),
                inflight: HashSet::new(),
                epochs: [0; 2],
                url_for: None,
                stop: false,
            }),
            Condvar::new(),
        ));
        let done = Arc::new(Mutex::new(Vec::new()));
        let failed = Arc::new(Mutex::new(Failures::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let workers = (0..WORKERS)
            .filter_map(|n| {
                let queue = Arc::clone(&queue);
                let done = Arc::clone(&done);
                let failed = Arc::clone(&failed);
                let stop = Arc::clone(&stop);
                std::thread::Builder::new()
                    .name(format!("tiles-{n}"))
                    .spawn(move || run(queue, done, failed, stop))
                    .ok()
            })
            .collect();

        TileStore {
            queue,
            done,
            failed,
            stop,
            workers,
        }
    }

    /// Point the pool at a set of services, dropping the work the change
    /// invalidates and keeping the work it does not.
    pub fn set_source(&self, url_for: UrlFor, what: Invalidates) {
        let (lock, wake) = &*self.queue;
        let mut queue = lock.lock().unwrap();
        queue.url_for = Some(url_for);

        let stale = move |id: &TileId| match what {
            Invalidates::Everything => true,
            Invalidates::OnlyTheSweeps => id.layer.group() == SWEEPS,
        };

        queue.epochs[SWEEPS] += 1;
        if what == Invalidates::Everything {
            queue.epochs[GROUND] += 1;
        }
        queue.discard(stale);

        let mut failed = self.failed.lock().unwrap();
        failed.refused.retain(|id| !stale(id));
        failed.blocked.retain(|id, _| !stale(id));
        wake.notify_all();
    }

    /// Ask for a tile that is on screen now.
    pub fn request(&self, id: TileId) {
        self.enqueue(id, true);
    }

    /// Ask for a tile that will be wanted shortly — the sweeps either side of
    /// the one showing. Served only once nothing on screen is waiting.
    pub fn prefetch(&self, id: TileId) {
        self.enqueue(id, false);
    }

    fn enqueue(&self, id: TileId, urgent: bool) {
        if self.failed.lock().unwrap().skip(&id) {
            return;
        }
        let (lock, wake) = &*self.queue;
        if lock.lock().unwrap().add(id, urgent) {
            wake.notify_one();
        }
    }

    /// Drop anything queued that has not started. Called when the view moves,
    /// so the pool works on what is on screen now rather than where it was.
    pub fn forget_pending(&self) {
        let (lock, _) = &*self.queue;
        let mut queue = lock.lock().unwrap();
        queue.now.clear();
        queue.soon.clear();
        queue.queued.clear();
    }

    /// Take everything that has arrived since the last call.
    pub fn collect(&self) -> Vec<Decoded> {
        std::mem::take(&mut *self.done.lock().unwrap())
    }

    /// Hand a finished tile straight to the pool, as if a worker had fetched
    /// it.
    ///
    /// Test-only, and here for the same reason the other test hooks in this
    /// file are: the property worth asserting is what happens to the view when
    /// a tile lands, and there is no other way to land one without a network.
    #[cfg(test)]
    pub fn deliver(&self, decoded: Decoded) {
        self.done.lock().unwrap().push(decoded);
    }

    pub fn pending(&self) -> usize {
        let (lock, _) = &*self.queue;
        let queue = lock.lock().unwrap();
        queue.now.len() + queue.soon.len() + queue.inflight.len()
    }
}

impl Default for TileStore {
    fn default() -> Self {
        TileStore::new()
    }
}

impl Drop for TileStore {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        {
            let (lock, wake) = &*self.queue;
            let mut queue = lock.lock().unwrap();
            queue.stop = true;
            queue.now.clear();
            queue.soon.clear();
            queue.queued.clear();
            wake.notify_all();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn run(
    queue: Arc<(Mutex<Queue>, Condvar)>,
    done: Arc<Mutex<Vec<Decoded>>>,
    failed: Arc<Mutex<Failures>>,
    stop: Arc<AtomicBool>,
) {
    loop {
        let (id, url, epoch) = {
            let (lock, wake) = &*queue;
            let mut guard = lock.lock().unwrap();
            loop {
                if guard.stop || stop.load(Ordering::Relaxed) {
                    return;
                }
                match guard.take() {
                    Some(id) => {
                        let url = guard.url_for.as_ref().and_then(|f| f(&id));
                        match url {
                            Some(url) => {
                                let epoch = guard.epochs[id.layer.group()];
                                guard.inflight.insert(id.clone());
                                break (id, url, epoch);
                            }
                            // A layer this style does not serve — the dark
                            // canvas is the only one with separate lettering.
                            None => continue,
                        }
                    }
                    None => guard = wake.wait(guard).unwrap(),
                }
            }
        };

        let outcome = super::radar::fetch_tile(&url, &id);

        {
            let (lock, _) = &*queue;
            let mut guard = lock.lock().unwrap();
            guard.inflight.remove(&id);
            // The service this belongs to was re-pointed while it was in the
            // air; the pixels are for a map nobody is looking at.
            if guard.epochs[id.layer.group()] != epoch {
                continue;
            }
        }

        match outcome {
            Ok(decoded) => {
                failed.lock().unwrap().blocked.remove(&id);
                done.lock().unwrap().push(Decoded { id, ..decoded });
            }
            // Either a tile the service will never serve — outside the mosaic's
            // region, past the level it publishes — or something that was
            // merely in the way. Opening the map asks for hundreds of tiles at
            // once and a few of them lose that race every time; treating that
            // as final is what left holes in the basemap that nothing short of
            // changing the style would fill in.
            Err(err) => {
                debug!("tile {id:?}: {err}");
                failed.lock().unwrap().record(id, &err);
            }
        }
    }
}

// ---------------------------------------------------------------- viewport

/// Where the map is looking: a centre, and a zoom that need not be a whole
/// level.
///
/// Fractional zoom is what makes a wheel feel continuous. Tiles only exist at
/// whole levels, so the nearest level is drawn scaled by the remainder — the
/// same thing every map does between steps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub lat: f64,
    pub lon: f64,
    pub zoom: f64,
    /// How much finer the tiles should be than the zoom alone implies, in
    /// levels. Set from the screen each frame rather than stored: it is a
    /// property of the display, not of where the map is looking.
    ///
    /// On a 2× display a 256-pixel tile drawn into 256 *points* is stretched
    /// across 512 real pixels, which is the difference between a map and a
    /// blurred one. A level deeper is four tiles in place of one at exactly the
    /// right size.
    pub detail: f64,
}

/// Below this the mosaic is a smear and the basemap is a world map.
///
/// The upper bound is past where the reflectivity has more to give — a mosaic
/// cell is about a kilometre, so by here one is several pixels across and the
/// echoes are visibly blocky. It is allowed anyway: the map underneath keeps
/// getting sharper, and somebody asking which side of a town it is raining on
/// wants the streets even if the rain is coarse. The service's own viewer goes
/// further still.
pub const MIN_ZOOM: f64 = 3.0;
pub const MAX_ZOOM: f64 = 12.0;

/// The most tiles one pass over the view will ever list, whatever it was asked
/// for. A backstop against a queue thousands long, not a way of choosing a
/// level — that is [`Viewport::level_within`], and a caller that uses it never
/// comes near this.
///
/// Kept clear of the budgets rather than close to them. At 256 it sat *below*
/// what a 4K screen legitimately wants for a view of the country, so it would
/// have silently truncated the map instead of backstopping it.
const MOST_TILES: usize = 512;

impl Viewport {
    pub fn new(lat: f64, lon: f64, zoom: f64) -> Viewport {
        Viewport {
            lat: lat.clamp(-85.0, 85.0),
            lon: lon.clamp(-180.0, 180.0),
            zoom: zoom.clamp(MIN_ZOOM, MAX_ZOOM),
            detail: 0.0,
        }
    }

    /// Match the tiles to the pixels the screen actually has.
    pub fn for_screen(mut self, pixels_per_point: f32) -> Viewport {
        self.detail = (pixels_per_point.max(0.1) as f64).log2();
        self
    }

    /// The whole level whose tiles are drawn, and how much they are scaled by.
    ///
    /// Rounded rather than taken downwards. Flooring means the fractional part
    /// of the zoom is always made up by *stretching* the tiles — up to twice
    /// their size — so the map was permanently soft and the place names, which
    /// are text and have hard edges to lose, looked pixelated. Rounding puts
    /// the scale within √2 of life size in either direction, and shrinking a
    /// tile costs nothing to look at.
    pub fn level(&self) -> u32 {
        (self.zoom + self.detail)
            .round()
            .clamp(MIN_ZOOM, MAX_ZOOM + 1.0) as u32
    }

    pub fn scale(&self) -> f64 {
        self.scale_at(self.level())
    }

    /// The same, for a level other than the one the zoom asks for.
    ///
    /// Levels are chosen per *layer* rather than once for the whole map — see
    /// [`Self::level_within`] — so everything that places a tile has to be able
    /// to ask about the level that tile actually came from.
    pub fn scale_at(&self, level: u32) -> f64 {
        2f64.powf(self.zoom - level as f64)
    }

    /// The centre, as a pixel on the drawn level's grid.
    pub fn centre_px(&self) -> (f64, f64) {
        self.centre_px_at(self.level())
    }

    pub fn centre_px_at(&self, level: u32) -> (f64, f64) {
        (lon_to_px(self.lon, level), lat_to_px(self.lat, level))
    }

    /// Move by a distance in *screen* pixels.
    pub fn panned(self, dx: f64, dy: f64) -> Viewport {
        let level = self.level();
        let scale = self.scale();
        let (cx, cy) = self.centre_px();
        // Dragging right brings what is west into view, so the centre goes the
        // other way.
        let x = cx - dx / scale;
        let y = cy - dy / scale;
        Viewport::new(px_to_lat(y, level), px_to_lon(x, level), self.zoom)
    }

    /// Zoom about a point given as an offset in screen pixels from the centre,
    /// so whatever is under the pointer stays under it.
    pub fn zoomed_about(self, delta: f64, offset_x: f64, offset_y: f64) -> Viewport {
        let target = (self.zoom + delta).clamp(MIN_ZOOM, MAX_ZOOM);
        if (target - self.zoom).abs() < f64::EPSILON {
            return self;
        }

        // Done in pixels rather than in degrees. Correcting the centre by the
        // *coordinate* difference is the obvious way and is wrong: Mercator is
        // not linear in latitude, so moving the centre by a tenth of a degree
        // does not move the point under the pointer by a tenth of a degree, and
        // the view creeps north every time somebody zooms.
        let (lat, lon) = self.at_offset(offset_x, offset_y);
        let next = Viewport::new(self.lat, self.lon, target);
        let level = next.level();
        let scale = next.scale();

        // The anchor sits at a known pixel on the new level's grid; the centre
        // has to sit exactly the pointer's offset away from it.
        let cx = lon_to_px(lon, level) - offset_x / scale;
        let cy = lat_to_px(lat, level) - offset_y / scale;
        Viewport::new(px_to_lat(cy, level), px_to_lon(cx, level), target)
    }

    /// The coordinate under a screen offset from the centre.
    pub fn at_offset(&self, dx: f64, dy: f64) -> (f64, f64) {
        let level = self.level();
        let scale = self.scale();
        let (cx, cy) = self.centre_px();
        (
            px_to_lat(cy + dy / scale, level),
            px_to_lon(cx + dx / scale, level),
        )
    }

    /// Whether this is far enough from another to be worth offering a way back.
    ///
    /// A nudge of the wheel or a few pixels of drag is not "somewhere else";
    /// putting a Reset button on screen for one would be noise.
    pub fn differs_from(&self, other: &Viewport) -> bool {
        // A degree of latitude is a fixed distance; a degree of longitude is
        // not, so the tolerance follows the zoom rather than being absolute.
        let tolerance = 0.35 / 2f64.powf(self.zoom - 8.0);
        (self.zoom - other.zoom).abs() > 0.2
            || (self.lat - other.lat).abs() > tolerance
            || (self.lon - other.lon).abs() > tolerance
    }

    /// Where a coordinate falls on screen — the inverse of [`Self::at_offset`].
    ///
    /// Needed to put anything *on* the map rather than merely read it: a pin,
    /// a marker, anything that belongs to a place rather than to the window.
    pub fn screen_of(&self, rect: eframe::egui::Rect, lat: f64, lon: f64) -> eframe::egui::Pos2 {
        let level = self.level();
        let scale = self.scale();
        let (cx, cy) = self.centre_px();
        eframe::egui::pos2(
            (rect.center().x as f64 + (lon_to_px(lon, level) - cx) * scale) as f32,
            (rect.center().y as f64 + (lat_to_px(lat, level) - cy) * scale) as f32,
        )
    }

    /// The half-open grid range a viewport of this size covers at a level.
    ///
    /// Unclamped, and possibly outside the world at either edge: north and
    /// south there is nothing there and the row is skipped, while east and west
    /// the grid wraps. Which is which is the caller's business.
    fn tile_bounds(&self, level: u32, width: f32, height: f32) -> (i64, i64, i64, i64) {
        let scale = self.scale_at(level);
        let (cx, cy) = self.centre_px_at(level);
        let half_w = width as f64 / 2.0 / scale;
        let half_h = height as f64 / 2.0 / scale;
        let tile = TILE_PX as f64;
        (
            ((cx - half_w) / tile).floor() as i64,
            ((cx + half_w) / tile).ceil() as i64,
            ((cy - half_h) / tile).floor() as i64,
            ((cy + half_h) / tile).ceil() as i64,
        )
    }

    /// The most tiles covering the view can cost at a level.
    ///
    /// Counted rather than listed. This is asked once per level while stepping
    /// back to one that fits, and the answer at a level that does not fit is a
    /// number nobody wants a vector of.
    ///
    /// Taken from the size of the window alone, deliberately, rather than from
    /// where the view happens to sit — the worst alignment rather than this
    /// one. A count that moved with the centre would step the level on and off
    /// as the map was dragged across a tile boundary, and every step is a whole
    /// layer refetched at a different level. This is the difference between a
    /// level that follows the zoom and one that flickers under the hand.
    pub fn tile_count(&self, level: u32, width: f32, height: f32) -> usize {
        let tile = TILE_PX as f64 * self.scale_at(level);
        // Whatever the alignment, a run of n pixels touches at most one more
        // tile than it spans.
        let across = (width as f64 / tile).ceil() as usize + 1;
        let down = (height as f64 / tile).ceil() as usize + 1;
        // Never more than the world holds: a view wrapped past the date line
        // draws a tile twice, and the second one costs nothing to keep.
        let span = 1usize << level.min(usize::BITS - 1);
        across.min(span) * down.min(span)
    }

    /// The deepest level, at or below the one the zoom asks for, whose tiles
    /// cover the view within a budget.
    ///
    /// The zoom picks a level for *sharpness*; this is what stops a layer
    /// spending an unbounded number of requests on it. The two are not the same
    /// question, and they are furthest apart exactly where the trouble was: a
    /// view of the whole country on a large, dense screen wants several hundred
    /// tiles per layer, and the reflectivity is ten layers deep. Nothing could
    /// fetch that inside a 400ms step or hold it in the cache afterwards, so
    /// the loop re-downloaded itself every time round — which on screen is a
    /// sweep that arrives in pieces, flickers and goes.
    ///
    /// A level back is four times fewer tiles for a picture at half the
    /// resolution, and at that scale a mosaic cell is already smaller than a
    /// pixel, so there is nothing on screen to lose.
    pub fn level_within(&self, budget: usize, width: f32, height: f32) -> u32 {
        let floor = MIN_ZOOM as u32;
        let mut level = self.level();
        while level > floor && self.tile_count(level, width, height) > budget.max(1) {
            level -= 1;
        }
        level
    }

    /// Every tile touching a viewport of this size, in drawing order.
    pub fn visible_tiles(&self, width: f32, height: f32) -> Vec<(u32, u32)> {
        self.visible_tiles_at(self.level(), width, height)
    }

    /// The same, from a level the caller chose — normally one
    /// [`Self::level_within`] handed back.
    ///
    /// Still bounded hard, as a backstop rather than a policy: a caller that
    /// asks for a level it has not budgeted for should get a truncated map
    /// rather than a queue thousands long.
    pub fn visible_tiles_at(&self, level: u32, width: f32, height: f32) -> Vec<(u32, u32)> {
        let span = 1u32 << level;
        let (first_x, last_x, first_y, last_y) = self.tile_bounds(level, width, height);

        let mut out = Vec::new();
        for y in first_y..last_y {
            if y < 0 || y >= span as i64 {
                continue;
            }
            for x in first_x..last_x {
                // Wrapped, so a view across the date line still draws.
                let wrapped = x.rem_euclid(span as i64) as u32;
                out.push((wrapped, y as u32));
                if out.len() >= MOST_TILES {
                    return out;
                }
            }
        }
        out
    }

    /// Where a tile lands on screen, given the rectangle the map is drawn in.
    pub fn tile_rect(&self, rect: eframe::egui::Rect, x: u32, y: u32) -> eframe::egui::Rect {
        self.tile_rect_at(self.level(), rect, x, y)
    }

    /// The same, for a tile from a level other than the map's own.
    ///
    /// Everything here is anchored to the coordinate at the centre rather than
    /// to a grid, so a coarser layer lands exactly over a finer one with no
    /// registration to do.
    pub fn tile_rect_at(
        &self,
        level: u32,
        rect: eframe::egui::Rect,
        x: u32,
        y: u32,
    ) -> eframe::egui::Rect {
        let scale = self.scale_at(level);
        let (cx, cy) = self.centre_px_at(level);
        let size = TILE_PX as f64 * scale;
        let left = rect.center().x as f64 + (x as f64 * TILE_PX as f64 - cx) * scale;
        let top = rect.center().y as f64 + (y as f64 * TILE_PX as f64 - cy) * scale;
        eframe::egui::Rect::from_min_size(
            eframe::egui::pos2(left as f32, top as f32),
            eframe::egui::vec2(size as f32, size as f32),
        )
    }

    /// The span the view covers top to bottom, in kilometres — for saying how
    /// far out it is in words.
    pub fn span_km(&self, height: f32) -> f64 {
        let level = self.level();
        let scale = self.scale();
        let (_, cy) = self.centre_px();
        let half = height as f64 / 2.0 / scale;
        let north = px_to_lat(cy - half, level);
        let south = px_to_lat(cy + half, level);
        (north - south).abs() * 111.32
    }

    /// How far off life size the tiles are drawn: 1.0 is exact, above it they
    /// are being stretched and the map goes soft.
    ///
    /// Not used to draw anything — it exists so the property that matters can
    /// be asserted, which is how the flooring bug would have been caught.
    #[cfg(test)]
    pub fn stretch(&self) -> f64 {
        self.stretch_at(self.level())
    }

    /// The same, for the level a layer is actually drawn from.
    ///
    /// The plain [`Self::stretch`] asks about the level the *zoom* picks, which
    /// is why it went on passing while the map went soft: layers choose their
    /// own level against a budget now, and nothing was asserting what that did
    /// to the picture. On a 4K screen showing the country it was drawing the
    /// ground at 1.65× and the sweeps at 3.31× life size, both well outside the
    /// band this file has always claimed to hold.
    ///
    /// Test-only again. It was briefly what the reflectivity sized itself
    /// against, back when a sweep was a picture and had to be baked larger to
    /// keep up with a zoom. A traced sweep does not care how far it is
    /// magnified, so nothing outside the tests asks any more.
    #[cfg(test)]
    pub fn stretch_at(&self, level: u32) -> f64 {
        self.scale_at(level) * 2f64.powf(self.detail)
    }

    /// The zoom that covers this much ground in a window this tall — how a span
    /// in kilometres becomes a place on the zoom scale.
    ///
    /// Solved rather than stepped through the levels. Tiles only exist at whole
    /// levels, but the view is drawn at a fractional zoom and scales the
    /// nearest level to suit, so there is no reason to round: asking for 200km
    /// and being shown 341 because that is where the next level fell is a
    /// setting that does not mean what it says.
    ///
    /// At zoom z a pixel is `156543.03 · cos(lat) / 2^z` metres of ground,
    /// which rearranges to this.
    pub fn zoom_for_span(lat: f64, span_km: f64, height: f32) -> f64 {
        const EQUATOR_M_PER_PX: f64 = 156_543.033_928;
        let span_m = (span_km * 1000.0).max(1.0);
        let ratio = height.max(1.0) as f64 * EQUATOR_M_PER_PX * lat.to_radians().cos() / span_m;
        if ratio <= 0.0 {
            return MIN_ZOOM;
        }
        ratio.log2().clamp(MIN_ZOOM, MAX_ZOOM)
    }
}

/// A least-recently-used bound on how many tiles are kept.
///
/// Tiles live on the GPU once uploaded, so this is video memory rather than
/// heap — but it is still finite, and a long session panning around the country
/// at ten frames a sweep would otherwise accumulate every tile it ever saw.
pub struct Lru {
    seen: HashMap<TileId, Instant>,
    limit: usize,
}

/// How recently a tile has to have been drawn to count as still on screen.
///
/// A few frames' worth. The loop steps every 400ms and the sweep on screen is
/// touched each time it is drawn, so this is long enough to cover a frame that
/// ran slowly and short enough that a sweep the loop has moved past is a
/// candidate again almost immediately.
const IN_USE: Duration = Duration::from_millis(250);

impl Lru {
    pub fn new(limit: usize) -> Lru {
        Lru {
            seen: HashMap::new(),
            limit,
        }
    }

    /// Move the ceiling.
    ///
    /// It used to be fixed, because every tile was 256 pixels square and a
    /// count of tiles was a count of megabytes. A sweep is now sized to how far
    /// the view magnifies it — see `crate::weather::reflectivity::repaint` —
    /// so one deeply zoomed sweep tile is worth sixty-four of a wide one, and
    /// counting them is no longer counting anything. The caller knows the
    /// detail and works the ceiling back from it.
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
    }

    pub fn touch(&mut self, id: &TileId) {
        self.seen.insert(id.clone(), Instant::now());
    }

    /// Forget everything the caller no longer holds.
    ///
    /// Needed because tiles are also dropped from outside — changing the
    /// basemap throws the ground away — and an entry here for a tile that is
    /// gone is an entry [`Self::evict`] will nominate instead of a real one.
    /// Left unsynced, a session that changed style a few times would name only
    /// ghosts and the cache would grow without bound.
    pub fn retain(&mut self, kept: impl Fn(&TileId) -> bool) {
        self.seen.retain(|id, _| kept(id));
    }

    /// Which tiles to let go of, oldest first — never one that is on screen.
    ///
    /// The limit is a bound on what is *kept*, not a bound on what can be
    /// drawn. A large window on a dense screen can want more tiles in one frame
    /// than the limit allows, and evicting purely by age there would take the
    /// ground: layers are drawn bottom up, so the basemap is the oldest of the
    /// tiles touched this frame. It would be thrown away and fetched again
    /// every frame, forever. Better to sit over the limit for as long as the
    /// view genuinely needs to.
    pub fn evict(&mut self, held: usize) -> Vec<TileId> {
        if held <= self.limit {
            return Vec::new();
        }
        let mut by_age: Vec<(TileId, Instant)> = self
            .seen
            .iter()
            .filter(|(_, at)| at.elapsed() > IN_USE)
            .map(|(id, at)| (id.clone(), *at))
            .collect();
        by_age.sort_by_key(|(_, at)| *at);

        let drop: Vec<TileId> = by_age
            .into_iter()
            .take(held - self.limit)
            .map(|(id, _)| id)
            .collect();
        for id in &drop {
            self.seen.remove(id);
        }
        drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grid has to round-trip, or panning would drift every time the view
    /// moved.
    #[test]
    fn the_grid_round_trips() {
        for level in [3u32, 6, 9, 11] {
            for (lat, lon) in [(42.062, -72.626), (25.76, -80.19), (61.22, -149.90), (0.0, 0.0)] {
                let back_lat = px_to_lat(lat_to_px(lat, level), level);
                let back_lon = px_to_lon(lon_to_px(lon, level), level);
                assert!((back_lat - lat).abs() < 1e-6, "level {level}: {lat} -> {back_lat}");
                assert!((back_lon - lon).abs() < 1e-6, "level {level}: {lon} -> {back_lon}");
            }
        }
    }

    /// Level 0 is one tile holding the whole world, and each level doubles.
    #[test]
    fn a_level_doubles_the_one_below_it() {
        assert_eq!(world_px(0), 256.0);
        assert_eq!(world_px(1), 512.0);
        assert_eq!(world_px(10), 256.0 * 1024.0);

        // The prime meridian at the equator is the middle of the world.
        assert!((lon_to_px(0.0, 1) - 256.0).abs() < 1e-9);
        assert!((lat_to_px(0.0, 1) - 256.0).abs() < 1e-6);
    }

    /// A tile's box has to line up with its neighbours, or the map shows seams.
    #[test]
    fn tiles_tessellate() {
        let (_, _, right, _) = tile_bbox(6, 20, 24);
        let (left, _, _, _) = tile_bbox(6, 21, 24);
        assert!((right - left).abs() < 1e-6, "a vertical seam at level 6");

        let (_, bottom, _, _) = tile_bbox(6, 20, 24);
        let (_, _, _, top) = tile_bbox(6, 20, 25);
        assert!((bottom - top).abs() < 1e-6, "a horizontal seam at level 6");

        // And the whole world is covered at level 0.
        let (left, bottom, right, top) = tile_bbox(0, 0, 0);
        assert!((left + 20_037_508.34).abs() < 1.0);
        assert!((right - 20_037_508.34).abs() < 1.0);
        assert!((top - 20_037_508.34).abs() < 1.0);
        assert!((bottom + 20_037_508.34).abs() < 1.0);
    }

    /// Dragging moves the map the way a hand expects, and exactly as far as the
    /// hand moved.
    #[test]
    fn panning_follows_the_pointer() {
        let start = Viewport::new(42.062, -72.626, 8.0);

        let east = start.panned(-100.0, 0.0);
        assert!(east.lon > start.lon, "dragging left looks east");
        let north = start.panned(0.0, 100.0);
        assert!(north.lat > start.lat, "dragging down looks north");

        // There and back, exactly — this grid has no latitude-dependent scale
        // to lose precision to, unlike panning measured in kilometres.
        let returned = start.panned(137.0, -84.0).panned(-137.0, 84.0);
        assert!((returned.lat - start.lat).abs() < 1e-9, "{returned:?}");
        assert!((returned.lon - start.lon).abs() < 1e-9, "{returned:?}");
    }

    /// The whole point of zooming at the pointer: what is under it stays there.
    #[test]
    fn zooming_holds_the_point_under_the_pointer() {
        let start = Viewport::new(42.062, -72.626, 7.0);
        for (dx, dy) in [(120.0, -80.0), (-200.0, 150.0), (0.0, 0.0)] {
            let before = start.at_offset(dx, dy);
            let zoomed = start.zoomed_about(1.0, dx, dy);
            let after = zoomed.at_offset(dx, dy);
            assert!(
                (before.0 - after.0).abs() < 1e-6 && (before.1 - after.1).abs() < 1e-6,
                "at ({dx},{dy}): {before:?} drifted to {after:?}"
            );
        }
    }

    #[test]
    fn zoom_is_bounded() {
        let start = Viewport::new(42.0, -72.0, 8.0);
        let mut deep = start;
        for _ in 0..40 {
            deep = deep.zoomed_about(1.0, 0.0, 0.0);
        }
        assert!(deep.zoom <= MAX_ZOOM);
        let mut wide = start;
        for _ in 0..40 {
            wide = wide.zoomed_about(-1.0, 0.0, 0.0);
        }
        assert!(wide.zoom >= MIN_ZOOM);
    }

    /// A view asks for the tiles it can see and no more — and never for so many
    /// that the queue becomes the problem.
    #[test]
    fn only_the_visible_tiles_are_asked_for() {
        let view = Viewport::new(42.062, -72.626, 8.0);

        // A 1024x768 window at one tile per 256px is about 5x4 plus edges.
        let tiles = view.visible_tiles(1024.0, 768.0);
        assert!((12..=42).contains(&tiles.len()), "got {} tiles", tiles.len());

        // A bigger window wants more of them.
        assert!(view.visible_tiles(2048.0, 1536.0).len() > tiles.len());

        // And nothing can ask for the world.
        let world = Viewport::new(0.0, 0.0, MIN_ZOOM).visible_tiles(8000.0, 8000.0);
        assert!(world.len() <= 256, "got {}", world.len());
    }

    /// A layer with a budget is drawn from a level it can afford — which over a
    /// view of the whole country is not the level the zoom asks for.
    ///
    /// This is what the radar over the country came down to: the sweeps cost
    /// hundreds of tiles each at the zoom's own level, ten deep, and nothing
    /// could fetch or hold that — so the loop re-downloaded itself every time
    /// round and arrived in patches.
    #[test]
    fn a_wide_view_is_drawn_from_a_level_it_can_afford() {
        // The lower 48, on a large screen: about the widest thing anyone points
        // this at.
        let country = Viewport::new(39.8, -98.6, Viewport::zoom_for_span(39.8, 4400.0, 1440.0))
            .for_screen(2.0);
        let (w, h) = (2560.0f32, 1440.0f32);

        let asked = country.tile_count(country.level(), w, h);
        assert!(asked > 128, "the whole country wants {asked} tiles at its own level");
        assert!(
            asked >= country.visible_tiles_at(country.level(), w, h).len(),
            "the count has to bound what the walk actually lists"
        );

        // And the level a layer settles on does not move as the map is dragged,
        // or panning across a tile boundary would refetch it at another level.
        for (dx, dy) in [(0.0, 0.0), (137.0, -84.0), (-260.0, 200.0)] {
            let moved = country.panned(dx, dy);
            assert_eq!(
                moved.level_within(36, w, h),
                country.level_within(36, w, h),
                "dragging by ({dx},{dy}) changed the level"
            );
        }

        for budget in [36usize, 128] {
            let level = country.level_within(budget, w, h);
            assert!(level <= country.level(), "never finer than the zoom asked for");
            assert!(level >= MIN_ZOOM as u32, "and never off the bottom of the grid");
            assert!(
                country.tile_count(level, w, h) <= budget,
                "a budget of {budget} came back with level {level}, costing {}",
                country.tile_count(level, w, h)
            );
        }

        // An ordinary view of a county is nowhere near either budget, so
        // nothing about it changes.
        let county = Viewport::new(42.062, -72.626, 8.4).for_screen(1.0);
        assert_eq!(county.level_within(36, 1000.0, 700.0), county.level());
    }

    /// A layer drawn a level back has to land exactly over one drawn at the
    /// zoom's own level, or the rain sits somewhere the map does not.
    #[test]
    fn a_coarser_layer_lands_over_a_finer_one() {
        use eframe::egui::{pos2, vec2, Rect};
        let rect = Rect::from_min_size(pos2(20.0, 40.0), vec2(900.0, 640.0));
        let view = Viewport::new(42.062, -72.626, 8.0);

        let level = view.level();
        let (cx, cy) = view.centre_px_at(level);
        let (x, y) = ((cx / TILE_PX as f64) as u32, (cy / TILE_PX as f64) as u32);

        let fine = view.tile_rect_at(level, rect, x, y);
        let coarse = view.tile_rect_at(level - 1, rect, x / 2, y / 2);

        // The coarse tile is twice the size and covers the fine one exactly.
        assert!((coarse.width() - fine.width() * 2.0).abs() < 0.01);
        assert!(coarse.contains_rect(fine), "{coarse:?} should hold {fine:?}");

        // And the quarter of it the fine tile occupies is the one the grid says.
        let quarter = eframe::egui::Rect::from_min_size(
            coarse.min + vec2((x % 2) as f32, (y % 2) as f32) * coarse.size() / 2.0,
            coarse.size() / 2.0,
        );
        assert!((quarter.min - fine.min).length() < 0.01, "{quarter:?} vs {fine:?}");
    }

    /// Tiles are placed relative to the centre, so the centre of the view is
    /// the coordinate it was asked for.
    #[test]
    fn the_centre_of_the_map_is_where_it_is_pointed() {
        use eframe::egui::{pos2, Rect};
        let view = Viewport::new(42.062, -72.626, 8.0);
        let rect = Rect::from_min_size(pos2(0.0, 0.0), eframe::egui::vec2(800.0, 600.0));

        let (cx, cy) = view.centre_px();
        let (tx, ty) = ((cx / TILE_PX as f64) as u32, (cy / TILE_PX as f64) as u32);
        let tile = view.tile_rect(rect, tx, ty);

        // The centre coordinate falls inside the tile that contains it.
        assert!(tile.contains(rect.center()), "{tile:?} should hold the centre");
    }

    /// Zero at the centre, and what it reports should be about the ground the
    /// window actually covers.
    #[test]
    fn the_span_reported_matches_the_zoom() {
        let tall = Viewport::new(42.0, -72.0, 8.0).span_km(600.0);
        let tighter = Viewport::new(42.0, -72.0, 9.0).span_km(600.0);
        assert!(tighter < tall, "zooming in covers less ground");
        assert!((tall / tighter - 2.0).abs() < 0.05, "each level halves it");

        // A span asked for is the span shown, near enough to read off the
        // caption — not rounded up to wherever the next whole level fell.
        for span in [80.0f64, 200.0, 420.0, 800.0] {
            let zoom = Viewport::zoom_for_span(42.0, span, 600.0);
            let covered = Viewport::new(42.0, -72.0, zoom).span_km(600.0);
            assert!(
                (covered - span).abs() < span * 0.05,
                "asked for {span}km, got {covered:.0}km"
            );
        }
    }

    /// Reading a place off the screen and putting one back have to agree, or a
    /// pin sits somewhere other than where it points.
    #[test]
    fn screen_positions_round_trip_through_coordinates() {
        use eframe::egui::{pos2, vec2, Rect};
        let rect = Rect::from_min_size(pos2(30.0, 70.0), vec2(900.0, 640.0));
        let view = Viewport::new(42.062, -72.626, 8.0);

        for (dx, dy) in [(0.0, 0.0), (180.0, -120.0), (-260.0, 200.0)] {
            let (lat, lon) = view.at_offset(dx, dy);
            let back = view.screen_of(rect, lat, lon);
            let expected = rect.center() + vec2(dx as f32, dy as f32);
            assert!(
                (back - expected).length() < 0.01,
                "({dx},{dy}) came back at {back:?}, expected {expected:?}"
            );
        }

        // And the centre of the view lands in the centre of the rectangle.
        let centre = view.screen_of(rect, view.lat, view.lon);
        assert!((centre - rect.center()).length() < 0.01);
    }

    /// Tiles have to be drawn at about the size they were fetched at, or the
    /// map is soft — which is what flooring the level did, permanently.
    #[test]
    fn tiles_are_never_badly_stretched() {
        for zoom in [3.0f64, 5.4, 7.9, 8.1, 9.5, 11.0] {
            for ppp in [1.0f32, 1.25, 2.0] {
                let view = Viewport::new(42.0, -72.0, zoom).for_screen(ppp);
                let stretch = view.stretch();
                assert!(
                    (0.70..=1.42).contains(&stretch),
                    "zoom {zoom} at {ppp}x is drawn at {stretch:.2}x life size"
                );
            }
        }
    }

    /// A denser screen asks for finer tiles, which is the whole point of it.
    #[test]
    fn a_sharper_screen_asks_for_a_deeper_level() {
        let at = |ppp| Viewport::new(42.0, -72.0, 8.0).for_screen(ppp).level();
        assert_eq!(at(1.0), 8);
        assert_eq!(at(2.0), 9, "a 2x display wants one level deeper");
    }

    /// A nudge is not somewhere else, but a real move is.
    #[test]
    fn only_a_real_move_offers_the_way_back() {
        let home = Viewport::new(42.062, -72.626, 8.0);
        assert!(!home.differs_from(&home));
        assert!(!home.panned(3.0, 2.0).differs_from(&home));
        assert!(home.panned(400.0, 0.0).differs_from(&home));
        assert!(home.zoomed_about(1.0, 0.0, 0.0).differs_from(&home));
    }

    /// Push everything recorded back in time, so eviction sees tiles the view
    /// has moved on from rather than ones drawn this instant.
    fn age(lru: &mut Lru, by: Duration) {
        for at in lru.seen.values_mut() {
            *at -= by;
        }
    }

    /// The cache is bounded, and lets go of what has not been looked at.
    #[test]
    fn the_cache_evicts_the_oldest_first() {
        let mut lru = Lru::new(3);
        let id = |x| TileId { layer: Layer::Base, level: 8, x, y: 1 };

        for x in 0..5 {
            lru.touch(&id(x));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Under the limit, nothing goes.
        assert!(lru.evict(3).is_empty());

        age(&mut lru, IN_USE * 2);
        let dropped = lru.evict(5);
        assert_eq!(dropped.len(), 2);
        assert!(dropped.contains(&id(0)) && dropped.contains(&id(1)), "{dropped:?}");
    }

    /// A window big enough to want more tiles at once than the cache is allowed
    /// to keep must not have the ground taken out from under it.
    ///
    /// Layers are drawn bottom up, so the basemap is the oldest of the tiles
    /// touched in a frame — evicting on age alone would drop exactly the tiles
    /// being looked at, and fetch them again the next frame, forever.
    #[test]
    fn nothing_on_screen_is_evicted() {
        let mut lru = Lru::new(2);
        let ground = TileId { layer: Layer::Base, level: 8, x: 1, y: 1 };
        let sweep = TileId { layer: Layer::Radar(0), level: 8, x: 1, y: 1 };
        let names = TileId { layer: Layer::Labels, level: 8, x: 1, y: 1 };

        // One frame's worth, drawn bottom to top.
        for id in [&ground, &sweep, &names] {
            lru.touch(id);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            lru.evict(3).is_empty(),
            "three tiles in a cache of two, and all three are on screen"
        );
    }

    /// A tile dropped from outside has to be dropped from here too, or eviction
    /// nominates a ghost and the real cache never shrinks.
    #[test]
    fn the_cache_forgets_what_is_no_longer_held() {
        let mut lru = Lru::new(1);
        let base = TileId { layer: Layer::Base, level: 8, x: 1, y: 1 };
        let radar = TileId { layer: Layer::Radar(0), level: 8, x: 1, y: 1 };
        lru.touch(&radar);
        std::thread::sleep(std::time::Duration::from_millis(2));
        lru.touch(&base);

        // The sweeps were thrown away; only the ground is still held.
        lru.retain(|id| !matches!(id.layer, Layer::Radar(_)));
        age(&mut lru, IN_USE * 2);
        assert_eq!(lru.evict(2), vec![base], "the ghost must not stand in for it");
    }

    fn base(x: u32) -> TileId {
        TileId { layer: Layer::Base, level: 8, x, y: 1 }
    }
    fn sweep(frame: usize) -> TileId {
        TileId { layer: Layer::Radar(frame), level: 8, x: 1, y: 1 }
    }

    fn empty_queue() -> Queue {
        Queue {
            now: VecDeque::new(),
            soon: VecDeque::new(),
            queued: HashSet::new(),
            inflight: HashSet::new(),
            epochs: [0; 2],
            url_for: None,
            stop: false,
        }
    }

    /// The whole of the fix for a map that sat black: a tile on screen is never
    /// served behind one that is merely going to be wanted.
    #[test]
    fn what_is_on_screen_is_fetched_before_the_rest_of_the_loop() {
        let mut queue = empty_queue();

        // The speculative half of a frame's requests, queued first — which is
        // what happens the moment the view moves and the ground has to be
        // asked for again.
        for frame in 0..8 {
            queue.add(sweep(frame), false);
        }
        queue.add(base(0), true);
        queue.add(base(1), true);

        assert_eq!(queue.take(), Some(base(0)));
        assert_eq!(queue.take(), Some(base(1)));
        assert_eq!(queue.take(), Some(sweep(0)), "and then the loop, in order");
    }

    /// Asking twice for the same tile queues it once, however it is asked for.
    #[test]
    fn a_tile_is_only_queued_once() {
        let mut queue = empty_queue();
        assert!(queue.add(base(0), true));
        assert!(!queue.add(base(0), true), "already on screen's list");
        assert!(!queue.add(base(0), false), "and not worth a second entry");

        assert_eq!(queue.take(), Some(base(0)));
        assert_eq!(queue.take(), None);
        // Taken, so it can be asked for again — which is how a tile dropped
        // mid-flight gets picked back up.
        assert!(queue.add(base(0), true));
    }

    /// A sweep list refreshes every couple of minutes. The ground does not move
    /// when it does, and throwing away basemap tiles already on the wire is
    /// what made a refresh look like a stall.
    #[test]
    fn a_fresh_sweep_list_leaves_the_ground_alone() {
        let mut queue = empty_queue();
        queue.add(base(0), true);
        queue.add(sweep(3), false);

        queue.discard(|id| id.layer.group() == SWEEPS);
        assert_eq!(queue.take(), Some(base(0)));
        assert_eq!(queue.take(), None, "the sweep went and nothing else did");

        // A different basemap, on the other hand, invalidates everything.
        queue.add(base(0), true);
        queue.add(sweep(3), false);
        queue.discard(|_| true);
        assert_eq!(queue.take(), None);
    }

    /// A tile the service will never serve is asked for once; one that merely
    /// lost a race is asked for again.
    #[test]
    fn only_a_refusal_is_final() {
        let mut failed = Failures::default();

        failed.record(base(0), &TileError::Refused("no such tile".into()));
        assert!(failed.skip(&base(0)), "asking again is a wasted request");

        failed.record(base(1), &TileError::Blocked("connection reset".into()));
        assert!(failed.skip(&base(1)), "not immediately, though");

        // Once it has cooled off it is asked for again — and the record is
        // cleared by the asking, so the next attempt is a fresh one.
        failed.blocked.insert(base(1), (1, Instant::now() - RETRY_AFTER * 2));
        assert!(!failed.skip(&base(1)));
        assert!(!failed.blocked.contains_key(&base(1)));
    }

    /// But a host that is simply down must not be asked for every tile on
    /// screen every few seconds for as long as the map is open.
    #[test]
    fn a_tile_that_never_arrives_is_given_up_on() {
        let mut failed = Failures::default();
        for _ in 0..RETRIES {
            failed.record(base(0), &TileError::Blocked("timed out".into()));
            // Cool it off by hand rather than sleeping through the real wait.
            if let Some(attempt) = failed.blocked.get_mut(&base(0)) {
                attempt.1 = Instant::now() - RETRY_AFTER * 2;
            }
        }
        assert!(failed.refused.contains(&base(0)));
        assert!(failed.skip(&base(0)), "and it stays given up on");
    }

    /// Touching a tile again saves it from the next eviction.
    #[test]
    fn a_tile_still_in_view_is_kept() {
        let mut lru = Lru::new(2);
        let id = |x| TileId { layer: Layer::Base, level: 8, x, y: 1 };
        for x in 0..3 {
            lru.touch(&id(x));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // The view has moved on from all three...
        age(&mut lru, IN_USE * 2);
        lru.touch(&id(0)); // ...and then this one came back on screen.

        let dropped = lru.evict(3);
        assert_eq!(dropped, vec![id(1)], "the one nobody looked at again");
    }
}
