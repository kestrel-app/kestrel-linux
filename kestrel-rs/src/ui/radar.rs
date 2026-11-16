//! The radar, as a map rather than a picture.
//!
//! What this replaced was three fixed rasters — a basemap, a reflectivity layer
//! and the warning polygons — all requested for one bounding box and stacked.
//! That is a screenshot of a map: every pan or zoom threw a megabyte away and
//! asked for another, so moving meant waiting, and the picture could only ever
//! be the shape it had been requested at — which is why it letterboxed inside a
//! camera cell instead of filling it.
//!
//! This draws the same four layers off the tile grid in
//! [`crate::weather::tiles`]. Panning costs the tiles at the leading edge,
//! zooming reuses the level above until the new one lands, and the map fills
//! whatever rectangle it is given, because a grid has no aspect of its own.
//!
//! Everything on top of it — the scrubber, the legend, the layer switches, the
//! warning read-out — is drawn here rather than fetched, so the controls cost
//! nothing until somebody touches them.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, CornerRadius, Rect, Stroke, TextureHandle, Vec2};

use super::theme;
use crate::weather::poller::RadarInfo;
use crate::weather::tiles::{self, Layer, TileId, TileStore, Viewport};
use crate::weather::clock_text;
use crate::weather::reflectivity;
use crate::weather::warnings::{self, Hazard, Warning};

/// How long each sweep is on screen.
///
/// 400ms put twenty minutes across four seconds, which is about the pace the
/// service's own viewer runs at. This is deliberately slower than that: at
/// 700ms the ten sweeps take seven seconds, and with the two held steps at the
/// end the cycle is eight and a half. A squall line crossing in seven seconds
/// can be followed across a county rather than caught at.
///
/// Nothing about an individual frame changes with this. [`FADE`] is a fraction
/// of a step, so a longer step simply draws proportionally more frames of both
/// kinds and the share of them mid-crossing — the expensive ones, which draw
/// two sweeps — stays exactly where it was. The measurements on [`FADE`] were
/// taken at 400ms and carry over unchanged for that reason.
///
/// What does change is [`FADE`]'s ceiling. The settled part of a step is
/// `STEP * (1 - FADE)`, so a longer step buys back the crisp moment that
/// widening the crossing spent: 105ms here, three drawn frames, against the
/// not-quite-two that 400ms left.
const STEP: Duration = Duration::from_millis(700);
/// Held at the end, so the picture on screen longest is the current one.
///
/// Two rather than four. The last sweep has nothing to slide toward, so its own
/// step is already still — and every held step is still on top of that. At four
/// the loop glided for three and a half seconds and then stopped dead for two,
/// which is half the frames of a playing loop with no motion in them at all and
/// reads as the animation stalling rather than as dwelling on the present. Two
/// still gives the current weather three times the time any other sweep gets.
const HOLD_STEPS: u32 = 2;

/// The furthest the weather is ever assumed to travel between two sweeps, in
/// metres.
///
/// A storm doing 120 km/h — which is about as fast as one goes — covers four
/// kilometres in the two minutes between sweeps. Used to decide how much more
/// ground than the screen the reflectivity is fetched over, so that sliding it
/// does not walk a bare band in from the edge.
const FURTHEST_TRAVEL: f64 = 4_000.0;

/// How often a playing loop is redrawn.
///
/// Thirty a second rather than the sixty a screen could take. The mesh is
/// rebuilt every frame — egui has no transform on a mesh, so sliding the
/// weather means rewriting its vertices — and at a hundred thousand triangles
/// that is the radar's whole cost. Thirty is comfortably enough for something
/// crossing the screen over four seconds, and half the price of sixty.
const PLAYING: Duration = Duration::from_millis(33);
/// How much of a step is spent crossing from one sweep to the next.
///
/// Not the whole of it: a crossing that never ends is a loop with no crisp
/// sweep anywhere in it, and the point of the shapes is that a band edge is
/// exactly as sharp as the screen can draw. But three tenths was too little.
/// Frames arrive about every 37ms rather than the 33 asked for, so three tenths
/// of a step is three or four opacity steps — few enough to count, which is
/// what makes it read as stepping rather than crossing.
///
/// Two thirds was the next answer and read as a crossing rather than a step.
/// This is the one after it, asked for directly: 0.85 of a step spent
/// dissolving rather than 0.65, so the same opacity range is spread over half
/// again as long and the change is gentler still. At the current [`STEP`] that
/// is 595ms of every 700.
///
/// What that spends is the sharp moment, and it is the reason this cannot go
/// much higher. Every cycle has to keep one settled frame — the assertion in
/// `the_crossing_is_the_end_of_a_step_and_no_more` holds it to at least a whole
/// drawn frame — because a band edge at full opacity is the entire reason the
/// echo is traced into shapes rather than fetched as pictures. Raising this
/// again eats those frames; the way to a slower crossing from here is a longer
/// [`STEP`], which lengthens both parts together.
///
/// The cost was measured rather than assumed, because a crossing frame draws
/// *both* sweeps and widening the window puts more frames on that path. In a
/// release build at 3840x2160 over real echo across Indiana, crossing frames go
/// from 39% to 50% of a playing loop, and the frame time moves from median
/// 3.62ms / p90 7.77ms to median 4.28ms / p90 7.81ms. The budget is
/// [`PLAYING`], 33ms. The p90 does not move and there is four times the
/// headroom either way. Measured at a 400ms step, and independent of it: see
/// [`STEP`].
const FADE: f32 = 0.85;

/// A layer that does not travel: the ground and the lettering.
const NO_DRIFT: [f32; 2] = [0.0, 0.0];

/// How far the crossing from one sweep to the next has got, from nothing to
/// one.
///
/// Reads straight off [`RadarView::into`], which already means "how far this
/// step has travelled" and is already pinned to zero everywhere a crossing
/// would be wrong: paused, holding on the newest sweep, waiting for the loop to
/// arrive, and across the wrap back to the oldest. So the wrap stays the cut it
/// has always been without anything here having to know about it.
fn blend(into: f32) -> f32 {
    let along = ((into - (1.0 - FADE)) / FADE).clamp(0.0, 1.0);
    // Eased at both ends rather than run at a constant rate. A linear crossing
    // starts and stops at full speed, and those two corners are the moments the
    // eye picks out — the crossing appears to switch on rather than to begin.
    // Smoothstep leaves the middle as quick as it was and spends the ends
    // getting there.
    along * along * (3.0 - 2.0 * along)
}

/// What each sweep of a crossing is drawn at: the one leaving, then the one
/// arriving.
///
/// Not `1 - t` and `t`. Two part-transparent sweeps over the same ground do not
/// add up to an opaque one — the gap between them is `t(1 - t)`, a quarter of
/// the map showing through the middle of a storm that both sweeps agree is
/// solid. Measured on a real one, the echo lost 12% of its saturation and 16%
/// of its brightness at the midpoint, which is a dip a wall would show.
///
/// Taking the root of each closes most of it: the two are 0.71 rather than 0.5
/// half way across, so the gap falls to about 9%, and the ends are unchanged at
/// one and nothing. What it costs is that where the two sweeps *disagree* both
/// are more visible at once — which is the right way round, because the drift
/// has already put them over the same ground and what is left to disagree about
/// is small.
fn opacities(crossing: f32) -> (f32, f32) {
    let crossing = crossing.clamp(0.0, 1.0);
    ((1.0 - crossing).sqrt(), crossing.sqrt())
}

/// How often a still map is redrawn while tiles are still arriving. Nothing is
/// moving, so this only has to be often enough that a tile landing shows up
/// promptly.
const FETCHING: Duration = Duration::from_millis(80);

/// How many tiles to keep. Each is 256×256 on the GPU — a quarter of a megabyte
/// — so this is the ceiling of a long session spent panning around with the
/// whole loop loaded, not what an ordinary view costs.
///
/// Derived rather than picked, from the two things that actually claim it: the
/// ground at its budget, two layers deep because the dark canvas serves its
/// lettering separately, plus whatever the loop is allowed. Written as the sum
/// so the three numbers cannot drift apart — which they had, and the test
/// caught it.
///
/// It was three layers while the warning polygons were tiles as well. They are
/// geometry now and hold no textures at all, which gives the cache back a whole
/// layer's worth of room.
const TILE_LIMIT: usize = GROUND_TILES * 2 + LOOP_TILES;

/// The same ceiling, once a traced sweep's own size is taken into account.
///
/// Counting tiles was a fair proxy for counting memory while every tile was 256
/// pixels square. A traced sweep is not a texture at all: it is a few hundred
/// kilobytes of triangles against a plain tile's forty-odd, so the loop's share
/// is divided down to match. The ground keeps its share whatever happens — it
/// is one texture per position, and blur is what ruins it.
fn tile_limit() -> usize {
    /// What a traced sweep costs against a plain 256-pixel tile. The busiest
    /// tile measured against the live service came to 547KB where a tile of the
    /// ground is 256KB, and the budget wants the worst case.
    const AGAINST_A_TILE: usize = 4;
    GROUND_TILES * 2 + (LOOP_TILES / AGAINST_A_TILE).max(LEAST_FRAMES * 4)
}

/// How many tiles a layer may spend covering the view before it is drawn from a
/// level back instead. See [`Viewport::level_within`].
///
/// Two numbers because the layers are not the same size of thing. The ground
/// and its lettering are one picture each and are fetched once for a place; the
/// reflectivity is *ten* pictures, refreshed every couple of minutes. So a
/// sweep is allowed a quarter of what the ground is, and the two budgets
/// together are what keeps a frame's worth of tiles inside [`TILE_LIMIT`] at
/// any window size a screen can be.
///
/// This is the whole of the fix for the radar over a view of the country. At
/// that scale the zoom asked for several hundred tiles per layer, so one turn
/// of the loop was some thousands of requests to one government host and rather
/// more tiles than the cache would hold — it evicted the sweep it had just
/// fetched to make room for the next, and did it again every four seconds.
/// Nothing was wrong with any single tile; there were simply far too many, and
/// what that looks like is a radar that arrives in patches, flickers and
/// vanishes.
/// These were set to bound the cache and are now also set to bound the *blur*,
/// which is the half that was missed. A budget in tiles is a budget in
/// sharpness: a screen with four times the pixels wants four times the tiles to
/// draw the same ground, so an absolute number penalises exactly the display
/// that should benefit. At 128 and 36 a 4K screen showing the country drew its
/// ground at 1.65× life size and its sweeps at 3.31×, against a band this
/// codebase has claimed since the map was written to hold within 1.42×.
///
/// The ground gets much the larger share because it earns it: it is fetched
/// once for a place and reused by every sweep, and it is the layer carrying the
/// thing blur ruins outright — place names are text. A view of the whole
/// country on a 4K screen wants about 220 tiles, so that is what it is allowed.
///
/// The sweeps stay tighter because there are ten of them and because
/// reflectivity is the layer that suffers least: an echo is a soft-edged blob
/// and the mosaic under it is a kilometre a cell, so a level back costs it
/// little. What it must not do is fall further than that — see the floor in
/// [`RadarView::draw_tiles`].
const GROUND_TILES: usize = 288;

/// How many tiles the whole loop may hold, across every sweep of it.
///
/// The lever that replaced coarsening the sweeps. Reflectivity is not a
/// photograph: the mosaic is coloured in dBZ bands with hard edges between
/// them, and every one of those edges is a place an upscale shows. A level
/// back reads far worse here than the same level back on a map, which is why
/// two goes at tuning the sweeps' own budget did not fix it - a coarser sweep
/// was never going to look right, at any level.
///
/// So the sweeps are drawn from the ground's level, always, and what gives
/// instead is how many of them are held. A shorter loop of the real picture
/// beats twenty minutes of a smeared one. On an ordinary screen nothing gives
/// at all - the whole loop fits - and it is only a very large window over a
/// very wide view that trades any of it away.
const LOOP_TILES: usize = 1000;

/// The loop never falls below this, whatever the arithmetic says. Two sweeps is
/// the least that is still a loop rather than a still.
const LEAST_FRAMES: usize = 2;

/// How long the loop will go without a sweep tile arriving before it plays with
/// whatever it has.
///
/// Measured from the last tile in, not from the first asked for. As a deadline
/// from the start it was a guess at how long a whole loop takes to fetch, and a
/// wrong one: a wide view over a busy sky is hundreds of tiles, the guess ran
/// out in the middle of them, and the loop started anyway and stuttered through
/// the gaps — which is the thing it was there to prevent. Silence is the real
/// signal. A fetch that is still arriving is worth waiting for however long it
/// takes, and one where nothing has landed for this long is not coming.
const LOOP_STALL: Duration = Duration::from_secs(12);

/// The longest the loop will hold for itself however well the fetch is going.
///
/// Only a backstop. Waiting on silence is the rule and it is the right one, but
/// it assumes arrivals eventually stop — and a view whose loop does not fit the
/// cache would evict tiles as fast as it fetches them, keep landing them for
/// ever, and hold the picture still for just as long. The loop is sized against
/// the cache precisely so that cannot happen; this is what makes the failure a
/// stuttering loop rather than a frozen one if it ever does.
const LOOP_PATIENCE: Duration = Duration::from_secs(120);

/// How often the polygons are asked for again for a view that has not moved. A
/// warning issued while somebody is watching one place is exactly the one worth
/// having.
const KEY_REFRESH: Duration = Duration::from_secs(120);
/// How much wider than the view the polygons are fetched for.
///
/// A tile map covers a drag by fetching the few tiles at the leading edge;
/// geometry has no leading edge to fetch, so it is asked for with a margin
/// instead. Half a screen in every direction covers an ordinary drag outright,
/// and it is nearly free — the tolerance widens with the box, so a bigger ask
/// is a coarser one rather than a heavier one.
const FETCH_MARGIN: f64 = 2.0;
/// How thick a polygon's border is drawn, in points.
///
/// The whole reason the shapes are worth having. This used to be whatever the
/// service had rendered, recovered by edge-detecting the fill and then
/// resampled to the tile grid, so it thickened and thinned with the zoom. A
/// stroke is a stroke at every scale.
const BORDER: f32 = 1.3;
/// The least the key will ever list before it starts counting instead. How many
/// it *does* list follows the room it has - see [`key_rows`].
const LEAST_KEY_ROWS: usize = 3;
/// A row of the key, in points.
const KEY_ROW: f32 = 17.0;
/// The room the basemap credit takes at the bottom right, backing included.
///
/// The credit has the lowest place on that side because its terms ask for it to
/// be legible; everything else stacks above it. Written down rather than left
/// implicit, because the two are placed by different functions and the key
/// landed straight on top of it the first time.
const CREDIT_ROOM: f32 = 26.0;
/// The colour patch beside each name.
const SWATCH: f32 = 11.0;



/// Where the bottom-right furniture stops, measured from the map's own bottom.
///
/// The scrubber owns the strip below this, and the credit the band above that.
fn furniture_top(rect: Rect) -> f32 {
    rect.bottom() - 48.0 - CREDIT_ROOM
}

/// How many hazards the key lists, given the room between the switches at the
/// top and the credit at the bottom.
///
/// Scaled rather than fixed: a wall on a 4K screen has room for every advisory
/// in force and no reason to hide seventeen of them behind "and 17 more", while
/// a small window has room for three and should say so honestly.
fn key_rows(available: f32, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    // The heading and the panel's own padding come out first; what is left
    // divides into rows, and one row is kept back for the "and N more" line in
    // case there is one.
    let fits = ((available - 30.0) / KEY_ROW).floor().max(0.0) as usize;
    if fits >= total {
        total
    } else {
        fits.saturating_sub(1).max(LEAST_KEY_ROWS).min(total)
    }
}

/// How many of the newest sweeps the cache can hold, given what one costs.
///
/// Split out because it is the whole of the trade and the numbers are easy to
/// get the wrong way round: it is the *loop* that shortens, never the picture
/// that softens.
fn playable_frames(frames: usize, per_sweep: usize) -> usize {
    if per_sweep == 0 || frames == 0 {
        return frames;
    }
    (LOOP_TILES / per_sweep).clamp(LEAST_FRAMES.min(frames), frames)
}

/// The map from Web Mercator metres to the screen, and back.
///
/// The polygons are held in metres because that is the projection the map is
/// drawn in, which makes putting one on screen an affine transform rather than
/// a projection: a multiply and an add per vertex, with the trigonometry done
/// once when the fetch landed and never again. A pan changes two numbers here
/// and nothing at all in the geometry.
///
/// Deliberately the same arithmetic as [`Viewport::screen_of`], which does the
/// same job one coordinate at a time for the pin. The scale is written out of
/// the level and the zoom together because their product does not depend on
/// which level was picked — `world_px(level) * scale_at(level)` is
/// `TILE_PX * 2^zoom` whatever the level is — so this cannot drift out of step
/// with the tiles under it.
#[derive(Clone, Copy)]
struct Ground {
    /// Screen points per metre.
    scale: f64,
    x0: f64,
    y0: f64,
}

impl Ground {
    fn of(rect: Rect, view: Viewport) -> Ground {
        let level = view.level();
        let zoom = view.scale_at(level);
        let (cx, cy) = view.centre_px_at(level);
        let world = tiles::world_px(level);
        Ground {
            scale: world / (2.0 * tiles::EDGE) * zoom,
            x0: rect.center().x as f64 + (world / 2.0 - cx) * zoom,
            y0: rect.center().y as f64 + (world / 2.0 - cy) * zoom,
        }
    }

    fn at(&self, point: [f64; 2]) -> egui::Pos2 {
        egui::pos2(
            (self.x0 + point[0] * self.scale) as f32,
            (self.y0 - point[1] * self.scale) as f32,
        )
    }

    /// Where a point on screen falls on the ground. North is up, so the y axis
    /// runs the other way from the screen's.
    fn metres_at(&self, at: egui::Pos2) -> (f64, f64) {
        (
            (at.x as f64 - self.x0) / self.scale,
            (self.y0 - at.y as f64) / self.scale,
        )
    }
}

/// Add a triangle list to a mesh, transformed onto the screen and coloured.
fn mesh_of(mesh: &mut egui::Mesh, fill: &[[f64; 2]], ground: &Ground, colour: Color32) {
    for corner in fill.chunks_exact(3) {
        let base = mesh.vertices.len() as u32;
        for point in corner {
            mesh.colored_vertex(ground.at(*point), colour);
        }
        mesh.add_triangle(base, base + 1, base + 2);
    }
}

/// Which layers are drawn.
///
/// The order they are drawn in is the whole of the compositing: ground, then
/// weather, then warnings over the weather, then place names over everything —
/// a town name is most worth reading when there is a storm sitting on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Showing {
    pub radar: bool,
    pub warnings: bool,
    pub labels: bool,
}

impl Default for Showing {
    fn default() -> Self {
        Showing {
            radar: true,
            warnings: true,
            labels: true,
        }
    }
}

/// What the map asked the app to do, if anything.
///
/// A bool was enough while the only question was "open the full radar", and
/// stopped being enough the moment the full radar wanted a way back out.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Asked {
    #[default]
    Nothing,
    /// Fill the pane with the radar - a double-click on the wall's own cell.
    OpenFull,
    /// Put the cameras back.
    ShowCameras,
}

/// A point that was clicked, and what the map says covers it.
///
/// `found` is filled the moment the click lands — the polygons are on this side
/// now, so the answer is a point-in-polygon test rather than a round trip. What
/// is still coming is only the *wording*, which lives on the alert itself, so
/// the box appears with the hazard named and fills in its headline afterwards.
struct Detail {
    at: egui::Pos2,
    found: Vec<Warning>,
}

pub struct RadarView {
    /// Uploaded tiles, keyed by the same id the pool fetches against — so a
    /// tile that scrolls back into view is already here.
    tiles: HashMap<TileId, TextureHandle>,
    /// Tiles that arrived with nothing painted on them.
    ///
    /// A reflectivity tile over a clear sky is fully transparent, which on
    /// screen is indistinguishable from one that failed to arrive — and "the
    /// radar is not showing" is what that looks like to somebody watching. The
    /// service says which, so the picture can too.
    blank: HashSet<TileId>,
    /// Sweeps that arrived as traced bands rather than as pictures, keyed the
    /// same way. A tile is in one map or the other, never both — see
    /// [`sharpness_for`].
    shapes: HashMap<TileId, reflectivity::Shapes>,
    lru: tiles::Lru,
    store: TileStore,
    /// What the pool is currently pointed at, so it is only re-pointed when the
    /// region, the basemap or the sweep list actually changes.
    sources: Option<crate::weather::radar::Sources>,

    frame: usize,
    /// How far the loop has travelled from [`Self::frame`] toward the next
    /// sweep, from nothing to one.
    ///
    /// The loop used to be a slideshow — ten sweeps two minutes apart, shown
    /// 400ms apart — so the weather did not travel across the map, it teleported
    /// ten times in four seconds. At the thirteen-mile view that jump is about
    /// ninety pixels. This is what fills the gap in: the sweep on screen is slid
    /// toward where the next one says it went, which is a change to where it is
    /// drawn and nothing else, because it is geometry.
    ///
    /// Zero whenever there is nothing to slide toward: while the loop is holding
    /// at the newest sweep, while it is waiting for tiles, and across the wrap
    /// from the last sweep back to the first — nine sweeps backwards is not a
    /// movement any displacement can express, so that stays a cut.
    into: f32,
    /// Which way each sweep went, keyed on the sweep and the level it was
    /// drawn from. One vector a sweep, not one a tile — see
    /// [`RadarView::settle_drift`].
    drift: HashMap<(usize, u32), [f32; 2]>,
    /// How many of the newest sweeps are being played, which on a wide view of
    /// a large screen is fewer than the service offers. See [`playable_frames`].
    playable: usize,
    /// Which sweeps have every tile they need, indexed like the sweep list.
    ready: Vec<bool>,
    /// When a tile of this sweep list last arrived, so a fetch that has stopped
    /// getting anywhere does not hold the picture still for ever — and one that
    /// is still arriving is waited for however long it takes.
    fetching_since: Instant,
    /// When this sweep list started filling, for [`LOOP_PATIENCE`].
    filling_since: Instant,
    playing: bool,
    hold: u32,
    stepped_at: Instant,

    pub showing: Showing,
    /// Whether the sweep on screen has any echo on it at all.
    echo: bool,
    detail: Option<Detail>,
    /// The watches, warnings and advisories covering the view, as geometry.
    ///
    /// One fetch answers three questions that used to be three requests: what
    /// to draw, what to name in the key, and what is under a click.
    hazards: Vec<Hazard>,
    /// What the key lists — the distinct hazards above, and what each is drawn
    /// in. Derived rather than fetched.
    key: Vec<(String, Color32)>,
    /// The view the polygons were fetched for, so they are only fetched again
    /// when the map has actually gone somewhere else.
    key_view: Option<Viewport>,
    key_asked: Option<Instant>,
    key_pending: Option<Arc<Mutex<Option<Vec<Hazard>>>>>,
    /// The wording of a clicked hazard, fetched on a thread of its own. The
    /// hazard itself is already named on screen by the time this is asked for.
    identify: Option<Arc<Mutex<Option<Vec<Warning>>>>>,
}

impl Default for RadarView {
    fn default() -> Self {
        RadarView {
            tiles: HashMap::new(),
            blank: HashSet::new(),
            shapes: HashMap::new(),
            lru: tiles::Lru::new(TILE_LIMIT),
            store: TileStore::new(),
            sources: None,
            frame: 0,
            into: 0.0,
            drift: HashMap::new(),
            playable: crate::weather::radar::FRAMES,
            ready: Vec::new(),
            fetching_since: Instant::now(),
            filling_since: Instant::now(),
            playing: true,
            hold: 0,
            stepped_at: Instant::now(),
            showing: Showing::default(),
            echo: true,
            detail: None,
            hazards: Vec::new(),
            key: Vec::new(),
            key_view: None,
            key_asked: None,
            key_pending: None,
            identify: None,
        }
    }
}

impl RadarView {
    /// Let go of every tile. The textures are the largest thing this holds, and
    /// a wall left on Live all night should not carry a map nobody is watching.
    pub fn release(&mut self) {
        self.tiles.clear();
        self.blank.clear();
        self.shapes.clear();
        self.drift.clear();
        self.lru = tiles::Lru::new(TILE_LIMIT);
        self.sources = None;
        self.detail = None;
        self.hazards.clear();
        self.key.clear();
        self.key_view = None;
    }

    /// Point the pool at the services this reading describes.
    fn aim(&mut self, info: &RadarInfo, style: &str) {
        let wanted = crate::weather::radar::Sources {
            region: info.region.clone(),
            style: style.to_string(),
            times: info.times.clone(),
        };
        if self.sources.as_ref() == Some(&wanted) {
            return;
        }

        // A new sweep list does not invalidate the ground: basemap tiles are
        // keyed by position, not by time. Only what actually moved is dropped —
        // and the same distinction is passed to the pool, so a refresh every
        // couple of minutes stops throwing away the basemap tiles it has in the
        // air. That is what made a refresh look like a stall.
        let ground_changed = self
            .sources
            .as_ref()
            .is_some_and(|old| old.style != wanted.style || old.region != wanted.region);
        let invalidates = if ground_changed || self.sources.is_none() {
            self.tiles.clear();
            self.shapes.clear();
            self.drift.clear();
            tiles::Invalidates::Everything
        } else {
            self.tiles.retain(|id, _| !matches!(id.layer, Layer::Radar(_)));
            self.shapes.clear();
            self.drift.clear();
            tiles::Invalidates::OnlyTheSweeps
        };
        // The cache tracks what is held; it has just been told less is.
        let held: HashSet<TileId> = self.tiles.keys().cloned().collect();
        self.lru.retain(|id| held.contains(id));
        self.blank.retain(|id| held.contains(id));

        let sources = wanted.clone();
        self.store
            .set_source(Arc::new(move |id: &TileId| sources.url_for(id)), invalidates);
        self.sources = Some(wanted);
        // A fresh list means a fresh newest sweep, and that is the one to show.
        self.frame = info.times.len().saturating_sub(1);
        self.ready = vec![false; info.times.len()];
        self.fetching_since = Instant::now();
        self.filling_since = Instant::now();
    }

    /// Take up whatever the pool has finished.
    fn take_up(&mut self, ctx: &egui::Context) {
        for decoded in self.store.collect() {
            self.lru.touch(&decoded.id);
            if decoded.opaque {
                self.blank.remove(&decoded.id);
            } else {
                self.blank.insert(decoded.id.clone());
            }
            // Something landed, so the fetch is not stalled. Only the sweeps
            // count: the basemap and the lettering arrive on their own schedule
            // and say nothing about whether the loop is filling.
            if matches!(decoded.id.layer, Layer::Radar(_)) {
                self.fetching_since = Instant::now();
            }
            // A sweep arrives as one or the other, never both.
            match decoded.shapes {
                Some(shapes) => {
                    self.shapes.insert(decoded.id, shapes);
                }
                None => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [decoded.width as usize, decoded.height as usize],
                        &decoded.rgba,
                    );
                    let texture = ctx.load_texture("tile", image, egui::TextureOptions::LINEAR);
                    self.tiles.insert(decoded.id, texture);
                }
            }
        }
        for id in self.lru.evict(self.tiles.len() + self.shapes.len()) {
            self.tiles.remove(&id);
            self.shapes.remove(&id);
            self.blank.remove(&id);
        }
    }

    /// Whether any sweep the loop would play is still missing tiles.
    fn loop_waiting(&self, count: usize) -> bool {
        (self.first_frame(count)..count).any(|frame| !self.ready.get(frame).copied().unwrap_or(false))
    }

    /// The oldest sweep being played. Everything before it is a sweep the
    /// service has that this view cannot afford to hold at full sharpness.
    fn first_frame(&self, count: usize) -> usize {
        count.saturating_sub(self.playable.max(1))
    }

    fn step(&mut self, count: usize) {
        let first = self.first_frame(count);
        // A sweep list that shrank, or a window that grew, can leave the loop
        // pointing behind its own start.
        if self.frame < first {
            self.frame = first;
        }
        // Nothing to slide toward until there is a reason to; every early
        // return below is a case where the loop is not travelling.
        self.into = 0.0;
        if !self.playing || count - first < 2 {
            return;
        }
        // Sit on the newest sweep until the rest of the loop has arrived.
        //
        // Stepping into a sweep whose tiles are still coming is what "half the
        // map is missing for half a minute" actually is: the loop moves every
        // 400ms, so within four seconds every frame has been asked for at once
        // and the one being looked at is queued behind nine that are not. Held
        // here, only the newest sweep is urgent, it completes in a second or
        // two, and the loop starts when it can run rather than stuttering
        // through gaps on the way.
        //
        // Bounded by silence rather than by a stopwatch: a tile the service
        // will not serve never arrives, and a loop that waited for it would
        // never play at all. Anything still landing keeps this open.
        if self.loop_waiting(count)
            && self.fetching_since.elapsed() < LOOP_STALL
            && self.filling_since.elapsed() < LOOP_PATIENCE
        {
            self.frame = count - 1;
            self.hold = 0;
            return;
        }
        // Between one sweep and the next, slide. Not while holding at the
        // newest, and not on the last sweep of the loop, whose next step is a
        // wrap of nine sweeps backwards.
        let along = self.stepped_at.elapsed().as_secs_f32() / STEP.as_secs_f32();
        if along < 1.0 {
            if self.hold == 0 && self.frame + 1 < count {
                self.into = along;
            }
            return;
        }

        self.stepped_at = Instant::now();
        if self.frame >= count - 1 && self.hold < HOLD_STEPS {
            self.hold += 1;
            return;
        }
        self.hold = 0;
        self.frame = if self.frame + 1 >= count { first } else { self.frame + 1 };
    }

    /// Draw the map into `rect`, filling it.
    ///
    /// `view` is where it is looking, updated in place — the caller owns it,
    /// because the wall cell and the Weather tab show the same place at
    /// different sizes and must not disagree about where that is.
    ///
    /// `compact` is a cell on the camera wall: the same map, without the
    /// furniture there is no room for.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        info: &RadarInfo,
        style: &str,
        twenty_four: bool,
        metric: bool,
        view: &mut Viewport,
        home: Viewport,
        compact: bool,
    ) -> Asked {
        // Tiles are chosen for the pixels the screen actually has, not for the
        // points egui lays out in. Without this a 2x display stretches every
        // 256-pixel tile across 512 real ones.
        *view = view.for_screen(ui.ctx().pixels_per_point());

        // Settled before anything is aimed or drawn, because all of it depends
        // on this: the sweeps are drawn from the ground's level, how many of
        // them are held is what pays for that, and how sharply each is
        // recoloured follows how far it is being magnified.
        let ground_level = view.level_within(GROUND_TILES, rect.width(), rect.height());
        let sweep_level = ground_level.min(crate::weather::radar::MOSAIC_LEVEL);

        let per_sweep = view.tile_count(sweep_level, rect.width(), rect.height());
        self.aim(info, style);
        // A traced sweep is dearer than a plain tile, so what the cache may
        // hold comes down to match.
        self.lru.set_limit(tile_limit());
        self.take_up(ui.ctx());

        self.playable = playable_frames(info.times.len(), per_sweep);
        self.step(info.times.len());

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, theme::INK);

        // Before anything is drawn, because the polygons are drawn from it and
        // a wall cell shows them too — only the key it has no room for is held
        // back to the full radar.
        self.refresh_hazards(rect, *view, ui.ctx().pixels_per_point());

        self.draw_tiles(&painter, rect, *view, info, ground_level);
        // Over the map and under the furniture: it belongs to the ground, but
        // it must not sit on top of the scrubber or the read-out.
        self.home_pin(&painter, rect, *view, home, compact);

        // The map claims the pointer *first*, and the controls are registered
        // after it. egui hands a click to whichever widget was registered last
        // at that position, so this order is what makes the switches and the
        // scrubber clickable at all — with the map last, it covered them and
        // took every click meant for them. Nothing needs to check whether the
        // pointer is over a control, because egui already has.
        let asked = self.interact(ui, rect, view, home, compact);

        if compact {
            self.compact_caption(&painter, rect, info, twenty_four);
        } else {
            self.caption(&painter, rect, info, *view, metric);
            self.legend(&painter, rect);
            self.draw_key(&painter, rect);
            self.switches(ui, rect);
            self.reset(ui, rect, view, home);
            if self.home_button(ui, rect) {
                return Asked::ShowCameras;
            }
            self.scrubber(ui, rect, info, twenty_four);
            self.read_out(ui, rect);
        }

        if self.nothing_yet(info) {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                if info.error.is_empty() {
                    "Fetching the radar…"
                } else {
                    &info.error
                },
                egui::FontId::proportional(13.0),
                theme::PLACEHOLDER,
            );
        }

        // Tiles still coming, or a loop still playing, means another frame.
        //
        // A playing loop wants far more of them than it used to. It was 80ms
        // when a step was a jump and the picture only changed ten times in four
        // seconds; the weather travels continuously now, and travel drawn at
        // twelve frames a second is not travel. This is the one part of moving
        // the weather that is not free: the mesh is rebuilt every frame, so the
        // rate is what the radar costs.
        if std::env::var("KESTREL_PROBE").is_ok() {
            eprintln!(
                "PROBE frame={} ready={:?} waiting={} pending={}",
                self.frame,
                self.ready,
                self.loop_waiting(info.times.len()),
                self.store.pending()
            );
        }
        if self.playing && info.times.len() > 1 {
            ui.ctx().request_repaint_after(PLAYING);
        } else if self.store.pending() > 0 {
            ui.ctx().request_repaint_after(FETCHING);
        }
        asked
    }

    fn nothing_yet(&self, info: &RadarInfo) -> bool {
        !info.ok && self.tiles.is_empty()
    }

    // ------------------------------------------------------------ the map

    fn draw_tiles(
        &mut self,
        painter: &egui::Painter,
        rect: Rect,
        view: Viewport,
        info: &RadarInfo,
        ground_level: u32,
    ) {
        // One level for everything. The sweeps used to take a coarser one to
        // pay for there being ten of them, and that is what made the
        // reflectivity look stretched: it is a colour-mapped picture with hard
        // edges between the dBZ bands, so an upscale shows on every one of
        // them in a way it does not on a map. What gives instead is how many
        // sweeps are held - see `playable_frames`.
        let (width, height) = (rect.width(), rect.height());
        let ground = view.visible_tiles_at(ground_level, width, height);
        // Never past the level the mosaic's own resolution runs out at. Asking
        // for a finer one does not get finer weather, it gets the same
        // kilometre cells resampled nearest-neighbour into hard squares - see
        // `MOSAIC_LEVEL`. Fetching where the data is and letting the GPU do the
        // scaling gives the smooth picture for a sixteenth of the tiles.
        let sweep_level = ground_level.min(crate::weather::radar::MOSAIC_LEVEL);
        // A little more ground than is on screen, because the sweep travels.
        //
        // Sliding the whole sweep toward where the next one says it went walks
        // the trailing edge of it in from the side of the screen, and behind
        // that edge there is no tile — a band of bare map that fills back in
        // every time the loop steps. The band is as wide as the weather
        // travels, which at the closest views is a couple of hundred pixels, so
        // it is not subtle.
        //
        // Sized to the most a storm covers between sweeps rather than to the
        // vector actually found, which is not known until the tiles it is
        // measured from have arrived. Four kilometres is the ceiling; at a wide
        // view that is a few pixels and adds nothing, and at a close one it is
        // a fraction of a tile that has few tiles in it to begin with.
        let reach = (FURTHEST_TRAVEL * Ground::of(rect, view).scale) as f32;
        let sweep_owned = view.visible_tiles_at(
            sweep_level,
            width + reach * 2.0,
            height + reach * 2.0,
        );
        let sweep = &sweep_owned;
        let frames = info.times.len();

        // Bottom to top. The weather is above the warning polygons, because the
        // weather is what the radar is for: a summer afternoon of heat
        // advisories drawn over the top is a seventh of the country with the
        // rain painted out of it. The polygons are held back to a wash as well
        // - see `warnings::WATCH_WASH` - so they read as ground the weather is
        // happening over rather than as something happening instead of it.
        //
        // The cost is that a warning under a storm is covered by the storm,
        // which is the one place it matters most. The key names it, and a click
        // still identifies it, so it is said rather than shown there.
        //
        // Place names stay above the lot. They are thin text with a dark halo,
        // they hide nothing, and a town name is most worth reading when there
        // is a storm sitting on it.
        let clip = painter.with_clip_rect(rect);
        let mut radar_seen = false;
        let mut radar_painted = false;

        for &(x, y) in &ground {
            self.place_tile(
                &clip,
                rect,
                view,
                Layer::Base,
                ground_level,
                x,
                y,
                Color32::WHITE,
                NO_DRIFT,
            );
        }

        // Not tiles at all. Between the ground and the weather because that is
        // where they were when they were tiles, and for the same reason. The
        // ground is handed to it as well: washing a polygon is drawing the
        // basemap back over it.
        if self.showing.warnings {
            self.draw_hazards(&clip, rect, view, ground_level, &ground);
        }

        if self.showing.radar {
            let frame = if frames == 0 { 0 } else { self.frame.min(frames - 1) };
            let sky = self.settle_drift(sweep, sweep_level, frame, frames);
            // One offset for the whole sweep, so it travels as one thing.
            let drift = [sky[0] * self.into, sky[1] * self.into];

            // The two sweeps of a crossing, and where each of them sits.
            //
            // The one arriving is drawn behind where it will be by exactly as
            // far as it still has to travel, so it reaches its own ground as
            // the step lands. Both sweeps are therefore over the same ground
            // the whole way across, and what happens between them is opacity
            // and nothing else — a crossing with a jump underneath it would be
            // worse than the cut it replaced.
            let crossing = blend(self.into);
            // Whether to cross at all is decided once, for the whole sweep.
            //
            // It used to be decided per tile — cross where the arriving tile
            // had arrived, hold where it had not — and that is the same mistake
            // as a motion vector per tile. Half the sweep mid-dissolve against
            // the other half at full opacity puts a seam down every boundary
            // between the two, which is tearing by another route. Past
            // LOOP_STALL the loop plays with gaps in it, and the honest answer
            // to a sweep that is only partly here is to keep showing the one
            // that is all here.
            let arriving = (crossing > 0.0 && frame + 1 < frames)
                .then(|| {
                    let whole = sweep.iter().all(|&(x, y)| {
                        let here = TileId { layer: Layer::Radar(frame), level: sweep_level, x, y };
                        let next = TileId {
                            layer: Layer::Radar(frame + 1),
                            level: sweep_level,
                            x,
                            y,
                        };
                        let held = |id: &TileId| {
                            self.tiles.contains_key(id) || self.shapes.contains_key(id)
                        };
                        // Only tiles the sweep on screen actually has are asked
                        // about: ground neither sweep covers is not a gap.
                        !held(&here) || held(&next)
                    });
                    let behind = self.into - 1.0;
                    whole.then(|| (frame + 1, [sky[0] * behind, sky[1] * behind]))
                })
                .flatten();
            let (leaving, arriving_at) = match arriving {
                Some(_) => opacities(crossing),
                None => (1.0, 0.0),
            };

            // One mesh for the whole sweep rather than one per tile. They are
            // all the same untextured mesh, so merging them costs nothing and
            // saves a couple of dozen allocations and as many shapes handed to
            // the painter on every frame of a playing loop.
            let mut leaving_mesh = egui::Mesh::default();
            let mut arriving_mesh = egui::Mesh::default();

            for &(x, y) in sweep {
                let id = TileId {
                    layer: Layer::Radar(frame),
                    level: sweep_level,
                    x,
                    y,
                };
                let held = self.tiles.contains_key(&id) || self.shapes.contains_key(&id);
                if held {
                    radar_seen = true;
                    radar_painted |= !self.blank.contains(&id);
                }

                let next = arriving.map(|(next, behind)| {
                    (
                        TileId { layer: Layer::Radar(next), level: sweep_level, x, y },
                        behind,
                    )
                });
                for (at, (id, drift, alpha)) in [
                    Some((id, drift, leaving)),
                    next.map(|(id, behind)| (id, behind, arriving_at)),
                ]
                .into_iter()
                .enumerate()
                .filter_map(|(at, one)| one.map(|one| (at, one)))
                {
                    if alpha <= 0.0 {
                        continue;
                    }
                    if self.shapes.contains_key(&id) {
                        let mesh =
                            if at == 0 { &mut leaving_mesh } else { &mut arriving_mesh };
                        self.place_shapes(rect, view, &id, sweep_level, x, y, drift, alpha, mesh);
                    } else {
                        self.place_tile(
                            &clip,
                            rect,
                            view,
                            id.layer,
                            sweep_level,
                            x,
                            y,
                            Color32::WHITE.gamma_multiply(alpha),
                            drift,
                        );
                    }
                }
            }

            // The one leaving under the one arriving, which is the order they
            // cross in.
            for mesh in [leaving_mesh, arriving_mesh] {
                if !mesh.is_empty() {
                    clip.add(egui::Shape::mesh(mesh));
                }
            }
        }

        if self.showing.labels {
            for &(x, y) in &ground {
                self.place_tile(
                    &clip,
                    rect,
                    view,
                    Layer::Labels,
                    ground_level,
                    x,
                    y,
                    Color32::WHITE,
                    NO_DRIFT,
                );
            }
        }

        // A sweep that has arrived everywhere and is blank everywhere is a
        // clear sky, and worth saying so.
        self.echo = !radar_seen || radar_painted;

        // The rest of the loop, so pressing play does not stall. Requested
        // after the visible frame, because that is the one being waited on.
        //
        // Bounded by what the cache can actually hold. A large window on a
        // dense screen sees four times the tiles, and ten frames of those would
        // be more than the cache keeps — so it would evict the tiles it just
        // fetched to make room for the rest of the same loop, and do it again
        // every frame. Past the budget the loop is filled in around the sweep
        // being shown instead, which is where a scrub or a play is going next.
        if self.showing.radar && frames > 1 {
            // Only the sweeps the loop actually plays. What is older than that
            // is not merely unaffordable to keep, it is never shown - fetching
            // it would be a request thrown away and a tile evicting one that
            // is about to be drawn.
            let first = self.first_frame(frames);
            if self.ready.len() != frames {
                self.ready = vec![false; frames];
            }
            for frame in first..frames {
                // A sweep is ready when every tile it needs is either held or
                // known to be empty sky. Recomputed rather than remembered:
                // the set of tiles a sweep needs changes with every pan.
                let mut complete = true;
                for &(x, y) in sweep {
                    let id = TileId {
                        layer: Layer::Radar(frame),
                        level: sweep_level,
                        x,
                        y,
                    };
                    if !self.tiles.contains_key(&id) && !self.shapes.contains_key(&id) {
                        complete = false;
                        // Behind everything on screen. The sweep being shown is
                        // asked for above; these are for where the loop is
                        // going next, and a basemap tile must never wait on one.
                        self.store.prefetch(id);
                    }
                }
                self.ready[frame] = complete;
            }
        }
    }

    /// Put one tile on screen, asking for it if it is not held.
    ///
    /// Split out when the warning polygons stopped being tiles: the draw used
    /// to be one loop over a list of layers, and with only the ground, the
    /// weather and the lettering left in it, naming the three in order reads
    /// better than a vector does.
    fn place_tile(
        &mut self,
        clip: &egui::Painter,
        rect: Rect,
        view: Viewport,
        layer: Layer,
        level: u32,
        x: u32,
        y: u32,
        tint: Color32,
        drift: [f32; 2],
    ) {
        let id = TileId { layer, level, x, y };
        let mut target = view.tile_rect_at(level, rect, x, y);
        // A sweep travels as one thing, and this is a tile of one when the ramp
        // was not recognised and it stayed a picture. Left standing still it
        // would sit motionless while the shapes either side of it slid past,
        // which tears the weather along that tile's edges — the exact fault one
        // vector for the whole sweep was meant to end. The ground and the
        // lettering are handed no drift and do not move.
        if drift != [0.0, 0.0] {
            target = target.translate(Vec2::new(
                drift[0] * target.width(),
                drift[1] * target.height(),
            ));
        }
        match self.tiles.get(&id) {
            Some(texture) => {
                self.lru.touch(&id);
                clip.image(
                    texture.id(),
                    target,
                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    tint,
                );
            }
            None => {
                self.store.request(id);
                // While it is coming, the level above is stretched over the
                // gap: the wrong resolution and exactly the right picture,
                // which is what stops a zoom flashing black.
                if layer == Layer::Base {
                    self.draw_parent(clip, target, x, y, level, tint);
                }
            }
        }
    }

    /// Which way the whole sweep went, worked out once and kept.
    ///
    /// One vector for all of it, and that is not a simplification — it is the
    /// only thing that can be right. Each tile's shapes are clipped to its own
    /// ground when they are traced, so a storm lying across a tile boundary is
    /// two pieces that stay joined only while both move by the same amount.
    /// Giving each tile its own vector pulls every such storm apart and leaves a
    /// seam of bare map down each boundary, which is exactly what it did.
    ///
    /// Kept, rather than worked out afresh each frame, for the same kind of
    /// reason. The estimate is a median over whichever tiles happen to be
    /// loaded, and that set changes as tiles arrive and are evicted — so
    /// recomputing every frame made the whole sky twitch between one answer and
    /// the next. It depends on the two sweeps and on nothing a pan or a zoom
    /// changes, so it is computed once for a sweep and left alone.
    ///
    /// Most tiles are empty sky and empty sky matches itself equally well at
    /// every offset, so [`reflectivity::motion`] declines on them; the median is
    /// taken over the ones that could say. Until enough of them have, the sweep
    /// does not move at all — a guess from one tile is worse than standing
    /// still.
    fn settle_drift(
        &mut self,
        visible: &[(u32, u32)],
        level: u32,
        frame: usize,
        frames: usize,
    ) -> [f32; 2] {
        /// How many tiles have to agree before the sky is allowed to move.
        const ENOUGH: usize = 3;

        if frame + 1 >= frames {
            return [0.0, 0.0];
        }
        if let Some(known) = self.drift.get(&(frame, level)) {
            return *known;
        }

        let mut found: Vec<[f32; 2]> = Vec::new();
        for &(x, y) in visible {
            let from = TileId { layer: Layer::Radar(frame), level, x, y };
            let to = TileId { layer: Layer::Radar(frame + 1), level, x, y };
            let (Some(one), Some(other)) = (self.shapes.get(&from), self.shapes.get(&to)) else {
                continue;
            };
            if let Some(moved) = reflectivity::motion(one, other) {
                found.push(moved);
            }
        }
        if found.len() < ENOUGH {
            // Not settled yet: stand still, and ask again next frame rather
            // than remembering that there was not enough to go on.
            return [0.0, 0.0];
        }

        let middle = |axis: usize| {
            let mut all: Vec<f32> = found.iter().map(|moved| moved[axis]).collect();
            all.sort_by(f32::total_cmp);
            all[all.len() / 2]
        };
        let sky = [middle(0), middle(1)];
        self.drift.insert((frame, level), sky);
        sky
    }

    /// Put one tile of traced reflectivity on screen.
    ///
    /// The shapes are in the tile's own coordinates, 0 to 1 across and down, so
    /// this is the tile's screen rectangle and nothing else — no projection and
    /// no origin to carry. A band edge therefore lands wherever the rectangle
    /// puts it, exactly, at any zoom: that is the whole point of it being
    /// geometry rather than a picture.
    ///
    /// One mesh for the tile, with the bands in the order they were traced —
    /// least intense first, so the heaviest rain is painted last and is not
    /// rubbed out by the lighter band it sits inside. Tiles do not overlap
    /// (they are clipped to their own ground when traced), so the order between
    /// them does not matter.
    fn place_shapes(
        &mut self,
        rect: Rect,
        view: Viewport,
        id: &TileId,
        level: u32,
        x: u32,
        y: u32,
        drift: [f32; 2],
        alpha: f32,
        mesh: &mut egui::Mesh,
    ) {
        let Some(shapes) = self.shapes.get(id) else { return };
        self.lru.touch(id);
        let target = view.tile_rect_at(level, rect, x, y);
        if !target.intersects(rect) {
            return;
        }

        // Room for what is about to go in, taken in one go. The mesh is rebuilt
        // every frame — egui has no transform on a mesh — so growing it a
        // handful of vertices at a time is a reallocation and a copy several
        // times per tile per frame, which at thirty frames a second across two
        // dozen tiles is the cheapest thing here to stop doing.
        let corners: usize = shapes.bands.iter().map(|p| p.triangles.len()).sum();
        mesh.vertices.reserve(corners);
        mesh.indices.reserve(corners);

        for patch in &shapes.bands {
            let Some([r, g, b]) = reflectivity::BANDS.get(patch.band).map(|(_, rgb)| *rgb) else {
                continue;
            };
            // The bands are rings rather than nested shapes, so one opacity
            // over the whole sweep is honest: no part of it is painted twice.
            let colour = Color32::from_rgb(r, g, b).gamma_multiply(alpha);
            for corner in patch.triangles.chunks_exact(3) {
                let base = mesh.vertices.len() as u32;
                for point in corner {
                    let (across, down) = reflectivity::Patch::at(*point);
                    // Slid toward where the next sweep says this went. The
                    // shapes are in the tile's own coordinates and so is the
                    // drift, so travelling is an addition — which is the whole
                    // reason to hold the weather as geometry.
                    mesh.colored_vertex(
                        egui::pos2(
                            target.left() + (across + drift[0]) * target.width(),
                            target.top() + (down + drift[1]) * target.height(),
                        ),
                        colour,
                    );
                }
                mesh.add_triangle(base, base + 1, base + 2);
            }
        }
    }

    /// Draw the watch and warning polygons.
    ///
    /// Three passes, and the middle one is the whole trick.
    ///
    /// The polygons overlap, heavily. A point in Mississippi on a summer
    /// afternoon is under a heat advisory, an extreme heat warning, an air
    /// quality alert and a severe thunderstorm watch at once. Drawn as four
    /// washes over each other that comes out very nearly opaque, in a
    /// magenta-brown that appears in nobody's legend — the map goes, the
    /// weather goes, and the key stops describing what is on screen. The
    /// service never had this problem because it rendered its whole layer to
    /// one picture in priority order first, so a pixel had one colour, and only
    /// then was that picture washed.
    ///
    /// The same result, without the picture: the standing hazards are filled
    /// *opaque*, in the order [`warnings::fetch`] hands them back, which is
    /// least serious first — so the most serious one covering any ground is the
    /// one left on it. Then the basemap is drawn over the lot again at the
    /// complementary alpha, which composites to exactly a wash:
    ///
    /// ```text
    ///   α·base + (1-α)·hazard   with   α = 255 - WATCH_WASH
    /// ```
    ///
    /// and where nothing is warned about it is the basemap over itself, which
    /// is the basemap. It costs one more pass over tiles already on the GPU.
    ///
    /// Working the overlaps out geometrically was the other way, and the
    /// arithmetic does not survive it: resolving them needs one sweep across
    /// every polygon at once, and that slices each of them at every other's
    /// vertex heights. An ordinary 226-mile view went from 2,700 triangles to
    /// 139,000, and a 4K view of the country to 2.2 million.
    ///
    /// The short-fuse warnings are washed the ordinary way afterwards, because
    /// they can be: five kinds covering well under a percent of the map, which
    /// do not stack in practice.
    ///
    /// The transform is affine and that is the point of keeping the geometry in
    /// Mercator metres: the map is drawn in that projection, so a vertex
    /// reaches the screen through a multiply and an add, with no trigonometry
    /// and nothing to recompute when the view moves. A pan is free.
    fn draw_hazards(
        &mut self,
        clip: &egui::Painter,
        rect: Rect,
        view: Viewport,
        ground_level: u32,
        visible: &[(u32, u32)],
    ) {
        if self.hazards.is_empty() {
            return;
        }
        let ground = Ground::of(rect, view);
        // Everything outside the view, rejected by its own bounding box before
        // a single vertex is transformed. Most of a fetch is off screen, since
        // it is asked for with a margin.
        let (west, north) = ground.metres_at(rect.left_top());
        let (east, south) = ground.metres_at(rect.right_bottom());
        let showing = |patch: &warnings::Patch| {
            let (left, bottom, right, top) = patch.bounds;
            right >= west && left <= east && top >= south && bottom <= north
        };

        // The standing hazards, opaque, least serious first.
        let mut standing = egui::Mesh::default();
        for hazard in self.hazards.iter().filter(|hazard| !hazard.short_fuse) {
            let Some((r, g, b)) = crate::weather::hazards::colour_of(&hazard.event) else {
                continue;
            };
            let solid = Color32::from_rgb(r, g, b);
            for patch in hazard.patches.iter().filter(|patch| showing(patch)) {
                mesh_of(&mut standing, &patch.fill, &ground, solid);
            }
        }
        if !standing.is_empty() {
            clip.add(egui::Shape::mesh(standing));
            // And the ground back over them, which is what turns solid colour
            // into a wash.
            for &(x, y) in visible {
                self.place_tile(
                    clip,
                    rect,
                    view,
                    Layer::Base,
                    ground_level,
                    x,
                    y,
                    Color32::from_white_alpha(255 - warnings::WATCH_WASH),
                    NO_DRIFT,
                );
            }
        }

        // The short-fuse warnings, washed the ordinary way.
        let mut urgent = egui::Mesh::default();
        for hazard in self.hazards.iter().filter(|hazard| hazard.short_fuse) {
            let Some((r, g, b)) = crate::weather::hazards::colour_of(&hazard.event) else {
                continue;
            };
            let wash = Color32::from_rgba_unmultiplied(r, g, b, warnings::WARNING_WASH);
            for patch in hazard.patches.iter().filter(|patch| showing(patch)) {
                mesh_of(&mut urgent, &patch.fill, &ground, wash);
            }
        }
        if !urgent.is_empty() {
            clip.add(egui::Shape::mesh(urgent));
        }

        // The borders last, over every fill rather than each over its own. It
        // is the border that says where a warning *is* — the one part of the
        // polygon with nothing behind it to read — so having them all on top is
        // the picture that reads best, as well as the one that batches.
        for hazard in &self.hazards {
            let Some((r, g, b)) = crate::weather::hazards::colour_of(&hazard.event) else {
                continue;
            };
            let border = Stroke::new(BORDER, Color32::from_rgb(r, g, b));
            for patch in hazard.patches.iter().filter(|patch| showing(patch)) {
                // Every ring of it — a hole has an edge as much as the outside
                // does, and it is the edge that says where the warning stops.
                for ring in &patch.rings {
                    if ring.len() < 3 {
                        continue;
                    }
                    let mut path: Vec<egui::Pos2> = ring.iter().map(|p| ground.at(*p)).collect();
                    path.push(path[0]);
                    clip.add(egui::Shape::line(path, border));
                }
            }
        }
    }

    /// Mark where the ZIP code landed.    /// Mark where the ZIP code landed.    /// Mark where the ZIP code landed.
    ///
    /// The map can be dragged anywhere, and a mosaic of rain over a grey map
    /// gives nothing away about which part of it is yours — the centre is only
    /// the centre until the first drag. So the place the radar was pointed at
    /// gets a pin, and keeps it wherever the view goes.
    ///
    /// Drawn with a dark outline under the copper, because it has to be legible
    /// over a bright green squall and a dark canvas alike, and a marker that
    /// disappears into heavy rain is a marker that fails exactly when the map
    /// is being read hardest.
    fn home_pin(&self, painter: &egui::Painter, rect: Rect, view: Viewport, home: Viewport, compact: bool) {
        let at = view.screen_of(rect, home.lat, home.lon);
        // Off screen: the Reset view button is what says so, rather than an
        // arrow pinned to the edge pointing at somewhere you cannot see.
        if !rect.expand(4.0).contains(at) {
            return;
        }

        let height = if compact { 16.0 } else { 22.0 };
        let radius = height * 0.34;
        let head = egui::pos2(at.x, at.y - height + radius);

        // The outline first, as a slightly larger copy of the same shape.
        for (grow, colour) in [(1.6f32, theme::INK), (0.0, theme::ACCENT)] {
            painter.circle_filled(head, radius + grow, colour);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(at.x - radius * 0.86 - grow, head.y + radius * 0.5),
                    egui::pos2(at.x + radius * 0.86 + grow, head.y + radius * 0.5),
                    egui::pos2(at.x, at.y + grow),
                ],
                colour,
                Stroke::NONE,
            ));
        }
        // Punched through, so it reads as a pin rather than a copper blob.
        painter.circle_filled(head, radius * 0.38, theme::INK);
    }

    /// Stretch the parent tile over a hole, if it happens to be held.
    ///
    /// Tinted like the tile it stands in for, so the wash pass covers a gap in
    /// the basemap with the same strength as the rest of it.
    fn draw_parent(
        &self,
        painter: &egui::Painter,
        target: Rect,
        x: u32,
        y: u32,
        level: u32,
        tint: Color32,
    ) {
        if level == 0 {
            return;
        }
        let parent = TileId {
            layer: Layer::Base,
            level: level - 1,
            x: x / 2,
            y: y / 2,
        };
        let Some(texture) = self.tiles.get(&parent) else {
            return;
        };
        // Which quarter of the parent this tile is.
        let (u, v) = ((x % 2) as f32 * 0.5, (y % 2) as f32 * 0.5);
        painter.image(
            texture.id(),
            target,
            Rect::from_min_max(egui::pos2(u, v), egui::pos2(u + 0.5, v + 0.5)),
            tint,
        );
    }

    // ------------------------------------------------------------ input

    fn interact(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        view: &mut Viewport,
        _home: Viewport,
        compact: bool,
    ) -> Asked {
        let response = ui.interact(
            rect,
            ui.id().with("radar-map"),
            egui::Sense::click_and_drag(),
        );

        if response.dragged() {
            // Applied straight away rather than deferred to the mouse coming
            // up. A tile map either has the tiles for where it is going or
            // wants only the few at the leading edge, so there is nothing worth
            // waiting for.
            let delta = response.drag_delta();
            *view = view.panned(delta.x as f64, delta.y as f64);
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if response.drag_stopped() {
            // Whatever is still queued is for where the view used to be.
            self.store.forget_pending();
        }

        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let at = response
                    .hover_pos()
                    .map(|p| p - rect.center())
                    .unwrap_or(Vec2::ZERO);
                *view = view.zoomed_about(scroll as f64 * 0.004, at.x as f64, at.y as f64);
                self.store.forget_pending();
            }
        }

        if response.double_clicked() {
            // The same gesture both ways, and it means the same thing both
            // times: show me the other thing. On the wall's cell that is the
            // full radar; on the full radar it is the cameras. It used to reset
            // the view here, which the Reset button already does and says.
            return if compact { Asked::OpenFull } else { Asked::ShowCameras };
        }
        // A click asks what the warnings say here. Not a pause: on a map,
        // clicking means "what is that", and the loop has a button of its own.
        if response.clicked() && !compact {
            if let Some(at) = response.interact_pointer_pos() {
                self.ask_what_is_here(at, rect, *view);
            }
        }
        Asked::Nothing
    }

    /// Say what covers a point.
    ///
    /// This used to be a request. The polygons arrived as pixels, so the
    /// picture could not say what it was showing — only the service could, and
    /// it had to be handed the map's extent so it could scale a tolerance for
    /// how close counted as *on*. It was also asked speculatively: a click on
    /// empty ground could only be recognised by finding that the warning tile
    /// under it had arrived with nothing painted on it, which is a heuristic
    /// standing in for a fact.
    ///
    /// The geometry is on this side now, so the question is a point-in-polygon
    /// test: exact at the edge, instant, and free when the answer is nothing.
    /// What still has to be fetched is the *wording*, which lives on the alert
    /// rather than on the shape — so the box appears immediately with the
    /// hazard named and when it runs out, and fills in the headline after.
    fn ask_what_is_here(&mut self, at: egui::Pos2, rect: Rect, view: Viewport) {
        // Any click puts down whatever was being shown. Without this, a click
        // meant to dismiss the read-out simply asked about somewhere else and
        // the box moved instead of going away.
        self.detail = None;
        self.identify = None;

        if !self.showing.warnings {
            return;
        }
        let (x, y) = Ground::of(rect, view).metres_at(at);
        let found = warnings::at_point(&self.hazards, x, y);
        if found.is_empty() {
            return;
        }

        self.detail = Some(Detail {
            at,
            found: found.iter().map(|hazard| hazard.locally()).collect(),
        });

        let answer = Arc::new(Mutex::new(None));
        let handle = Arc::clone(&answer);
        if std::thread::Builder::new()
            .name("radar-wording".into())
            .spawn(move || {
                let said = warnings::wording(&found);
                *handle.lock().unwrap() = Some(said);
            })
            .is_ok()
        {
            self.identify = Some(answer);
        }
    }

    // ------------------------------------------------------------ furniture

    /// The place, how far out it is, and the credit the basemap's terms ask for.
    fn caption(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        info: &RadarInfo,
        view: Viewport,
        metric: bool,
    ) {
        let top = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 44.0));
        painter.rect_filled(top, CornerRadius::ZERO, Color32::from_black_alpha(150));

        let place = if info.place.is_empty() {
            "Radar".to_string()
        } else {
            info.place.clone()
        };
        painter.text(
            egui::pos2(rect.left() + 16.0, top.center().y),
            egui::Align2::LEFT_CENTER,
            place,
            egui::FontId::proportional(15.0),
            theme::TEXT,
        );
        let mut right = format!(
            "{} across",
            crate::weather::span_text(view.span_km(rect.height()), metric)
        );
        if self.showing.radar && !self.echo {
            right = format!("nothing showing  ·  {right}");
        }
        painter.text(
            egui::pos2(rect.right() - 16.0, top.center().y),
            egui::Align2::RIGHT_CENTER,
            right,
            egui::FontId::proportional(12.0),
            theme::TEXT_DIM,
        );
        // Backed, because it has to be readable over whatever the map is
        // showing and it is a term of using the basemap at all — an attribution
        // that disappears into a green squall is not an attribution.
        let credit = painter.layout_no_wrap(
            crate::weather::radar::canvas_credit().to_owned(),
            egui::FontId::proportional(10.0),
            theme::TEXT_DIM,
        );
        let at = egui::pos2(
            rect.right() - 10.0 - credit.size().x,
            rect.bottom() - 48.0 - credit.size().y,
        );
        painter.rect_filled(
            Rect::from_min_size(at, credit.size()).expand2(Vec2::new(6.0, 3.0)),
            CornerRadius::same(4),
            Color32::from_black_alpha(170),
        );
        painter.galley(at, credit, theme::TEXT_DIM);
    }

    /// The same, for a cell on the camera wall.
    ///
    /// One line, sized to the cell and measured before it is drawn. What this
    /// replaces put the place on the left and the time on the right at fixed
    /// sizes, which in a wall cell ran one straight through the other.
    fn compact_caption(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        info: &RadarInfo,
        twenty_four: bool,
    ) {
        let size = (rect.height() * 0.055).clamp(9.0, 13.0);
        let font = egui::FontId::proportional(size);

        let when = info
            .clocks
            .get(self.frame)
            .copied()
            .flatten()
            .map(|clock| clock_text(clock, twenty_four))
            .unwrap_or_default();
        let place = if info.place.is_empty() {
            "Radar".to_string()
        } else {
            info.place.clone()
        };

        // Measured, then dropped a piece at a time until it fits. The place
        // matters more than the time, and both matter more than a collision.
        let room = rect.width() - 20.0;
        let fits = |text: &str| {
            painter
                .layout_no_wrap(text.to_owned(), font.clone(), theme::TEXT)
                .size()
                .x
                <= room
        };
        let when = if self.showing.radar && !self.echo {
            if when.is_empty() {
                "nothing showing".to_string()
            } else {
                format!("{when}  ·  clear")
            }
        } else {
            when
        };
        let both = format!("{place}  ·  {when}");
        let text = if !when.is_empty() && fits(&both) {
            both
        } else if fits(&place) {
            place
        } else {
            when
        };
        if text.is_empty() {
            return;
        }

        let galley = painter.layout_no_wrap(text, font, theme::TEXT);
        let strip = Rect::from_min_size(rect.min, Vec2::new(rect.width(), galley.size().y + 10.0));
        painter.rect_filled(strip, CornerRadius::ZERO, Color32::from_black_alpha(150));
        painter.galley(
            egui::pos2(rect.left() + 10.0, strip.center().y - galley.size().y / 2.0),
            galley,
            theme::TEXT,
        );
    }

    /// Fetch the watch and warning geometry for this view.
    ///
    /// Only when the map has actually gone somewhere else, and never twice at
    /// once. This is one request answering everything the polygons are for —
    /// the shapes, the key that names them, and what a click will find — so it
    /// is worth paying for a pan and not worth paying for a frame.
    ///
    /// The simplification tolerance is the view's own metres-per-pixel, which
    /// is what keeps the cost flat across the zoom range: a wide view has more
    /// hazards in it and asks for each of them coarser, in the same proportion.
    /// At full fidelity the whole country is 29MB; at a pixel's tolerance it is
    /// under a megabyte, and no screen can tell the two apart.
    fn refresh_hazards(&mut self, rect: Rect, view: Viewport, pixels_per_point: f32) {
        // Take the answer first, so a request that has landed is used before
        // the next one is considered.
        if let Some(hazards) = self
            .key_pending
            .as_ref()
            .and_then(|pending| pending.lock().unwrap().take())
        {
            self.key = warnings::kinds(&hazards)
                .into_iter()
                .filter_map(|hazard| {
                    let (r, g, b) = crate::weather::hazards::colour_of(&hazard)?;
                    Some((hazard, Color32::from_rgb(r, g, b)))
                })
                .collect();
            self.hazards = hazards;
            self.key_pending = None;
        }
        if !self.showing.warnings {
            self.hazards.clear();
            self.key.clear();
            self.key_view = None;
            return;
        }
        if self.key_pending.is_some() {
            return;
        }

        let moved = self
            .key_view
            .map(|had| view.differs_from(&had))
            .unwrap_or(true);
        // Re-asked on a timer as well as on a move: a warning issued while
        // somebody is watching one place is exactly the one worth naming.
        let stale = self
            .key_asked
            .map(|at| at.elapsed() > KEY_REFRESH)
            .unwrap_or(true);
        if !moved && !stale {
            return;
        }

        // Asked for rather more ground than is on screen, so a drag does not
        // walk off the edge of the geometry before the next fetch lands. The
        // polygons are cheap enough that the margin costs less than the gap
        // would show.
        let (half_w, half_h) = (
            rect.width() as f64 / 2.0 * FETCH_MARGIN,
            rect.height() as f64 / 2.0 * FETCH_MARGIN,
        );
        let (north, west) = view.at_offset(-half_w, -half_h);
        let (south, east) = view.at_offset(half_w, half_h);
        let bbox = (
            crate::weather::radar::mercator_x(west),
            crate::weather::radar::mercator_y(south),
            crate::weather::radar::mercator_x(east),
            crate::weather::radar::mercator_y(north),
        );
        // One screen pixel of ground, which is the finest geometry that can
        // make any difference to what is drawn.
        //
        // A *pixel*, not a point. The map is laid out in points and drawn in
        // whatever the display actually has, which on a hidpi screen is two of
        // the latter per one of the former — and a 4K wall is exactly where
        // asking for geometry twice as coarse as the screen can draw would
        // show. The tiles are already chosen this way; see
        // [`Viewport::for_screen`].
        let across = rect.width() as f64 * FETCH_MARGIN * pixels_per_point.max(0.1) as f64;
        let tolerance = (bbox.2 - bbox.0) / across.max(1.0);

        self.key_view = Some(view);
        self.key_asked = Some(Instant::now());
        let answer = Arc::new(Mutex::new(None));
        let handle = Arc::clone(&answer);
        if std::thread::Builder::new()
            .name("radar-warnings".into())
            .spawn(move || {
                *handle.lock().unwrap() = Some(warnings::fetch(bbox, tolerance, 20));
            })
            .is_ok()
        {
            self.key_pending = Some(answer);
        }
    }

    /// What each colour on the warning layer means, for the ones on screen.
    ///
    /// The service publishes a hundred and eleven of these and draws them as
    /// pixels, so without this a reader has a magenta county and no way to
    /// learn that it is an extreme heat warning. Listed rather than legended:
    /// only what is actually in force here, most serious first.
    fn draw_key(&self, painter: &egui::Painter, rect: Rect) {
        if self.key.is_empty() || rect.width() < 420.0 || rect.height() < 260.0 {
            return;
        }

        let font = egui::FontId::proportional(11.5);
        let heading = painter.layout_no_wrap(
            "IN FORCE HERE".to_owned(),
            egui::FontId::proportional(9.5),
            theme::TEXT_DIM,
        );
        // Measured before it is placed, so a long hazard name widens the panel
        // rather than running off it.
        // The room between the switches along the top and the credit below.
        let room = furniture_top(rect) - (rect.top() + 90.0);
        let rows = key_rows(room, self.key.len());
        if rows == 0 {
            return;
        }
        let shown: Vec<_> = self
            .key
            .iter()
            .take(rows)
            .map(|(hazard, colour)| {
                (
                    painter.layout_no_wrap(hazard.clone(), font.clone(), theme::TEXT),
                    *colour,
                )
            })
            .collect();
        let more = self.key.len().saturating_sub(shown.len());

        let widest = shown
            .iter()
            .map(|(galley, _)| galley.size().x)
            .fold(heading.size().x, f32::max);
        let width = (widest + SWATCH + 22.0).min(rect.width() * 0.4);
        let height = heading.size().y + 6.0 + KEY_ROW * shown.len() as f32
            + if more > 0 { KEY_ROW } else { 0.0 }
            + 14.0;

        // Bottom right: the intensity scale has the other corner, the scrubber
        // has the strip below, and the top is the caption and the switches.
        let panel = Rect::from_min_size(
            egui::pos2(rect.right() - width - 14.0, furniture_top(rect) - height),
            Vec2::new(width, height),
        );
        painter.rect_filled(panel, CornerRadius::same(6), Color32::from_black_alpha(180));

        let mut y = panel.top() + 7.0;
        painter.galley(egui::pos2(panel.left() + 10.0, y), heading, theme::TEXT_DIM);
        y += 6.0 + 10.0;

        for (galley, colour) in shown {
            let swatch = Rect::from_min_size(
                egui::pos2(panel.left() + 10.0, y + 2.0),
                Vec2::splat(SWATCH),
            );
            painter.rect_filled(swatch, CornerRadius::same(2), colour);
            // Outlined, because several of these are pale and the panel behind
            // them is nearly black - a swatch has to read as a colour rather
            // than as a hole.
            painter.rect_stroke(
                swatch,
                CornerRadius::same(2),
                Stroke::new(1.0_f32, Color32::from_black_alpha(120)),
                egui::StrokeKind::Inside,
            );
            let size = galley.size();
            painter.galley(
                egui::pos2(panel.left() + 10.0 + SWATCH + 8.0, y + (SWATCH - size.y) / 2.0 + 2.0),
                galley,
                theme::TEXT,
            );
            y += KEY_ROW;
        }
        if more > 0 {
            painter.text(
                egui::pos2(panel.left() + 10.0 + SWATCH + 8.0, y + 2.0),
                egui::Align2::LEFT_TOP,
                format!("and {more} more"),
                egui::FontId::proportional(10.5),
                theme::PLACEHOLDER,
            );
        }
    }

    /// The intensity scale.
    ///
    /// Drawn from the same stops the mosaic is coloured with rather than
    /// fetched. The layer serves its own legend as a fixed 500×30 raster
    /// whatever size is asked for, with nine-pixel labels and most of the bar
    /// given to the low end that never appears on screen. Green, yellow, red is
    /// a convention older than the television; this only says which is which.
    fn legend(&self, painter: &egui::Painter, rect: Rect) {
        const STOPS: [(f32, Color32); 6] = [
            (5.0, Color32::from_rgb(0x04, 0xE9, 0xE7)),
            (20.0, Color32::from_rgb(0x01, 0xC5, 0x01)),
            (35.0, Color32::from_rgb(0xFD, 0xF8, 0x02)),
            (45.0, Color32::from_rgb(0xFD, 0x95, 0x00)),
            (55.0, Color32::from_rgb(0xFD, 0x00, 0x00)),
            (65.0, Color32::from_rgb(0xBC, 0x00, 0xFF)),
        ];
        if rect.width() < 260.0 || rect.height() < 220.0 {
            return;
        }

        let width = (rect.width() * 0.26).clamp(130.0, 240.0);
        let bar = Rect::from_min_size(
            egui::pos2(rect.left() + 22.0, rect.bottom() - 84.0),
            Vec2::new(width, 9.0),
        );
        painter.rect_filled(
            bar.expand2(Vec2::new(12.0, 20.0)),
            CornerRadius::same(6),
            Color32::from_black_alpha(150),
        );

        // Two flat blocks per stop reads as a ramp at this size and needs no
        // gradient mesh.
        let step = bar.width() / (STOPS.len() - 1) as f32;
        for (index, (_, colour)) in STOPS.iter().enumerate().take(STOPS.len() - 1) {
            let left = bar.left() + index as f32 * step;
            for (half, shade) in [(0.0, *colour), (step / 2.0, STOPS[index + 1].1)] {
                painter.rect_filled(
                    Rect::from_min_size(
                        egui::pos2(left + half, bar.top()),
                        Vec2::new(step / 2.0 + 0.5, bar.height()),
                    ),
                    CornerRadius::ZERO,
                    shade,
                );
            }
        }

        for (at, align, text) in [
            (bar.left(), egui::Align2::LEFT_BOTTOM, "light"),
            (bar.right(), egui::Align2::RIGHT_BOTTOM, "heavy"),
        ] {
            painter.text(
                egui::pos2(at, bar.top() - 3.0),
                align,
                text,
                egui::FontId::proportional(10.0),
                theme::TEXT_DIM,
            );
        }
        for (at, align, value) in [
            (bar.left(), egui::Align2::LEFT_TOP, STOPS[0].0),
            (bar.right(), egui::Align2::RIGHT_TOP, STOPS[STOPS.len() - 1].0),
        ] {
            painter.text(
                egui::pos2(at, bar.bottom() + 3.0),
                align,
                format!("{value:.0} dBZ"),
                egui::FontId::proportional(9.5),
                theme::PLACEHOLDER,
            );
        }
    }

    /// Which layers are on, as three switches over the picture.
    fn switches(&mut self, ui: &mut egui::Ui, rect: Rect) {
        if rect.width() < 320.0 {
            return;
        }
        let painter = ui.painter_at(rect);
        let mut x = rect.left() + 16.0;
        let y = rect.top() + 54.0;

        let mut showing = self.showing;
        for (index, (label, on)) in [
            ("Radar", &mut showing.radar),
            ("Warnings", &mut showing.warnings),
            ("Labels", &mut showing.labels),
        ]
        .into_iter()
        .enumerate()
        {
            let galley = painter.layout_no_wrap(
                label.to_owned(),
                egui::FontId::proportional(11.5),
                theme::TEXT,
            );
            let pill = Rect::from_min_size(egui::pos2(x, y), galley.size() + Vec2::new(32.0, 10.0));
            let response = ui
                .interact(pill, ui.id().with(("radar-layer", index)), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);

            painter.rect_filled(
                pill,
                CornerRadius::same(5),
                if *on {
                    theme::ACCENT_DEEP
                } else {
                    Color32::from_black_alpha(170)
                },
            );
            // A tick rather than a checkbox: at this size a box and a tick are
            // the same three pixels, and only one of them says "on".
            if *on {
                let mark = egui::pos2(pill.left() + 11.0, pill.center().y);
                for (from, to) in [((-4.0, 0.0), (-1.5, 3.0)), ((-1.5, 3.0), (3.5, -3.5))] {
                    painter.line_segment(
                        [
                            mark + Vec2::new(from.0, from.1),
                            mark + Vec2::new(to.0, to.1),
                        ],
                        Stroke::new(1.7_f32, theme::TEXT),
                    );
                }
            }
            painter.galley(
                egui::pos2(pill.left() + 22.0, pill.center().y - galley.size().y / 2.0),
                galley,
                if *on { theme::TEXT } else { theme::TEXT_DIM },
            );

            if response.clicked() {
                *on = !*on;
            }
            x = pill.right() + 6.0;
        }
        self.showing = showing;
    }

    /// The way back to where you live.
    ///
    /// Double-clicking does it too, but nothing on screen says so — and a map
    /// dragged two states away with no visible way home is one somebody has to
    /// close and reopen.
    fn reset(&mut self, ui: &mut egui::Ui, rect: Rect, view: &mut Viewport, home: Viewport) {
        if !view.differs_from(&home) {
            return;
        }
        let painter = ui.painter_at(rect);
        let galley = painter.layout_no_wrap(
            "Reset view".to_owned(),
            egui::FontId::proportional(11.5),
            theme::TEXT,
        );
        let pill = Rect::from_min_size(
            egui::pos2(rect.right() - galley.size().x - 36.0, rect.top() + 54.0),
            galley.size() + Vec2::new(20.0, 10.0),
        );

        let response = ui
            .interact(pill, ui.id().with("radar-reset"), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        painter.rect_filled(
            pill,
            CornerRadius::same(5),
            if response.hovered() {
                theme::ACCENT_DEEP
            } else {
                Color32::from_black_alpha(170)
            },
        );
        painter.galley(
            egui::pos2(pill.left() + 10.0, pill.center().y - galley.size().y / 2.0),
            galley,
            theme::TEXT,
        );
        if response.clicked() {
            *view = home;
        }
    }

    /// The way back to the cameras.
    ///
    /// Double-clicking the map does it too, and nothing on screen says so -
    /// which is the same reason the Reset button exists beside a double-click
    /// that goes home. A radar filling the pane with no visible way out is one
    /// somebody leaves by hunting through the tabs.
    ///
    /// Top left, under the layer switches, because it is about leaving rather
    /// than about the map: the other corner is where the map's own controls
    /// live, and Reset view is already there.
    fn home_button(&mut self, ui: &mut egui::Ui, rect: Rect) -> bool {
        let painter = ui.painter_at(rect);
        let galley = painter.layout_no_wrap(
            "\u{2302}  Cameras".to_owned(),
            egui::FontId::proportional(11.5),
            theme::TEXT,
        );
        let pill = Rect::from_min_size(
            egui::pos2(rect.left() + 16.0, rect.top() + 88.0),
            galley.size() + Vec2::new(20.0, 10.0),
        );
        if !rect.contains(pill.max) {
            return false;
        }

        let response = ui
            .interact(pill, ui.id().with("radar-home"), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Back to the camera wall  (or double-click the map)");
        painter.rect_filled(
            pill,
            CornerRadius::same(5),
            if response.hovered() {
                theme::ACCENT_DEEP
            } else {
                Color32::from_black_alpha(170)
            },
        );
        painter.galley(
            egui::pos2(pill.left() + 10.0, pill.center().y - galley.size().y / 2.0),
            galley,
            theme::TEXT,
        );
        response.clicked()
    }

    /// The radius of the handle at the end of the scrubber's track.
    const HANDLE: f32 = 7.0;
    /// The margin the time is set against.
    const TIME_MARGIN: f32 = 12.0;

    /// Where the track ends, and where the time begins.
    ///
    /// One rule for both, because they are one constraint. The handle is a
    /// circle *centred* on the end of the track, so it reaches its radius past
    /// it — which is how it came to sit on the time when the loop reached the
    /// newest sweep. That is the one sweep whose caption is widened by "· now",
    /// and the widening was enough to overrun the fixed reservation this
    /// replaces.
    ///
    /// `widest` is the widest label the loop will show rather than the one
    /// showing, so the track keeps still while it plays.
    fn strip_end(strip: Rect, widest: f32) -> (f32, f32) {
        let time_left = strip.right() - Self::TIME_MARGIN - widest;
        (time_left - Self::HANDLE - 6.0, time_left)
    }

    /// The loop, as a strip that can be dragged.
    fn scrubber(&mut self, ui: &mut egui::Ui, rect: Rect, info: &RadarInfo, twenty_four: bool) {
        let count = info.times.len();
        // Only the sweeps being played. A track offering twenty minutes while
        // the loop holds twelve is a control that lies about what it can reach:
        // scrubbing to the left of what is held would show a sweep that has to
        // be fetched before it can be drawn, which is the stall this whole
        // arrangement exists to avoid.
        let first = self.first_frame(count);
        if count - first < 2 || rect.height() < 160.0 {
            return;
        }

        let strip = Rect::from_min_size(
            egui::pos2(rect.left(), rect.bottom() - 40.0),
            Vec2::new(rect.width(), 40.0),
        );
        let painter = ui.painter_at(rect);
        // Nearly opaque: the controls have to read against whatever the map
        // happens to be showing under them, and heavy rain is bright.
        painter.rect_filled(strip, CornerRadius::ZERO, Color32::from_black_alpha(225));

        // Play and pause.
        let button = Rect::from_min_size(
            egui::pos2(strip.left() + 12.0, strip.center().y - 11.0),
            Vec2::splat(22.0),
        );
        let play = ui
            .interact(button, ui.id().with("radar-play"), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if play.clicked() {
            self.playing = !self.playing;
        }

        let centre = button.center();
        if self.playing {
            for side in [-3.5f32, 3.5] {
                painter.rect_filled(
                    Rect::from_center_size(centre + Vec2::new(side, 0.0), Vec2::new(3.0, 12.0)),
                    CornerRadius::ZERO,
                    theme::TEXT,
                );
            }
        } else {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    centre + Vec2::new(-4.0, -6.5),
                    centre + Vec2::new(-4.0, 6.5),
                    centre + Vec2::new(6.5, 0.0),
                ],
                theme::TEXT,
                Stroke::NONE,
            ));
        }

        // The time at the end, and the room it needs.
        //
        // Measured rather than allowed for. A fixed reservation has to be right
        // for the *widest* label, and the widest is not the one the loop spends
        // most of its time showing: the newest sweep is captioned "· now",
        // which is half again as long as a clock — so the handle, which ends up
        // at the right-hand end of the track for exactly that sweep, ran into
        // it. Sized to the longest of what is showing and what will be showing
        // at the end, so the track does not shift about as the loop plays.
        let font = egui::FontId::proportional(11.5);
        let clock = |frame: usize| {
            info.clocks
                .get(frame)
                .copied()
                .flatten()
                .map(|clock| clock_text(clock, twenty_four))
                .unwrap_or_else(|| "—".into())
        };
        let caption = |frame: usize| {
            let when = clock(frame);
            if frame + 1 == count {
                format!("{when} · now")
            } else {
                when
            }
        };
        // Laid out uncoloured, so whether this is the newest sweep can still
        // decide the shade when it is painted further down.
        let showing = painter.layout_no_wrap(caption(self.frame), font.clone(), Color32::PLACEHOLDER);
        let widest = showing.size().x.max(
            painter
                .layout_no_wrap(caption(count - 1), font, Color32::PLACEHOLDER)
                .size()
                .x,
        );

        let (track_right, time_left) = Self::strip_end(strip, widest);
        let track = Rect::from_min_max(
            egui::pos2(button.right() + 14.0, strip.center().y - 3.0),
            egui::pos2(track_right, strip.center().y + 3.0),
        );
        if track.width() < 40.0 {
            return;
        }
        painter.rect_filled(track, CornerRadius::same(3), theme::BORDER);

        let grab = ui
            .interact(
                track.expand2(Vec2::new(8.0, 13.0)),
                ui.id().with("radar-track"),
                egui::Sense::click_and_drag(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if (grab.dragged() || grab.clicked()) && count - first > 1 {
            if let Some(at) = grab.interact_pointer_pos() {
                let along = ((at.x - track.left()) / track.width()).clamp(0.0, 1.0);
                let span = (count - 1 - first) as f32;
                self.frame = first + ((along * span).round() as usize).min(count - 1 - first);
                // Taking hold of the handle means watching this one.
                self.playing = false;
                self.hold = 0;
            }
        }

        let along = (self.frame - first) as f32 / (count - 1 - first) as f32;
        let handle_x = track.left() + along * track.width();
        painter.rect_filled(
            Rect::from_min_max(track.min, egui::pos2(handle_x, track.bottom())),
            CornerRadius::same(3),
            theme::ACCENT,
        );
        for index in first..count {
            if index == self.frame {
                continue;
            }
            let x = track.left()
                + (index - first) as f32 / (count - 1 - first) as f32 * track.width();
            // Dimmer while a sweep is still coming, so a loop that is holding
            // still is visibly filling rather than apparently stuck.
            let landed = self.ready.get(index).copied().unwrap_or(true);
            painter.circle_filled(
                egui::pos2(x, track.center().y),
                1.6,
                if landed { theme::TEXT_DIM } else { theme::PLACEHOLDER },
            );
        }
        painter.circle_filled(egui::pos2(handle_x, track.center().y), Self::HANDLE, theme::ACCENT);
        painter.circle_filled(egui::pos2(handle_x, track.center().y), 3.0, theme::INK);

        // Laid out before the track, because its width is what set the track's
        // right-hand end. Right-aligned within the room reserved, so a shorter
        // label still sits against the same margin.
        let latest = self.frame + 1 == count;
        painter.galley(
            egui::pos2(
                time_left + widest - showing.size().x,
                strip.center().y - showing.size().y / 2.0,
            ),
            showing,
            if latest { theme::TEXT } else { theme::TEXT_DIM },
        );
    }

    /// What the service said about the point that was clicked.
    fn read_out(&mut self, ui: &mut egui::Ui, rect: Rect) {
        // Taken out before it is read, so the answer can be stored back
        // without the borrow of `self.identify` still being alive.
        let answered = self
            .identify
            .as_ref()
            .and_then(|pending| pending.lock().unwrap().take());
        if let Some(said) = answered {
            // Only if it learned something. A failed fetch leaves the local
            // answer standing rather than replacing it with a blank one — the
            // hazard and when it runs out came off the geometry and are true
            // whether or not the alert could be read.
            if let Some(detail) = self.detail.as_mut() {
                if !said.is_empty() {
                    detail.found = said;
                }
            }
            self.identify = None;
        }

        let Some(detail) = &self.detail else { return };
        let painter = ui.painter_at(rect);

        // No waiting state, and nothing to spin. The click was answered out of
        // the geometry before this was ever called, so there is never a moment
        // where the box is on screen with nothing in it — the headline arrives
        // into a panel that already says what the hazard is.

        let mut lines: Vec<(String, egui::FontId, Color32)> = Vec::new();
        for warning in &detail.found {
            lines.push((
                warning.event.clone(),
                egui::FontId::proportional(12.5),
                // The strongest treatment for the ones that warrant it, the
                // same rule the alert banner follows.
                if warning.severe { theme::ERROR } else { theme::WARN },
            ));
            if !warning.said.is_empty() {
                lines.push((
                    warning.said.clone(),
                    egui::FontId::proportional(11.0),
                    theme::TEXT_DIM,
                ));
            }
        }

        let width = 320f32.min(rect.width() - 24.0);
        let laid: Vec<_> = lines
            .into_iter()
            .map(|(text, font, colour)| {
                let mut job = egui::text::LayoutJob::simple(text, font, colour, width - 36.0);
                job.wrap.max_rows = 4;
                (painter.layout_job(job), colour)
            })
            .collect();
        let height: f32 = laid.iter().map(|(g, _)| g.size().y + 3.0).sum::<f32>() + 14.0;

        // Kept inside the map, so a click near an edge does not put the answer
        // off the side of it.
        let anchor = egui::pos2(
            detail
                .at
                .x
                .min(rect.right() - width - 8.0)
                .max(rect.left() + 8.0),
            detail
                .at
                .y
                .min(rect.bottom() - height - 48.0)
                .max(rect.top() + 8.0),
        );
        let box_ = Rect::from_min_size(anchor, Vec2::new(width, height));

        painter.rect_filled(box_, CornerRadius::same(7), theme::PANEL_ALT);
        painter.rect_stroke(
            box_,
            CornerRadius::same(7),
            Stroke::new(1.0_f32, theme::BORDER),
            egui::StrokeKind::Inside,
        );

        let mut y = box_.top() + 7.0;
        for (galley, colour) in laid {
            let size = galley.size();
            painter.galley(egui::pos2(box_.left() + 10.0, y), galley, colour);
            y += size.y + 3.0;
        }

        // A cross, because a panel with no visible way to close it is one
        // people try to close by clicking away from it — which on a map is
        // another question, not a dismissal.
        let close = Rect::from_center_size(
            egui::pos2(box_.right() - 13.0, box_.top() + 13.0),
            Vec2::splat(18.0),
        );
        let over = ui.interact(box_, ui.id().with("radar-detail"), egui::Sense::click());
        let on_close = ui
            .interact(close, ui.id().with("radar-detail-close"), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);

        let mark = if on_close.hovered() { theme::TEXT } else { theme::PLACEHOLDER };
        for (from, to) in [((-3.5, -3.5), (3.5, 3.5)), ((-3.5, 3.5), (3.5, -3.5))] {
            painter.line_segment(
                [
                    close.center() + Vec2::new(from.0, from.1),
                    close.center() + Vec2::new(to.0, to.1),
                ],
                Stroke::new(1.5_f32, mark),
            );
        }

        if over.clicked() || on_close.clicked() {
            self.detail = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(frames: usize) -> RadarInfo {
        RadarInfo {
            ok: true,
            times: (0..frames).map(|i| format!("t{i}")).collect(),
            clocks: vec![None; frames],
            ..RadarInfo::default()
        }
    }

    /// The loop plays forwards and waits on the current sweep, so the picture
    /// on screen longest is the picture of now.
    #[test]
    fn the_loop_holds_on_the_newest_sweep() {
        let mut view = RadarView::default();
        // The loop has arrived; holding for it is its own test below.
        view.ready = vec![true; 4];
        let mut tick = |view: &mut RadarView| {
            view.stepped_at = Instant::now() - STEP * 2;
            view.step(4);
        };

        view.frame = 0;
        for expected in [1, 2, 3] {
            tick(&mut view);
            assert_eq!(view.frame, expected);
        }
        for _ in 0..HOLD_STEPS {
            tick(&mut view);
            assert_eq!(view.frame, 3, "the newest sweep is held");
        }
        tick(&mut view);
        assert_eq!(view.frame, 0, "and then it starts again");
    }

    /// The loop sits on the newest sweep until the rest of it has arrived.
    ///
    /// Stepping into sweeps that are still coming is what "half the map is
    /// missing for half a minute" actually was: the loop moves every 400ms, so
    /// within four seconds every sweep has been asked for as urgently as the
    /// one being looked at, and the picture on screen queues behind nine that
    /// nobody is looking at.
    #[test]
    fn the_loop_waits_for_itself_before_playing() {
        let mut view = RadarView::default();
        view.ready = vec![true, true, false, true];
        view.frame = 0;
        view.stepped_at = Instant::now() - STEP * 2;
        view.step(4);
        assert_eq!(view.frame, 3, "sits on the newest while one is still coming");

        // Once it has all landed, it plays from the start as usual.
        view.ready = vec![true; 4];
        view.frame = 0;
        view.stepped_at = Instant::now() - STEP * 2;
        view.step(4);
        assert_eq!(view.frame, 1);

        // A sweep that never arrives must not hold the picture for ever.
        view.ready = vec![true, true, false, true];
        view.fetching_since = Instant::now() - LOOP_STALL * 2;
        view.frame = 0;
        view.stepped_at = Instant::now() - STEP * 2;
        view.step(4);
        assert_eq!(view.frame, 1, "gives up waiting and plays anyway");

        // Only the sweeps it would actually play are waited on.
        view.playable = 2;
        view.ready = vec![false, false, true, true];
        view.fetching_since = Instant::now();
        assert!(!view.loop_waiting(4), "the two it plays have both landed");
    }

    /// What restarts the clock is a *sweep* landing. The loop is waiting for
    /// its own tiles; the ground and the lettering arrive on their own schedule
    /// and say nothing about whether it has filled.
    #[test]
    fn a_sweep_landing_is_what_says_the_fetch_is_alive() {
        use crate::weather::tiles::Decoded;

        let ctx = egui::Context::default();
        let sweep = |layer| Decoded {
            id: TileId { layer, level: 8, x: 1, y: 1 },
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 0],
            shapes: Some(crate::weather::reflectivity::Shapes::default()),
            opaque: false,
        };

        let mut view = RadarView::default();
        view.fetching_since = Instant::now() - LOOP_STALL * 2;
        view.store.deliver(sweep(Layer::Radar(1)));
        view.take_up(&ctx);
        assert!(
            view.fetching_since.elapsed() < LOOP_STALL,
            "a sweep landing means the fetch is still going"
        );

        // The basemap is not the loop.
        view.fetching_since = Instant::now() - LOOP_STALL * 2;
        view.store.deliver(sweep(Layer::Base));
        view.take_up(&ctx);
        assert!(
            view.fetching_since.elapsed() > LOOP_STALL,
            "the ground arriving says nothing about the loop"
        );
    }

    /// The backstop. Waiting on silence assumes arrivals eventually stop, and a
    /// loop that did not fit its cache would land tiles for ever — so there is
    /// still an outer bound, far past anything a real fetch takes.
    #[test]
    fn the_loop_does_not_hold_the_picture_for_ever() {
        let mut view = RadarView::default();
        view.playable = 4;
        view.ready = vec![true, true, false, true];
        // Tiles still landing, so the stall clock says keep waiting...
        view.fetching_since = Instant::now();
        // ...but this has been going on far too long.
        view.filling_since = Instant::now() - LOOP_PATIENCE * 2;
        view.frame = 0;
        view.stepped_at = Instant::now() - STEP * 2;
        view.step(4);
        assert_eq!(view.frame, 1, "a loop that never fills still has to play");

        assert!(
            LOOP_PATIENCE > LOOP_STALL * 4,
            "the backstop must be far past the rule it backs up"
        );
    }

    /// The loop gives up on silence, not on a stopwatch.
    ///
    /// As a deadline measured from the first request it was a guess at how long
    /// a whole loop takes to fetch, and a wide view over a busy sky outran it —
    /// so the loop began part-filled and stuttered through the gaps, which is
    /// the one thing the wait exists to prevent. A fetch that is still landing
    /// tiles has to be waited for however long it takes.
    #[test]
    fn a_loop_still_arriving_is_waited_for_however_long_it_takes() {
        let mut view = RadarView::default();
        view.playable = 4;
        view.ready = vec![true, true, false, true];

        // Far longer than any fixed deadline would have allowed, but a tile
        // landed a moment ago, so it is still filling and still worth waiting.
        view.fetching_since = Instant::now() - LOOP_STALL / 4;
        view.frame = 0;
        view.stepped_at = Instant::now() - STEP * 2;
        view.step(4);
        assert_eq!(view.frame, 3, "still filling, so it sits on the newest sweep");

        // Nothing has landed for a while: this one is not coming.
        view.fetching_since = Instant::now() - LOOP_STALL * 2;
        view.frame = 0;
        view.stepped_at = Instant::now() - STEP * 2;
        view.step(4);
        assert_eq!(view.frame, 1, "gone quiet, so it plays with what it has");
    }

    /// The key lists as much as it has room for. A 4K wall can show every
    /// advisory in force and has no business hiding seventeen of them behind a
    /// count; a small window has room for three and should say so.
    #[test]
    fn the_key_lists_as_much_as_it_has_room_for() {
        // A wall on a 4K screen: room for everything in force.
        assert_eq!(key_rows(1700.0, 19), 19);
        // A modest window: as many as fit, keeping a row back for the count.
        let small = key_rows(140.0, 19);
        assert!((LEAST_KEY_ROWS..19).contains(&small), "got {small}");
        assert!(30.0 + KEY_ROW * (small + 1) as f32 <= 140.0 + KEY_ROW, "{small} rows overruns");
        // Never fewer than three, however cramped, and never more than there
        // are to show.
        assert_eq!(key_rows(0.0, 19), LEAST_KEY_ROWS);
        assert_eq!(key_rows(1700.0, 2), 2);
        assert_eq!(key_rows(1700.0, 0), 0);
        assert_eq!(key_rows(0.0, 1), 1, "one hazard needs no count line");
    }

    /// The credit has the lowest place on that side because its terms ask for
    /// it to be legible. The key landed straight on top of it the first time,
    /// which is why the two now share one measurement.
    #[test]
    fn the_key_stacks_above_the_credit() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(1600.0, 900.0));
        let top = furniture_top(rect);
        assert!(top < rect.bottom() - 48.0, "the scrubber's strip is below it");
        assert!(
            rect.bottom() - top >= 48.0 + CREDIT_ROOM,
            "the credit has to fit between them"
        );
    }

    /// The loop shortens; the picture does not soften. That is the whole trade,
    /// and it only ever gives anything up on a large window over a wide view.
    #[test]
    fn the_loop_gives_way_before_the_picture_does() {
        use crate::weather::radar::FRAMES;

        // An ordinary view costs little enough that the whole loop is held.
        assert_eq!(playable_frames(FRAMES, 40), FRAMES, "a 400km view on 1080p");
        assert_eq!(playable_frames(FRAMES, 66), FRAMES, "the country on 1080p");

        // A 4K screen over the country wants 220 tiles a sweep, and cannot have
        // ten of those - so it holds fewer, at full sharpness.
        let wide = playable_frames(FRAMES, 220);
        assert!(wide < FRAMES && wide >= LEAST_FRAMES, "got {wide}");
        assert!(wide * 220 <= LOOP_TILES, "{wide} sweeps of 220 overruns the budget");

        // It never collapses to a still, however dear a sweep gets.
        assert_eq!(playable_frames(FRAMES, 100_000), LEAST_FRAMES);
        // And it never claims more than the service actually sent.
        assert_eq!(playable_frames(3, 1), 3);
        assert_eq!(playable_frames(1, 100_000), 1, "one sweep is all there is");
        assert_eq!(playable_frames(0, 220), 0);
    }

    /// Playing the newest sweeps means wrapping to the start of what is held,
    /// not to the start of what the service sent - otherwise the loop steps
    /// into sweeps nothing has fetched and the picture stalls.
    #[test]
    fn the_loop_wraps_to_what_is_held() {
        let mut view = RadarView::default();
        view.playable = 3;
        view.ready = vec![true; 10];
        assert_eq!(view.first_frame(10), 7, "the newest three of ten");

        view.frame = 9;
        view.hold = HOLD_STEPS;
        view.stepped_at = Instant::now() - STEP * 2;
        view.step(10);
        assert_eq!(view.frame, 7, "wraps to the oldest one held, not to zero");

        // A window that grew, or a list that shrank, must not leave the loop
        // pointing behind its own start.
        view.frame = 0;
        view.step(10);
        assert_eq!(view.frame, 7);
    }

    #[test]
    fn a_loop_of_one_or_none_does_not_step() {
        let mut view = RadarView::default();
        for count in [0usize, 1] {
            view.frame = 0;
            view.stepped_at = Instant::now() - STEP * 2;
            view.step(count);
            assert_eq!(view.frame, 0, "count {count}");
        }
    }

    #[test]
    fn a_paused_loop_stays_where_it_is() {
        let mut view = RadarView::default();
        view.playing = false;
        view.frame = 2;
        view.stepped_at = Instant::now() - STEP * 10;
        view.step(4);
        assert_eq!(view.frame, 2);
    }

    /// A new list of sweeps means a new newest one, and that is what should be
    /// on screen — not whatever index the old loop happened to be at.
    #[test]
    fn a_fresh_sweep_list_shows_the_newest() {
        let mut view = RadarView::default();
        view.aim(&info(10), "dark");
        assert_eq!(view.frame, 9);

        view.frame = 3;
        // The same list again changes nothing, including the frame.
        view.aim(&info(10), "dark");
        assert_eq!(view.frame, 3, "an unchanged list must not jump the loop");
    }

    /// Changing the basemap throws the ground away; a new sweep list does not,
    /// because those tiles are keyed by place rather than by time.
    #[test]
    fn only_what_actually_changed_is_dropped() {
        let mut view = RadarView::default();
        view.aim(&info(4), "dark");

        let base = TileId { layer: Layer::Base, level: 8, x: 1, y: 1 };
        let radar = TileId { layer: Layer::Radar(0), level: 8, x: 1, y: 1 };
        // Stand in for uploaded tiles by recording them as held.
        view.lru.touch(&base);
        view.lru.touch(&radar);

        // A new sweep list: the ground survives.
        view.sources = Some(crate::weather::radar::Sources {
            region: "conus".into(),
            style: "dark".into(),
            times: vec!["a".into()],
        });
        view.aim(&info(4), "dark");
        assert!(view.sources.as_ref().unwrap().times.len() == 4);
    }

    /// The wall cell has no room for furniture, and what it does show has to be
    /// measured rather than assumed — the overlap this replaced was two fixed
    /// labels drawn from opposite edges.
    #[test]
    fn the_compact_caption_is_bounded_by_its_cell() {
        // Exercised through the sizing rule the caption uses, which is the part
        // that decides whether anything collides.
        for height in [80.0f32, 200.0, 900.0] {
            let size = (height * 0.055).clamp(9.0, 13.0);
            assert!((9.0..=13.0).contains(&size), "at {height}px got {size}");
        }
    }

    /// The handle and the time share the right-hand end of the strip, and the
    /// handle is centred on the end of the track — so the room kept for the
    /// time has to cover the label *and* the half of the handle that reaches
    /// past. It did not, once the newest sweep's caption grew a "· now".
    #[test]
    fn the_handle_never_reaches_the_time() {
        let strip = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(600.0, 40.0));

        // From a bare clock through to the widest thing the strip ever shows.
        for widest in [24.0f32, 48.0, 92.0, 140.0] {
            let (track_right, time_left) = RadarView::strip_end(strip, widest);
            assert!(
                track_right + RadarView::HANDLE <= time_left,
                "a {widest}px label leaves the handle at {} over text starting at {time_left}",
                track_right + RadarView::HANDLE
            );
            assert!(
                time_left + widest <= strip.right(),
                "and the time itself stays inside the strip"
            );
        }
    }

    /// Every view the wall is actually shown in has to come out sharp *and*
    /// fit in the cache, and the two pull against each other: a budget in tiles
    /// is a budget in sharpness, so tightening one to bound memory is how the
    /// map went soft on a 4K screen without a single test noticing.
    ///
    /// The one that existed multiplied the two ceilings together, which cannot
    /// happen — the sweeps are chosen relative to the ground, so both can never
    /// be at their limit at once — and it asserted nothing at all about the
    /// picture. This walks the screens and the spans instead.
    #[test]
    fn no_view_is_drawn_soft_or_larger_than_the_cache() {
        use crate::weather::tiles::Viewport;

        // Windows the map is really drawn into, in points, with the pixel
        // density that goes with each.
        let screens = [
            ("1080p", 1920.0f32, 950.0f32, 1.0f32),
            ("1440p", 2560.0, 1300.0, 1.0),
            ("4K", 3840.0, 1980.0, 1.0),
            ("4K hidpi", 1920.0, 990.0, 2.0),
            ("a wall cell", 620.0, 340.0, 1.0),
        ];
        // From the whole country down to a street corner.
        let spans = [4500.0f64, 1200.0, 400.0, 120.0, 40.0];

        for (name, w, h, ppp) in screens {
            for span in spans {
                let view = Viewport::new(39.8, -98.6, Viewport::zoom_for_span(39.8, span, h))
                    .for_screen(ppp);
                let ground = view.level_within(GROUND_TILES, w, h);
                // The sweeps follow the ground until the mosaic runs out of
                // resolution, and stop there.
                let sweep = ground.min(crate::weather::radar::MOSAIC_LEVEL);
                let at = |span: f64| format!("{name} at {span:.0}km");

                // The ground carries the text and the hard edges - place names
                // and warning polygons - so it holds the band the map has
                // claimed since it was written.
                assert!(
                    view.stretch_at(ground) <= 1.45,
                    "{} draws the ground at {:.2}x life size",
                    at(span),
                    view.stretch_at(ground)
                );
                // Where the mosaic still has resolution to give, the sweeps
                // hold the same band as the ground - an upscale of a
                // colour-mapped picture shows on every dBZ band edge, and that
                // is what "badly stretched" was. Past level 7 there is nothing
                // left to hold: the cells are a kilometre of ground, so the
                // picture is scaled up whatever is done, and the only question
                // is whether the GPU does it smoothly or GeoServer does it in
                // squares.
                if sweep == ground {
                    assert!(
                        view.stretch_at(sweep) <= 1.45,
                        "{} draws the sweeps at {:.2}x life size",
                        at(span),
                        view.stretch_at(sweep)
                    );
                } else {
                    assert_eq!(
                        sweep,
                        crate::weather::radar::MOSAIC_LEVEL,
                        "{} fetches the mosaic somewhere other than where it lives",
                        at(span)
                    );
                }

                // And what it costs still fits: two ground layers - the dark
                // canvas serves its lettering separately - plus as much of the
                // loop as is being played. It was three while the warning
                // polygons were tiles too; they hold no textures now.
                let per_sweep = view.tile_count(sweep, w, h);
                let frames = playable_frames(crate::weather::radar::FRAMES, per_sweep);
                assert!(frames >= LEAST_FRAMES, "{} plays only {frames} sweeps", at(span));
                let held = view.tile_count(ground, w, h) * 2 + per_sweep * frames;
                assert!(
                    held <= TILE_LIMIT,
                    "{} holds {held} tiles and the cache keeps {TILE_LIMIT}",
                    at(span)
                );
            }
        }
    }

    /// A traced sweep is dearer than a plain tile, so a full cache has to stay
    /// near the budget the old fixed 256-pixel tiles set — whatever the zoom,
    /// because the zoom no longer changes what a sweep costs. That is the point
    /// of drawing them as shapes: a zoom is a transform, and a transform is
    /// free.
    #[test]
    fn a_full_cache_costs_about_the_same_at_every_zoom() {
        /// The busiest tile measured against the live service.
        const WORST_SHAPE: usize = 560 * 1024;
        let was = TILE_LIMIT * 256 * 256 * 4;

        let limit = tile_limit();
        let sweeps = limit.saturating_sub(GROUND_TILES * 2);
        let held = GROUND_TILES * 2 * 256 * 256 * 4 + sweeps * WORST_SHAPE;
        assert!(
            held <= was,
            "a full cache is {:.0} MB against the old {:.0} MB",
            held as f64 / 1024.0 / 1024.0,
            was as f64 / 1024.0 / 1024.0
        );
        assert!(sweeps >= LEAST_FRAMES * 4, "only room for {sweeps} sweeps");
    }

    /// Between one sweep and the next the loop travels, and how far along it is
    /// is what slides the weather. Nothing else in the loop changed: the sweep
    /// on screen is still the sweep on screen.
    #[test]
    fn the_loop_travels_between_sweeps_rather_than_jumping() {
        let mut view = RadarView::default();
        view.ready = vec![true; 6];
        view.playable = 6;
        view.frame = 2;
        view.stepped_at = Instant::now() - STEP / 2;
        view.step(6);

        assert_eq!(view.frame, 2, "it is still showing the sweep it was showing");
        assert!(
            (0.3..=0.7).contains(&view.into),
            "half way between sweeps and it reads as {}",
            view.into
        );

        // And a whole step later it has moved on and starts again from nothing.
        view.stepped_at = Instant::now() - STEP;
        view.step(6);
        assert_eq!(view.frame, 3);
        assert_eq!(view.into, 0.0, "a fresh sweep has not travelled anywhere yet");
    }

    /// Three places where there is nothing to travel toward, and sliding anyway
    /// would drag the weather somewhere it never went.
    #[test]
    fn there_is_nothing_to_slide_toward_at_the_ends() {
        // Holding at the newest sweep: the next sweep does not exist yet.
        let mut view = RadarView::default();
        view.ready = vec![true; 4];
        view.playable = 4;
        view.frame = 3;
        view.stepped_at = Instant::now() - STEP / 2;
        view.step(4);
        assert_eq!(view.into, 0.0, "the newest sweep has nowhere to go");

        // Waiting for the loop to arrive.
        let mut view = RadarView::default();
        view.ready = vec![true, false, true, true];
        view.playable = 4;
        view.frame = 1;
        view.fetching_since = Instant::now();
        view.stepped_at = Instant::now() - STEP / 2;
        view.step(4);
        assert_eq!(view.into, 0.0, "a loop still arriving is not travelling");

        // Paused.
        let mut view = RadarView::default();
        view.ready = vec![true; 4];
        view.playable = 4;
        view.playing = false;
        view.frame = 1;
        view.stepped_at = Instant::now() - STEP / 2;
        view.step(4);
        assert_eq!(view.into, 0.0, "a paused loop is not travelling");
    }

    /// The crossing occupies the end of a step and nothing before it. A sweep
    /// that is still travelling is one sweep; only near the change is there a
    /// second one on screen at all.
    #[test]
    fn the_crossing_is_the_end_of_a_step_and_no_more() {
        assert_eq!(blend(0.0), 0.0, "a fresh sweep is not crossing anywhere");
        // Expressed against FADE rather than at a fixed 0.2, which was inside
        // the settled part when the crossing was narrower and inside the
        // crossing once it widened.
        assert_eq!(
            blend((1.0 - FADE) * 0.5),
            0.0,
            "nor part way through the settled half of its step"
        );
        assert_eq!(blend(1.0 - FADE), 0.0, "the crossing has not started");
        assert!(blend(1.0 - FADE / 2.0) > 0.4, "half way across");
        assert!(blend(1.0 - FADE / 2.0) < 0.6);
        assert_eq!(blend(1.0), 1.0, "and it lands exactly as the step does");

        // There is still a crisp sweep in every cycle: the crossing is most of
        // a step and not the whole of it.
        assert!(FADE < 1.0, "a loop that never settles has no sharp edge in it");

        // And "not the whole of it" has to mean something on screen. The part
        // of a step that is not crossing must last at least one drawn frame, or
        // there is no frame anywhere in the loop showing a single sweep at full
        // opacity — which is the entire reason the echo is traced into shapes
        // rather than drawn as pictures. It is the constraint that stops FADE
        // going higher without STEP going with it, and it is why the two were
        // raised together.
        //
        // Written against both constants rather than against a number, so it
        // keeps meaning the same thing when either moves.
        let sharp = STEP.mul_f32(1.0 - FADE);
        assert!(
            sharp >= PLAYING,
            "a crossing this wide leaves no frame with a crisp sweep in it: \
             {sharp:?} of settled picture against a {PLAYING:?} frame"
        );

        // Eased at both ends, so it begins and finishes gently. A linear ramp
        // covers a tenth of the crossing in its first tenth; this covers much
        // less, and the same at the far end.
        let tenth = 1.0 - FADE + FADE * 0.1;
        assert!(blend(tenth) < 0.05, "it should ease in, not switch on");
        let last = 1.0 - FADE * 0.1;
        assert!(blend(last) > 0.95, "and ease out rather than stop dead");

        // The middle is still quick — easing spends the ends, it does not make
        // the whole thing sluggish.
        let quarter = 1.0 - FADE * 0.75;
        let three_quarters = 1.0 - FADE * 0.25;
        assert!(
            blend(three_quarters) - blend(quarter) > 0.6,
            "the middle half of the crossing should carry most of it"
        );

        // Monotonic, so nothing on screen goes backwards.
        let mut last = 0.0;
        for step in 0..=40 {
            let now = blend(step as f32 / 40.0);
            assert!(now >= last, "the crossing went backwards at {step}");
            last = now;
        }
    }

    /// The two sweeps of a crossing have to very nearly cover the ground
    /// between them, or a storm both of them agree is solid shows the map
    /// through its middle every time the loop steps.
    #[test]
    fn a_crossing_very_nearly_covers_the_ground_under_it() {
        // The ends are whole: one sweep at full, the other absent.
        assert_eq!(opacities(0.0), (1.0, 0.0));
        assert_eq!(opacities(1.0), (0.0, 1.0));

        // Everywhere between, what is left uncovered where the two agree is the
        // product of what each of them is not. Straight `1 - t` and `t` would
        // leave a quarter of it showing at the midpoint; this keeps it under a
        // tenth the whole way across.
        for step in 0..=20 {
            let (leaving, arriving) = opacities(step as f32 / 20.0);
            let gap = (1.0 - leaving) * (1.0 - arriving);
            assert!(gap < 0.09, "{gap:.3} of the map shows through at {step}");
            assert!(leaving >= 0.0 && leaving <= 1.0);
            assert!(arriving >= 0.0 && arriving <= 1.0);
        }

        // And it is worth having: the naive pair is nearly three times worse at
        // the midpoint, which is what was measured on a real storm.
        let (leaving, arriving) = opacities(0.5);
        let naive = (1.0 - 0.5_f32) * (1.0 - 0.5_f32);
        assert!((1.0 - leaving) * (1.0 - arriving) < naive / 2.0);
    }

    /// Everywhere the loop is not travelling, it is not crossing either.
    ///
    /// `into` is already pinned to zero while paused, while holding on the
    /// newest sweep, while the loop is still arriving and across the wrap — so
    /// the four cases a crossing would be wrong in are the four cases it does
    /// not happen in, without the crossing having to know about any of them.
    #[test]
    fn a_loop_that_is_not_travelling_is_not_crossing() {
        assert_eq!(blend(0.0), 0.0);

        for mut view in [
            {
                // Holding at the newest sweep.
                let mut view = RadarView::default();
                view.ready = vec![true; 4];
                view.playable = 4;
                view.frame = 3;
                view
            },
            {
                // Paused.
                let mut view = RadarView::default();
                view.ready = vec![true; 4];
                view.playable = 4;
                view.playing = false;
                view.frame = 1;
                view
            },
            {
                // About to wrap.
                let mut view = RadarView::default();
                view.ready = vec![true; 5];
                view.playable = 5;
                view.frame = 4;
                view.hold = HOLD_STEPS;
                view
            },
        ] {
            let count = view.ready.len();
            view.stepped_at = Instant::now() - STEP / 2;
            view.step(count);
            assert_eq!(blend(view.into), 0.0, "one sweep, not two");
        }
    }

    /// The sweep arriving is drawn behind where it will be by exactly as far as
    /// it still has to travel, so both sweeps are over the same ground the whole
    /// way across and the crossing is opacity alone.
    ///
    /// Without this the two would be a whole step's travel apart, and every
    /// storm would be drawn twice in two places while they changed over.
    #[test]
    fn both_sweeps_of_a_crossing_are_over_the_same_ground() {
        let sky = [0.06_f32, -0.02_f32];
        for into in [0.72_f32, 0.85, 0.93, 1.0] {
            let leaving = [sky[0] * into, sky[1] * into];
            let behind = into - 1.0;
            let arriving = [sky[0] * behind, sky[1] * behind];

            // A step apart in their own coordinates, which is exactly the
            // ground between one sweep and the next.
            assert!((leaving[0] - arriving[0] - sky[0]).abs() < 1e-6);
            assert!((leaving[1] - arriving[1] - sky[1]).abs() < 1e-6);
        }

        // And as the step lands the arriving sweep is at its own position,
        // which is what the sweep it becomes is drawn at a frame later.
        let behind = 1.0_f32 - 1.0;
        assert_eq!([sky[0] * behind, sky[1] * behind], [0.0, 0.0]);
    }

    /// The wrap stays a cut. Going from the last sweep back to the first is nine
    /// sweeps backwards, and no displacement expresses that — sliding into it
    /// would drag the whole sky the wrong way for four hundred milliseconds.
    #[test]
    fn the_wrap_back_to_the_start_is_a_cut() {
        let mut view = RadarView::default();
        view.ready = vec![true; 5];
        view.playable = 5;
        view.frame = 4;
        // Past the hold, so the next step is the wrap itself.
        view.hold = HOLD_STEPS;
        view.stepped_at = Instant::now() - STEP / 2;
        view.step(5);
        assert_eq!(view.into, 0.0, "the loop is about to wrap, not to travel");
    }

    /// A tile whose weather sits at a given offset, so a pair of them has a
    /// motion to find.
    fn drifting(shift: f64) -> crate::weather::reflectivity::Shapes {
        use crate::weather::reflectivity::{Shapes, COARSE};
        let mut coarse = vec![0u8; COARSE * COARSE];
        for row in 0..COARSE {
            for column in 0..COARSE {
                let dx = column as f64 - (22.0 + shift);
                let dy = row as f64 - 30.0;
                let near = (1.0 - (dx * dx + dy * dy).sqrt() / 8.0).max(0.0);
                coarse[row * COARSE + column] = (64.0 + near * 90.0) as u8;
            }
        }
        Shapes { bands: Vec::new(), coarse }
    }

    /// The whole sweep travels as one thing, and that is not a simplification —
    /// it is the only thing that can be right. Every tile's shapes are clipped
    /// to its own ground, so a storm lying across a boundary is two pieces that
    /// stay joined only while both move by the same amount. Giving each tile its
    /// own vector tore every such storm in half and left a seam down each
    /// boundary.
    #[test]
    fn the_whole_sweep_travels_as_one_thing() {
        let mut view = RadarView::default();
        let at = |frame: usize, x: u32| TileId { layer: Layer::Radar(frame), level: 8, x, y: 0 };
        let visible: Vec<(u32, u32)> = (0..5).map(|x| (x, 0)).collect();

        // Five tiles, each of whose weather moved by a slightly different
        // amount — which is what real tiles do.
        for (x, moved) in [(0u32, 1.0), (1, 2.0), (2, 3.0), (3, 4.0), (4, 5.0)] {
            view.shapes.insert(at(0, x), drifting(0.0));
            view.shapes.insert(at(1, x), drifting(moved));
        }

        let sky = view.settle_drift(&visible, 8, 0, 2);
        assert!(sky[0] > 0.0, "the sky did not move at all: {sky:?}");

        // Asking again gives the same answer whatever is visible by then. The
        // estimate is a median over whichever tiles happen to be loaded, and
        // that set changes as tiles arrive and are evicted — recomputing every
        // frame made the whole sky twitch between one answer and the next.
        let again = view.settle_drift(&visible[..2], 8, 0, 2);
        assert_eq!(sky, again, "the sweep's motion changed under it");
    }

    /// One tile agreeing is not agreement. Until enough of them say the same
    /// thing the sweep stands still, because a guess from a single tile moves
    /// the entire sky on the strength of one cell.
    #[test]
    fn the_sky_stands_still_until_enough_of_it_agrees() {
        let mut view = RadarView::default();
        let at = |frame: usize, x: u32| TileId { layer: Layer::Radar(frame), level: 8, x, y: 0 };

        // Two tiles with weather, three of empty sky that cannot say.
        for x in 0..2u32 {
            view.shapes.insert(at(0, x), drifting(0.0));
            view.shapes.insert(at(1, x), drifting(3.0));
        }
        let flat = crate::weather::reflectivity::Shapes {
            bands: Vec::new(),
            coarse: vec![64; crate::weather::reflectivity::COARSE.pow(2)],
        };
        for x in 2..5u32 {
            view.shapes.insert(at(0, x), flat.clone());
            view.shapes.insert(at(1, x), flat.clone());
        }

        let visible: Vec<(u32, u32)> = (0..5).map(|x| (x, 0)).collect();
        assert_eq!(
            view.settle_drift(&visible, 8, 0, 2),
            [0.0, 0.0],
            "two tiles is not enough of a sky to move"
        );
        // And having declined, it must not have remembered declining — the
        // tiles it needs may simply not have arrived yet.
        assert!(view.drift.is_empty(), "it wrote off a sweep it had not seen");
    }

    /// The newest sweep has nothing after it to travel toward.
    #[test]
    fn the_newest_sweep_does_not_travel() {
        let mut view = RadarView::default();
        assert_eq!(view.settle_drift(&[(0, 0)], 8, 3, 4), [0.0, 0.0]);
    }

    /// Layers default to everything on, which is what the picture was before
    /// there were switches.
    #[test]
    fn every_layer_starts_on() {
        let showing = Showing::default();
        assert!(showing.radar && showing.warnings && showing.labels);
    }
}
