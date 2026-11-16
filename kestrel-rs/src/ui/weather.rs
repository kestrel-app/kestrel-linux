//! The weather on screen: the glyphs, the strip over the grid, and the pane.
//!
//! Two presentations of one reading, which is why they live together. The strip
//! is a glance while you are watching cameras; the pane is the read. Both take
//! the same [`Model`] from the same poller, so opening the pane costs no
//! request and it updates underneath while it is open.
//!
//! The layout rules are the Roku channel's, because the problem is the same
//! one: three zones each taking a fixed share of the width and laying
//! themselves out inside it, which is what makes the strip hold still. A
//! reading arriving three characters longer than the last one moves nothing but
//! itself, and the middle is a grid rather than a run of text so that values
//! line up under each other whatever they say.
//!
//! One thing is simpler here than there. The channel had to *estimate* how wide
//! a string would be — Roku measures a label only after drawing it — and a
//! third of that file is the estimator and the apology for it. egui lays text
//! out before it paints, so every fit decision below is made against the real
//! width.

use eframe::egui::{self, Color32, Rect, RichText, Stroke, Vec2};

use super::theme;
use crate::config::Preferences;
use crate::weather::poller::RadarInfo;
use crate::weather::tiles::Viewport;
use crate::weather::{clock_text, date_text, Icon, Model, Period};

// ---------------------------------------------------------------- glyphs

/// The sun and the app's accent are the same copper: it is the warmest thing in
/// the palette and the strip is where the palette gets to be warm.
const SUN: Color32 = theme::ACCENT_BRIGHT;
const MOON: Color32 = Color32::from_rgb(0xC7, 0xD3, 0xE0);
const CLOUD: Color32 = Color32::from_rgb(0x9E, 0xAD, 0xBB);
/// The second cloud on an overcast glyph, so two clouds read as two.
const CLOUD_DEEP: Color32 = Color32::from_rgb(0x69, 0x78, 0x86);
const WATER: Color32 = Color32::from_rgb(0x6C, 0xA8, 0xD8);
const FLAKE: Color32 = Color32::from_rgb(0xDA, 0xE8, 0xF4);

/// Draw a condition into a box.
///
/// Drawn rather than shipped as artwork. The Roku channel carries a folder of
/// PNGs because a television draws Posters; this has a painter, so the same
/// twelve glyphs are arcs and strokes that stay sharp whether they are 18px in
/// a forecast row or 90px in the strip.
pub fn paint_icon(painter: &egui::Painter, rect: Rect, icon: Icon) {
    // Square, centred: every glyph is composed against a unit box and a
    // rectangle that is not one would shear the sun into an ellipse.
    let side = rect.width().min(rect.height());
    let box_ = Rect::from_center_size(rect.center(), Vec2::splat(side));

    // Where the cloud sits in glyphs that have one: across the bottom two
    // thirds, leaving the top corner for a sun or a moon to look out of.
    let full_cloud = Rect::from_min_size(
        box_.min + Vec2::new(side * 0.04, side * 0.24),
        Vec2::new(side * 0.92, side * 0.50),
    );
    let tucked_cloud = Rect::from_min_size(
        box_.min + Vec2::new(side * 0.02, side * 0.34),
        Vec2::new(side * 0.78, side * 0.44),
    );

    match icon {
        Icon::ClearDay => sun(painter, box_.center(), side * 0.24),
        Icon::ClearNight => moon(painter, box_.center(), side * 0.28),

        Icon::PartlyDay => {
            sun(
                painter,
                box_.center() + Vec2::new(side * 0.20, -side * 0.20),
                side * 0.17,
            );
            cloud(painter, tucked_cloud, CLOUD);
        }
        Icon::PartlyNight => {
            moon(
                painter,
                box_.center() + Vec2::new(side * 0.22, -side * 0.20),
                side * 0.18,
            );
            cloud(painter, tucked_cloud, CLOUD);
        }

        Icon::Cloudy => {
            // Two clouds, the far one darker, so overcast reads as more than
            // "partly" with the sun left off.
            cloud(
                painter,
                Rect::from_min_size(
                    box_.min + Vec2::new(side * 0.18, side * 0.14),
                    Vec2::new(side * 0.72, side * 0.40),
                ),
                CLOUD_DEEP,
            );
            cloud(painter, tucked_cloud, CLOUD);
        }

        Icon::Rain => {
            cloud(painter, full_cloud, CLOUD);
            drops(painter, box_, side, &[0.30, 0.50, 0.70], side * 0.16, WATER);
        }
        Icon::Showers => {
            cloud(painter, full_cloud, CLOUD);
            // Shorter and more of them: a shower is not steady rain.
            drops(
                painter,
                box_,
                side,
                &[0.26, 0.42, 0.58, 0.74],
                side * 0.10,
                WATER,
            );
        }
        Icon::Storm => {
            cloud(painter, full_cloud, CLOUD_DEEP);
            bolt(painter, box_, side);
        }
        Icon::Snow => {
            cloud(painter, full_cloud, CLOUD);
            flakes(painter, box_, side);
        }
        Icon::Sleet => {
            cloud(painter, full_cloud, CLOUD);
            // A drop and a pellet, alternating — which is what sleet is.
            drops(painter, box_, side, &[0.28, 0.62], side * 0.13, WATER);
            for across in [0.45f32, 0.79] {
                painter.circle_filled(
                    egui::pos2(box_.left() + side * across, box_.top() + side * 0.90),
                    side * 0.048,
                    FLAKE,
                );
            }
        }
        Icon::Fog => {
            cloud(painter, tucked_cloud, CLOUD_DEEP);
            // Bands across the lower half, ragged widths so it reads as haze
            // rather than as a barcode.
            for (index, width) in [0.82f32, 0.66, 0.88].into_iter().enumerate() {
                let y = box_.top() + side * (0.76 + index as f32 * 0.10);
                let inset = (side - side * width) / 2.0;
                painter.line_segment(
                    [
                        egui::pos2(box_.left() + inset, y),
                        egui::pos2(box_.right() - inset, y),
                    ],
                    Stroke::new(side * 0.045, CLOUD),
                );
            }
        }
        Icon::Wind => {
            // Three streaks, each curling back on itself at the end — the
            // convention for moving air on every weather map there has ever
            // been.
            for (index, (length, y)) in [(0.58f32, 0.34f32), (0.74, 0.52), (0.46, 0.70)]
                .into_iter()
                .enumerate()
            {
                let y = box_.top() + side * y;
                let left = box_.left() + side * 0.08;
                let right = left + side * length;
                let stroke = Stroke::new(side * 0.06, if index == 1 { CLOUD } else { CLOUD_DEEP });

                // One polyline, not a line plus a separate arc: the arc has to
                // start exactly where the line ends, and computing that twice
                // is how the first attempt ended up drawing detached rings.
                let curl = side * 0.11;
                let mut points = vec![egui::pos2(left, y), egui::pos2(right, y)];
                points.extend(arc_points(egui::pos2(right, y - curl), curl, -90.0, 170.0));
                painter.add(egui::Shape::line(points, stroke));
            }
        }
    }
}

fn sun(painter: &egui::Painter, centre: egui::Pos2, radius: f32) {
    painter.circle_filled(centre, radius, SUN);
    for step in 0..8 {
        let angle = std::f32::consts::TAU * step as f32 / 8.0;
        let (sin, cos) = angle.sin_cos();
        painter.line_segment(
            [
                centre + Vec2::new(cos, sin) * radius * 1.40,
                centre + Vec2::new(cos, sin) * radius * 1.95,
            ],
            Stroke::new(radius * 0.30, SUN),
        );
    }
}

/// A crescent, laid down as a run of circles that taper to a point at each
/// horn.
///
/// Neither of the obvious ways works here. Punching one circle out of another
/// means painting the bite in the background colour, and these glyphs sit over
/// a panel in one place and a photograph in another — a crescent with a
/// panel-coloured bite only works on a panel. Filling the lune as a polygon
/// means a concave fill, and egui tessellates a closed path as a fan, which is
/// only right for convex shapes.
///
/// So it is built the way the shape actually is: a circle of constant outer
/// radius whose *thickness* is greatest opposite the bite and zero at the two
/// horns. Drawn with a stroke of one width instead, which is what this was
/// first, it reads as the letter C.
fn moon(painter: &egui::Painter, centre: egui::Pos2, radius: f32) {
    const STEPS: usize = 48;
    // The gap faces right. A little under a full turn: too wide a gap is a
    // quarter moon, too narrow is a ring.
    let (from, to) = (58.0f32, 302.0f32);

    for step in 0..=STEPS {
        let along = step as f32 / STEPS as f32;
        let radians = (from + (to - from) * along).to_radians();

        // Thickest halfway round, tapering to nothing at each end.
        let thickness = radius * 0.62 * (std::f32::consts::PI * along).sin();
        if thickness <= 0.0 {
            continue;
        }
        // Centred inside the outer edge rather than on it, so the outer
        // boundary stays a clean circle and only the inner edge tapers.
        let seat = radius - thickness / 2.0;
        painter.circle_filled(
            centre + Vec2::new(radians.cos(), -radians.sin()) * seat,
            thickness / 2.0,
            MOON,
        );
    }
}

/// The points of a circular arc, in screen coordinates (y downwards), from
/// `from` degrees to `to` degrees measured anticlockwise from three o'clock.
fn arc_points(centre: egui::Pos2, radius: f32, from: f32, to: f32) -> Vec<egui::Pos2> {
    let steps = 24;
    (0..=steps)
        .map(|step| {
            let degrees = from + (to - from) * step as f32 / steps as f32;
            let radians = degrees.to_radians();
            centre + Vec2::new(radians.cos(), -radians.sin()) * radius
        })
        .collect()
}

/// Three puffs sitting on a slab. Every piece is convex, which is what lets
/// this be four fills rather than a tessellated outline.
///
/// Each puff is placed by its *bottom* rather than its centre — tangent to the
/// base line, so the flat underside a cloud has is the slab and nothing bulges
/// through it. Placing them by centre is how the first attempt came out looking
/// like a clover: the widest puff is nearly as tall as the whole glyph, and
/// half of it hung below the cloud.
fn cloud(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let (w, h) = (rect.width(), rect.height());
    let base = rect.bottom();

    let slab = Rect::from_min_max(
        egui::pos2(rect.left() + w * 0.06, base - h * 0.30),
        egui::pos2(rect.right() - w * 0.06, base),
    );
    painter.rect_filled(slab, (h * 0.14) as u8, colour);

    // Left, middle, right: the middle one tallest, so the top edge reads as a
    // cloud rather than as a row of equal bumps.
    for (centre, radius) in [(0.23f32, 0.30f32), (0.48, 0.44), (0.75, 0.32)] {
        let radius = h * radius;
        painter.circle_filled(
            egui::pos2(rect.left() + w * centre, base - radius),
            radius,
            colour,
        );
    }
}

/// Falling water, slanted the way rain falls in a wind.
///
/// Placed at named positions rather than spread evenly across a count, because
/// sleet interleaves pellets between them and "evenly spaced" in two separate
/// calls does not interleave — it overlaps.
fn drops(painter: &egui::Painter, box_: Rect, side: f32, at: &[f32], length: f32, colour: Color32) {
    let stroke = Stroke::new(side * 0.055, colour);
    for (index, across) in at.iter().enumerate() {
        let x = box_.left() + side * across;
        // Staggered, so they do not read as a comb.
        let top = box_.top() + side * (0.80 + if index % 2 == 0 { 0.0 } else { 0.06 });
        painter.line_segment(
            [
                egui::pos2(x + length * 0.28, top),
                egui::pos2(x - length * 0.28, top + length),
            ],
            stroke,
        );
    }
}

fn flakes(painter: &egui::Painter, box_: Rect, side: f32) {
    let stroke = Stroke::new(side * 0.045, FLAKE);
    for (index, x) in [0.30f32, 0.52, 0.74].into_iter().enumerate() {
        let centre = egui::pos2(
            box_.left() + side * x,
            box_.top() + side * (0.88 + if index == 1 { 0.06 } else { 0.0 }),
        );
        let arm = side * 0.075;
        for step in 0..3 {
            let angle = std::f32::consts::PI * step as f32 / 3.0;
            let (sin, cos) = angle.sin_cos();
            let reach = Vec2::new(cos, sin) * arm;
            painter.line_segment([centre - reach, centre + reach], stroke);
        }
    }
}

fn bolt(painter: &egui::Painter, box_: Rect, side: f32) {
    // A zigzag drawn as a thick line rather than filled as a polygon: a bolt is
    // concave, and egui fills convex paths.
    let points: Vec<egui::Pos2> = [(0.58f32, 0.72f32), (0.40, 0.90), (0.56, 0.90), (0.40, 1.02)]
        .into_iter()
        .map(|(x, y)| egui::pos2(box_.left() + side * x, box_.top() + side * y))
        .collect();
    painter.add(egui::Shape::line(
        points,
        Stroke::new(side * 0.075, theme::WARN),
    ));
}

// ---------------------------------------------------------------- placement

/// A proportion of a height, held between a floor and a ceiling.
fn size_for(height: f32, factor: f32, least: f32, most: f32) -> f32 {
    (height * factor).clamp(least, most)
}

/// Places one line and returns where the next one starts.
///
/// A line with nothing to say, or one that would run past the bottom it was
/// given, is not drawn — dropping a reading whole beats truncating it in the
/// middle, and the pane has all of them anyway. What *does* overflow its own
/// column is ellipsised by egui rather than clipped mid-glyph.
fn place_line(
    painter: &egui::Painter,
    text: &str,
    at: egui::Pos2,
    width: f32,
    size: f32,
    colour: Color32,
    bottom: f32,
    gap: f32,
) -> f32 {
    if text.is_empty() {
        return at.y;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        egui::FontId::proportional(size),
        colour,
    );
    job.wrap.max_width = width.max(1.0);
    job.wrap.max_rows = 1;

    let galley = painter.layout_job(job);
    if at.y + galley.size().y > bottom {
        return at.y;
    }
    let height = galley.size().y;
    painter.galley(at, galley, colour);
    at.y + height + gap
}

/// The same, right-aligned in a box of the given width.
///
/// Aligned in a *box* rather than at a point, because a value that grows
/// leftwards with nothing to stop it is how "4 mph NE, gusting 22 mph" came to
/// be drawn back through the word "Wind" in the channel this is ported from.
fn place_right(
    painter: &egui::Painter,
    text: &str,
    right: f32,
    y: f32,
    width: f32,
    size: f32,
    colour: Color32,
) {
    if text.is_empty() {
        return;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        egui::FontId::proportional(size),
        colour,
    );
    job.wrap.max_width = width.max(1.0);
    job.wrap.max_rows = 1;
    job.halign = egui::Align::RIGHT;

    let galley = painter.layout_job(job);
    painter.galley(egui::pos2(right, y), galley, colour);
}

/// How fast a running line travels, and how much blank follows it, both as
/// multiples of the type size rather than as pixels.
///
/// Derived from the size so the strip reads the same at every height. The wall
/// this is built for is 4K and sizes its type off the strip's height, so a
/// speed in pixels per second would be a sedate crawl there and a blur on a
/// laptop.
const MARQUEE_SPEED: f32 = 7.0;
const MARQUEE_GAP: f32 = 6.0;
/// How long the line holds at its start before running, in seconds.
///
/// Long enough to read the first alert without waiting for a lap. A ticker that
/// is already moving when you look up has to be waited out from wherever it
/// happens to be, and the first alert is the most serious one — [`Model::
/// alerts_line`] puts them in the order the map paints them.
const MARQUEE_DWELL: f64 = 2.5;

/// How far a running line has travelled at a given moment.
///
/// Split out from the drawing so the arithmetic can be tested. The wrap is the
/// part most likely to be wrong, and one that is a frame short shows a band of
/// empty strip the width of the gap, once a lap, forever.
///
/// The cycle is a dwell followed by a single run of `travel` pixels, where
/// `travel` is the text plus the blank that follows it. At the end of the run
/// the trailing copy stands exactly where the leading one started, so resetting
/// to zero is invisible and the line runs continuously.
fn marquee_offset(now: f64, travel: f32, speed: f32, dwell: f64) -> f32 {
    if !(travel > 0.0) || !(speed > 0.0) {
        return 0.0;
    }
    let run = f64::from(travel) / f64::from(speed);
    let elapsed = now.rem_euclid(dwell + run) - dwell;
    if elapsed <= 0.0 {
        return 0.0;
    }
    // Clamped rather than trusted: a clock far enough along that the
    // multiplication loses its last places should stop at the wrap rather than
    // step past it and pull the seam into view.
    ((elapsed * f64::from(speed)) as f32).clamp(0.0, travel)
}

/// Places a line that runs when it is too wide to show, and holds still when it
/// is not.
///
/// A watch and a warning out at once is exactly when the width stops being
/// enough, and exactly when ellipsising is worst: "Tornado Watch  ·  Severe
/// Thunderstorm W…" is a strip that has cut off the half you needed. So the
/// line travels instead, the way the Roku channel's does, and everything on it
/// comes past.
///
/// A line that fits is drawn precisely where [`place_line`] would put it and
/// never moves. Movement on a camera wall is expensive — it takes the eye off
/// the pictures — so it has to mean *there is more here than fits*, not merely
/// that there is weather.
fn place_scrolling_line(
    painter: &egui::Painter,
    text: &str,
    at: egui::Pos2,
    width: f32,
    size: f32,
    colour: Color32,
    bottom: f32,
) {
    if text.is_empty() {
        return;
    }
    let galley = painter.layout_no_wrap(text.to_owned(), egui::FontId::proportional(size), colour);
    // The rule [`place_line`] follows, for the same reason: a line that would
    // run past the bottom of the strip is dropped whole rather than shown with
    // its descenders sliced off.
    if at.y + galley.size().y > bottom {
        return;
    }

    let travel = galley.size().x + size * MARQUEE_GAP;
    if galley.size().x <= width {
        painter.galley(at, galley, colour);
        return;
    }

    let offset = marquee_offset(
        painter.ctx().input(|i| i.time),
        travel,
        size * MARQUEE_SPEED,
        MARQUEE_DWELL,
    );

    // Clipped to its own column. The strip holds still because its three zones
    // each keep their share of the width, and an alert sliding out from under
    // the forecast and across the clock would be the one thing that undoes
    // that.
    let column = Rect::from_min_max(
        egui::pos2(at.x, painter.clip_rect().top()),
        egui::pos2(at.x + width, painter.clip_rect().bottom()),
    );
    let painter = painter.with_clip_rect(painter.clip_rect().intersect(column));
    painter.galley(egui::pos2(at.x - offset, at.y), galley.clone(), colour);
    // The trailing copy, which is what makes the wrap seamless: it is off the
    // right edge for most of the lap and arrives at the start exactly as the
    // leading one leaves.
    painter.galley(egui::pos2(at.x - offset + travel, at.y), galley, colour);

    // Nothing else on the strip moves, so the wall is otherwise redrawn only
    // when a frame or a reading arrives. Ask for what this needs, capped:
    // sixty a second is as fluid as the display, and an unqualified request
    // would have an idle machine repainting the whole wall as fast as it can.
    painter
        .ctx()
        .request_repaint_after(std::time::Duration::from_millis(16));
}

fn text_width(painter: &egui::Painter, text: &str, size: f32) -> f32 {
    painter
        .layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(size),
            theme::TEXT,
        )
        .size()
        .x
}

// ---------------------------------------------------------------- the view

/// Which half of the weather pane is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneTab {
    Conditions,
    Radar,
}

/// Everything the weather draws with, and the little state it keeps.
///
/// The reading itself lives in the poller; what is here is which reading is
/// currently the best one to show, and which forecast period the pane has
/// selected.
pub struct WeatherView {
    /// The last reading worth showing. A failed poll does not clear it: the
    /// last reading is still the best answer available, and a station that has
    /// gone quiet for a few minutes is ordinary. It is said in the corner
    /// rather than by blanking the numbers.
    model: std::sync::Arc<Model>,
    has_data: bool,
    stale: bool,
    note: String,
    period: usize,
    pub showing: PaneTab,
    /// Set when the radar asked to be left - a double-click on the map, or the
    /// button that says so. Read and cleared by the shell, which owns the tabs.
    leaving_for_cameras: bool,
    pub radar: super::radar::RadarView,
}

impl Default for WeatherView {
    fn default() -> Self {
        WeatherView {
            model: std::sync::Arc::new(Model::empty()),
            has_data: false,
            stale: false,
            // The note names neither source: the strip is handed a model and is
            // never told which one filled it in, and "waiting for the weather"
            // is true of both.
            note: "Waiting for the weather…".into(),
            period: 0,
            showing: PaneTab::Conditions,
            leaving_for_cameras: false,
            radar: super::radar::RadarView::default(),
        }
    }
}

impl WeatherView {
    /// Take in whatever the poller last got.
    ///
    /// Called every frame with the same reading for minutes at a time, so it
    /// has to be cheap and idempotent: taking the `Arc` is a refcount bump, and
    /// everything else here only writes when the answer changed.
    pub fn absorb(&mut self, incoming: std::sync::Arc<Model>) {
        if incoming.ok {
            self.model = incoming;
            self.has_data = true;
            self.stale = false;
            self.note.clear();
            self.period = self.period.min(self.model.periods.len().saturating_sub(1));
        } else {
            self.stale = true;
            if self.note != incoming.error {
                self.note.clone_from(&incoming.error);
            }
            if !self.has_data {
                self.model = incoming;
            }
        }
    }

    /// Forget the reading, for when the weather is switched off — otherwise
    /// turning it back on would flash the last reading from an hour ago.
    pub fn reset(&mut self) {
        *self = WeatherView::default();
    }

    /// Whether the radar asked to be left. Taking it clears it.
    pub fn take_leaving(&mut self) -> bool {
        std::mem::take(&mut self.leaving_for_cameras)
    }

    /// The forecast, for the wall to put in the cells the cameras left spare.
    ///
    /// Empty until a reading has landed. A wall that has never had one is a
    /// wall of cameras and nothing else, rather than a row of blank cells
    /// waiting to be filled in.
    pub fn periods(&self) -> &[Period] {
        if self.has_data {
            &self.model.periods
        } else {
            &[]
        }
    }

    /// Why there is no forecast yet, for a cell promised one — empty while it
    /// is simply still coming.
    pub fn waiting_note(&self) -> &str {
        if self.has_data {
            ""
        } else {
            &self.note
        }
    }

    /// Open the pane on one period, for a click on its tile out on the wall.
    pub fn show_period(&mut self, index: usize) {
        self.showing = PaneTab::Conditions;
        self.period = index.min(self.model.periods.len().saturating_sub(1));
    }

    // ------------------------------------------------------------ the strip

    /// The strip over the camera grid: three zones, left to right — what it is
    /// doing now, what the station is reading, and what day it is.
    pub fn bar(&self, ui: &mut egui::Ui, rect: Rect, prefs: &Preferences) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, theme::PANEL);
        painter.line_segment(
            [
                egui::pos2(rect.left(), rect.bottom() - 0.5),
                egui::pos2(rect.right(), rect.bottom() - 0.5),
            ],
            Stroke::new(1.0_f32, theme::BORDER_SOFT),
        );

        let (w, h) = (rect.width(), rect.height());
        let pad = 18.0;

        // The right zone is sized to hold the longest day this can print —
        // "Wednesday 30 September" — because it is pinned to the right edge and
        // everything else is measured back from it.
        let right_width = (w * 0.17).clamp(150.0, 260.0);
        let right_x = rect.right() - pad - right_width;
        let middle_x = rect.left() + (w * 0.30).max(200.0);
        let middle_right = right_x - w * 0.02;

        self.bar_left(&painter, rect, pad, middle_x - 16.0, h, prefs);
        self.bar_middle(&painter, rect, middle_x, middle_right, h, prefs);
        self.bar_right(&painter, rect, right_x, right_width, h, prefs);
    }

    /// The glyph and the temperature on one line, the conditions under the
    /// temperature, and whose reading this is under both.
    fn bar_left(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        pad: f32,
        edge: f32,
        h: f32,
        prefs: &Preferences,
    ) {
        let temp_size = size_for(h, 0.345, 22.0, 56.0);
        let summary_size = size_for(h, 0.135, 12.0, 20.0);
        let station_size = size_for(h, 0.105, 10.5, 15.0);
        let icon_size = h * 0.43;

        let top = rect.top() + h * 0.09;
        let x = rect.left() + pad;
        let mut text_x = x;

        if self.has_data {
            if let Some(icon) = self.model.icon {
                // Centred against the temperature beside it rather than sharing
                // its top edge: the glyph is square and the type is not.
                let box_ = Rect::from_min_size(
                    egui::pos2(x, top + (temp_size * 1.3 - icon_size) / 2.0),
                    Vec2::splat(icon_size),
                );
                paint_icon(painter, box_, icon);
                text_x = x + icon_size + icon_size * 0.18;
            }
        }

        if self.has_data {
            let y = place_line(
                painter,
                &self.model.temp_big,
                egui::pos2(text_x, top),
                edge - text_x,
                temp_size,
                theme::TEXT,
                rect.bottom(),
                2.0,
            );
            place_line(
                painter,
                &self.model.summary,
                egui::pos2(text_x, y),
                edge - text_x,
                summary_size,
                theme::ACCENT_BRIGHT,
                rect.bottom(),
                0.0,
            );
        } else {
            // Nothing has arrived yet: say so where the temperature would be,
            // and draw none of the furniture that would otherwise frame empty
            // values.
            let message = if self.note.is_empty() {
                "Weather unavailable"
            } else {
                &self.note
            };
            place_line(
                painter,
                message,
                egui::pos2(text_x, top + (temp_size * 1.3 - summary_size * 1.3) / 2.0),
                edge - text_x,
                summary_size,
                theme::TEXT_DIM,
                rect.bottom(),
                0.0,
            );
        }

        // Flush with the glyph rather than with the temperature: it belongs to
        // the whole block, not to the reading.
        let station = if self.model.station.is_empty() {
            prefs.weather_description()
        } else {
            self.model.station.clone()
        };
        place_line(
            painter,
            &station,
            egui::pos2(x, rect.bottom() - h * 0.11 - station_size * 1.3),
            edge - x,
            station_size,
            theme::TEXT_DIM,
            rect.bottom(),
            0.0,
        );
    }

    /// The readings on a two-column grid, and what is coming across the foot of
    /// them.
    fn bar_middle(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        x: f32,
        edge: f32,
        h: f32,
        prefs: &Preferences,
    ) {
        let value_size = size_for(h, 0.118, 11.5, 18.0);
        let label_size = size_for(h, 0.092, 10.0, 15.0);
        let outlook_size = size_for(h, 0.105, 10.5, 16.0);

        let width = edge - x;
        let pair_width = width * 0.46;
        let column_step = width * 0.5;

        let top = rect.top() + h * 0.10;
        let pitch = value_size * 1.3 + h * 0.04;
        const ROWS: usize = 3;

        // The foot is the forecast's, and the grid stops above it.
        let outlook_y = rect.bottom() - h * 0.10 - outlook_size * 1.3;
        let bottom = outlook_y - 6.0;

        if self.has_data {
            for (slot, (name, value)) in self.model.stats().into_iter().take(ROWS * 2).enumerate() {
                let (column, row) = (slot / ROWS, slot % ROWS);
                let y = top + row as f32 * pitch;
                if y + value_size * 1.3 > bottom {
                    continue;
                }
                let column_x = x + column as f32 * column_step;

                // The label takes what it needs and the value takes the rest of
                // the pair, right-aligned in it.
                let name_width = text_width(painter, name, label_size);
                // Riding down to sit on the value's line, since the two are
                // different sizes and a shared top edge would leave the smaller
                // one floating.
                let name_y = y + (value_size - label_size) * 1.3 / 2.0;
                place_line(
                    painter,
                    name,
                    egui::pos2(column_x, name_y),
                    name_width,
                    label_size,
                    theme::TEXT_DIM,
                    rect.bottom(),
                    0.0,
                );
                place_right(
                    painter,
                    value,
                    column_x + pair_width,
                    y,
                    pair_width - name_width - 12.0,
                    value_size,
                    theme::TEXT,
                );
            }
        }

        self.bar_outlook(painter, rect, x, outlook_y, width, outlook_size, prefs);
    }

    /// What is coming, or what is already out.
    ///
    /// An alert takes the forecast's place rather than sharing the row with it:
    /// the two are the same shape on screen, and a warning that reads as a
    /// forecast is worse than no warning at all.
    ///
    /// The channel scrolled this line, because a television has no other way to
    /// show text that does not fit. It turns out neither does a wall: the strip
    /// is glanced at from across a room, where a hover is not available and an
    /// ellipsis is simply the rest of the sentence gone. So this scrolls too,
    /// and only when it has to — see [`place_scrolling_line`]. The forecast
    /// below it does not: it is one clause, it is written to fit, and the pane
    /// has it in full.
    fn bar_outlook(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        x: f32,
        y: f32,
        width: f32,
        size: f32,
        prefs: &Preferences,
    ) {
        if !self.has_data {
            return;
        }

        if !self.model.alerts.is_empty() {
            let text = self.model.alerts_line(prefs.clock_is_24_hour());
            if !text.is_empty() {
                let colour = if self.model.alerts_severe() {
                    theme::ERROR
                } else {
                    theme::WARN
                };
                painter.rect_filled(
                    Rect::from_min_size(egui::pos2(x, y), Vec2::new(4.0, size * 1.3)),
                    1,
                    colour,
                );
                place_scrolling_line(
                    painter,
                    &text,
                    egui::pos2(x + 12.0, y),
                    width - 12.0,
                    size,
                    theme::TEXT,
                    rect.bottom(),
                );
                return;
            }
        }

        // The same glyph the forecast rows carry, at the size of the line it
        // sits on, so the strip and the pane are speaking the same language.
        let mut text_x = x;
        if let Some(icon) = self.model.periods.first().and_then(|p| p.icon) {
            let glyph = size * 1.5;
            paint_icon(
                painter,
                Rect::from_min_size(egui::pos2(x, y - glyph * 0.14), Vec2::splat(glyph)),
                icon,
            );
            text_x = x + glyph + 8.0;
        }

        let text = match (
            self.model.outlook_name.is_empty(),
            self.model.outlook_text.is_empty(),
        ) {
            (false, false) => format!("{} — {}", self.model.outlook_name, self.model.outlook_text),
            (true, false) => self.model.outlook_text.clone(),
            _ => self.model.outlook_name.clone(),
        };
        place_line(
            painter,
            &text,
            egui::pos2(text_x, y),
            width - (text_x - x),
            size,
            theme::TEXT,
            rect.bottom(),
            0.0,
        );
    }

    /// The day, the time, and — only while there is something to say about it —
    /// when the reading was taken.
    fn bar_right(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        x: f32,
        width: f32,
        h: f32,
        prefs: &Preferences,
    ) {
        let date_size = size_for(h, 0.105, 10.5, 16.0);
        let time_size = size_for(h, 0.155, 14.0, 26.0);
        let stale_size = size_for(h, 0.092, 10.0, 14.0);
        let twenty_four = prefs.clock_is_24_hour();

        let now = chrono::Local::now();
        let clock = crate::weather::Clock {
            hours: chrono::Timelike::hour(&now),
            minutes: chrono::Timelike::minute(&now),
        };

        let mut y = rect.top() + h * 0.16;
        y = place_line(
            painter,
            &date_text(now),
            egui::pos2(x, y),
            width,
            date_size,
            theme::TEXT_DIM,
            rect.bottom(),
            2.0,
        );
        y = place_line(
            painter,
            &clock_text(clock, twenty_four),
            egui::pos2(x, y),
            width,
            time_size,
            theme::TEXT,
            rect.bottom(),
            2.0,
        );

        // The reading's own timestamp, and only while it is not the same story
        // the clock above is telling. A strip of numbers with nothing saying
        // they have stopped moving is the one way this can mislead; a strip
        // saying "22:42" under a clock reading 22:47 is noise.
        if self.stale {
            let text = match self.model.observed {
                Some(observed) => {
                    format!("Last reading {}", clock_text(observed, twenty_four))
                }
                None => "Not updating".to_string(),
            };
            place_line(
                painter,
                &text,
                egui::pos2(x, y),
                width,
                stale_size,
                theme::WARN,
                rect.bottom(),
                0.0,
            );
        }
    }

    // ------------------------------------------------------------ the pane

    /// Everything the reading has, on one pane.
    ///
    /// Returns the size the radar wants its layers at, when the radar is the
    /// half showing — the caller reconciles the poller against it. Quantised,
    /// so dragging a window edge does not restart a fetch on every frame.
    pub fn pane(
        &mut self,
        ui: &mut egui::Ui,
        prefs: &Preferences,
        info: Option<&RadarInfo>,
        view: &mut Viewport,
        home: Viewport,
    ) -> bool {
        let radar_available = prefs.radar_usable();
        if !radar_available && self.showing == PaneTab::Radar {
            self.showing = PaneTab::Conditions;
        }

        self.pane_header(ui, prefs, radar_available);
        ui.add_space(10.0);

        if self.showing == PaneTab::Radar {
            let rect = ui.available_rect_before_wrap();
            let default = RadarInfo::default();
            let asked = self.radar.show(
                ui,
                rect,
                info.unwrap_or(&default),
                &prefs.radar_basemap,
                prefs.clock_is_24_hour(),
                prefs.weather_metric,
                view,
                home,
                false,
            );
            if asked == super::radar::Asked::ShowCameras {
                self.leaving_for_cameras = true;
            }
            return true;
        }

        self.radar.release();
        if !self.has_data {
            let message = if self.note.is_empty() {
                "Weather unavailable".to_string()
            } else {
                format!("Weather unavailable — {}", self.note)
            };
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new(message).size(14.0).color(theme::PLACEHOLDER));
            });
            return false;
        }

        self.pane_alerts(ui, prefs);
        ui.add_space(6.0);

        // The reading down the left, the forecast down the right: the left is a
        // fixed set of rows and the right is as long as the service wrote it.
        //
        // Both columns are given an explicit size and an explicit top-down
        // layout. A child allocated inside a horizontal layout *inherits* that
        // layout, so without saying so the left column lays itself out sideways
        // — the temperature, then the readings beside it, then the source line
        // beside those.
        let area = ui.available_rect_before_wrap();
        let left_width = (area.width() * 0.38).clamp(260.0, 460.0);
        const GUTTER: f32 = 20.0;

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(left_width, area.height()),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| self.pane_current(ui, prefs),
            );
            ui.add_space(GUTTER);
            ui.allocate_ui_with_layout(
                Vec2::new((area.width() - left_width - GUTTER).max(200.0), area.height()),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| self.pane_forecast(ui, area.height()),
            );
        });
        false
    }

    fn pane_header(&mut self, ui: &mut egui::Ui, prefs: &Preferences, radar_available: bool) {
        ui.horizontal(|ui| {
            let title = if self.model.station.is_empty() {
                "Weather".to_string()
            } else {
                self.model.station.clone()
            };
            ui.label(RichText::new(title).size(19.0).strong().color(theme::TEXT));

            if let Some(observed) = self.model.observed {
                let when = clock_text(observed, prefs.clock_is_24_hour());
                let label = if self.stale {
                    RichText::new(format!("Last reading {when}")).color(theme::WARN)
                } else {
                    RichText::new(format!("Observed at {when}")).color(theme::TEXT_DIM)
                };
                ui.label(label.size(12.0));
            } else if self.stale && !self.note.is_empty() {
                ui.label(
                    RichText::new(&self.note)
                        .size(12.0)
                        .color(theme::WARN),
                );
            }

            if radar_available {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (label, tab) in
                        [("Radar", PaneTab::Radar), ("Conditions", PaneTab::Conditions)]
                    {
                        if ui
                            .selectable_label(self.showing == tab, label)
                            .clicked()
                        {
                            self.showing = tab;
                        }
                    }
                });
            }
        });
    }

    /// Every alert that is out, each in full, stacked.
    ///
    /// The strip runs them together on one line because that is all the room it
    /// has. Here they get a block each: three watches out at once is precisely
    /// when the second and third are worth reading. Bounded all the same —
    /// past four the stack would push the reading off the pane, and the ones
    /// dropped are the mildest rather than the only.
    fn pane_alerts(&self, ui: &mut egui::Ui, prefs: &Preferences) {
        if self.model.alerts.is_empty() {
            return;
        }
        let shown = self.model.alerts.len().min(4);

        for (index, alert) in self.model.alerts.iter().take(shown).enumerate() {
            let colour = if alert.is_severe() {
                theme::ERROR
            } else {
                theme::WARN
            };
            let mut event = alert.event.clone();
            if index + 1 == shown && self.model.alerts.len() > shown {
                event.push_str(&format!(
                    "   (+{} more)",
                    self.model.alerts.len() - shown
                ));
            }

            let mut detail = alert.detail();
            if !alert.ends.is_empty() && detail != alert.ends {
                if !detail.is_empty() {
                    detail.push_str("  ·  ");
                }
                detail.push_str(&format!("until {}", alert.ends));
            }

            egui::Frame::NONE
                .fill(theme::PANEL_ALT)
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(12, 9))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let top = ui.min_rect().top();
                    ui.label(RichText::new(event).size(14.0).strong().color(theme::TEXT));
                    if !detail.is_empty() {
                        ui.label(RichText::new(detail).size(12.0).color(theme::TEXT_DIM));
                    }
                    // The severity stripe, drawn down the whole block after its
                    // height is known.
                    let bottom = ui.min_rect().bottom();
                    ui.painter().rect_filled(
                        Rect::from_min_max(
                            egui::pos2(ui.min_rect().left() - 12.0, top - 9.0),
                            egui::pos2(ui.min_rect().left() - 7.0, bottom + 9.0),
                        ),
                        1,
                        colour,
                    );
                });
            ui.add_space(4.0);
        }
        let _ = prefs;
    }

    /// The temperature, what it is doing, and the readings the strip has no
    /// room for.
    fn pane_current(&self, ui: &mut egui::Ui, prefs: &Preferences) {
        ui.horizontal(|ui| {
            if let Some(icon) = self.model.icon {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(84.0), egui::Sense::hover());
                paint_icon(ui.painter(), rect, icon);
                ui.add_space(6.0);
            }
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(&self.model.temp_big)
                        .size(46.0)
                        .color(theme::TEXT),
                );
                ui.label(
                    RichText::new(&self.model.summary)
                        .size(15.0)
                        .color(theme::ACCENT_BRIGHT),
                );
                if !self.model.feels.is_empty() {
                    ui.label(
                        RichText::new(&self.model.feels)
                            .size(13.0)
                            .color(theme::TEXT_DIM),
                    );
                }
            });
        });

        ui.add_space(14.0);
        egui::Grid::new("weather-stats")
            .num_columns(2)
            .spacing([16.0, 7.0])
            .show(ui, |ui| {
                for (name, value) in self.model.detailed_stats() {
                    ui.label(RichText::new(name).size(13.0).color(theme::TEXT_DIM));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(value).size(13.0).color(theme::TEXT));
                    });
                    ui.end_row();
                }
            });

        ui.add_space(12.0);
        ui.label(
            RichText::new(prefs.weather_description())
                .size(11.0)
                .color(theme::PLACEHOLDER),
        );
    }

    /// One row per forecast period, with the full wording of the selected one
    /// underneath.
    ///
    /// Eight periods of three sentences each will not fit on a pane at a
    /// readable size, and the short form is what the list is for.
    ///
    /// `height` is passed in rather than read off the `Ui`: the column is an
    /// allocated child, and asking it how much room is left gives the room left
    /// in the row rather than the height of the pane.
    fn pane_forecast(&mut self, ui: &mut egui::Ui, height: f32) {
        if self.model.periods.is_empty() {
            ui.label(
                RichText::new("No forecast in this reading")
                    .size(12.0)
                    .color(theme::PLACEHOLDER),
            );
            return;
        }

        // The narrative gets a fixed share of the bottom, so the list does not
        // resize every time a longer period is selected.
        let narrative_height = (height * 0.28).clamp(60.0, 160.0);
        let list_height = (height - narrative_height - 26.0).max(80.0);

        let mut chosen = self.period;
        egui::ScrollArea::vertical()
            .max_height(list_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, period) in self.model.periods.iter().enumerate() {
                    if row(ui, period, index == self.period) {
                        chosen = index;
                    }
                }
            });
        self.period = chosen.min(self.model.periods.len() - 1);

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        let detail = self
            .model
            .periods
            .get(self.period)
            .map(|p| p.detailed.clone())
            .unwrap_or_default();
        egui::ScrollArea::vertical()
            .id_salt("weather-narrative")
            .max_height(narrative_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.label(RichText::new(detail).size(13.0).color(theme::TEXT_DIM));
            });
    }
}

/// One forecast period, in a cell the cameras left spare.
///
/// A wall is tiled into a square-ish grid, so unless the camera count divides
/// into it the last row ends short: five cameras make a 3x2 with one cell
/// spare, seven make a 3x3 with two. Those cells were empty. They now take the
/// front of the forecast, one period per cell, in the order the service sends
/// them — so the spare cell on a five-camera wall is tonight, and a
/// seven-camera wall gets tonight and tomorrow.
///
/// Nothing is displaced to make room and nothing is added to make a period fit:
/// where the cameras tile exactly there are no forecast tiles, and what is
/// coming next is in the strip at the top on every wall regardless.
///
/// Drawn with the camera tiles' own name strip, at the camera tiles' own
/// height, because it stands in a row of them — see
/// [`super::tile::TITLE_HEIGHT`]. Everything under the strip is sized off the
/// cell, which can be a quarter of the screen or a sixteenth of it, and
/// anything that would run past the bottom is dropped rather than clipped.
///
/// Clicking one opens the Weather tab on that period, with the full wording the
/// cell has no room for.
pub fn forecast_tile(ui: &mut egui::Ui, rect: Rect, period: &Period) -> egui::Response {
    let response = ui
        .allocate_rect(rect, egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0, theme::INK);

    let bar = Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width(), super::tile::TITLE_HEIGHT),
    );
    // A camera lights its whole strip when it is selected; there is nothing to
    // select here, so the strip does the same thing under the pointer instead —
    // which is the only thing on the wall saying the cell can be opened.
    painter.rect_filled(
        bar,
        0,
        if response.hovered() {
            theme::ACCENT_DEEP
        } else {
            theme::PANEL
        },
    );
    painter.text(
        bar.left_center() + Vec2::new(6.0, 0.0),
        egui::Align2::LEFT_CENTER,
        &period.name,
        egui::FontId::proportional(12.0),
        theme::TEXT,
    );

    let (w, h) = (rect.width(), rect.height());
    let pad = if w < 260.0 { 10.0 } else { 16.0 };
    let left = rect.left() + pad;
    let width = w - pad * 2.0;
    let bottom = rect.bottom() - pad;
    let mut y = bar.bottom() + pad;

    // Sized off the cell rather than fixed: a spare cell on a two-camera wall
    // is half the screen and one on a sixteen-camera wall is a postage stamp.
    let glyph = (h * 0.24).clamp(22.0, 72.0);
    let big = (h * 0.13).clamp(15.0, 34.0);
    let small = (h * 0.055).clamp(10.0, 15.0);

    // The glyph with the temperature beside it on the same optical line. It is
    // the first thing read from across a room and the last thing that should be
    // dropped, so it goes in before the fit rules start hiding anything.
    let mut text_x = left;
    let mut text_width = width;
    let drawn_glyph = match period.icon {
        Some(icon) if y + glyph <= bottom => {
            paint_icon(
                &painter,
                Rect::from_min_size(egui::pos2(left, y), Vec2::splat(glyph)),
                icon,
            );
            text_x = left + glyph + glyph * 0.18;
            text_width = rect.right() - pad - text_x;
            glyph
        }
        _ => 0.0,
    };

    let temp_top = y + ((drawn_glyph - big * 1.2) / 2.0).max(0.0);
    place_line(
        &painter,
        &period.temperature,
        egui::pos2(text_x, temp_top),
        text_width,
        big,
        theme::TEXT,
        bottom,
        0.0,
    );
    y = if drawn_glyph > 0.0 {
        y + drawn_glyph + 8.0
    } else {
        temp_top + big * 1.2 + 6.0
    };

    // The narrative gets whatever height is left, up to three lines, so a tall
    // cell says "Patchy fog before 9am, then sunny" and a short one says
    // nothing rather than half of it.
    let line = small * 1.3;
    let reserved = if period.precip.is_empty() { 0.0 } else { line + 4.0 };
    let mut rows = 3usize;
    while rows > 0 && y + line * rows as f32 > bottom - reserved {
        rows -= 1;
    }
    if rows > 0 && !period.short.is_empty() {
        let mut job = egui::text::LayoutJob::simple(
            period.short.clone(),
            egui::FontId::proportional(small),
            theme::TEXT_DIM,
            width.max(1.0),
        );
        job.wrap.max_rows = rows;
        let galley = painter.layout_job(job);
        let used = galley.size().y;
        painter.galley(egui::pos2(left, y), galley, theme::TEXT_DIM);
        y += used + 4.0;
    }

    place_line(
        &painter,
        &period.precip,
        egui::pos2(left, y),
        width,
        small,
        theme::ACCENT_BRIGHT,
        bottom,
        0.0,
    );

    response
}

/// A cell the forecast has been promised, before there is a reading to put in
/// it.
///
/// Only reachable with the forecast set to always take cells: the wall's shape
/// is chosen from that promise, so the cells exist from the first frame and a
/// reading is a minute away at worst. Drawn rather than left black because an
/// empty cell in a row of cameras reads as a camera that has failed, and it
/// keeps the wall still — filling the cells in later is a picture arriving,
/// while relaying the whole wall around them is the grid jumping under
/// somebody watching it.
pub fn forecast_waiting(ui: &mut egui::Ui, rect: Rect, note: &str) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0, theme::INK);
    painter.rect_filled(
        Rect::from_min_size(rect.min, Vec2::new(rect.width(), super::tile::TITLE_HEIGHT)),
        0,
        theme::PANEL,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        if note.is_empty() { "Waiting for the forecast…" } else { note },
        egui::FontId::proportional(12.0),
        theme::PLACEHOLDER,
    );
}

/// One forecast row. Returns whether it was clicked.
fn row(ui: &mut egui::Ui, period: &Period, selected: bool) -> bool {
    const HEIGHT: f32 = 62.0;

    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), HEIGHT),
        egui::Sense::click(),
    );
    let painter = ui.painter_at(rect);
    let body = rect.shrink2(Vec2::new(0.0, 3.0));

    painter.rect_filled(
        body,
        6,
        if selected {
            theme::PANEL_ALT
        } else if response.hovered() {
            theme::PANEL
        } else {
            theme::INK
        },
    );
    if selected {
        painter.rect_filled(
            Rect::from_min_size(body.min, Vec2::new(4.0, body.height())),
            2,
            theme::ACCENT,
        );
    }

    let mut x = body.left() + 14.0;
    if let Some(icon) = period.icon {
        let glyph = 34.0;
        paint_icon(
            &painter,
            Rect::from_min_size(
                egui::pos2(x, body.center().y - glyph / 2.0),
                Vec2::splat(glyph),
            ),
            icon,
        );
        x += glyph + 10.0;
    }

    // The period and its temperature on the first line, the summary across most
    // of the second. The service writes summaries like "Showers And
    // Thunderstorms Likely then Chance Showers And Thunderstorms", which needs
    // the width of the row rather than a column beside the temperature.
    let right = body.right() - 14.0;
    let temp_width = 120.0;
    place_line(
        &painter,
        &period.name,
        egui::pos2(x, body.top() + 8.0),
        right - x - temp_width - 8.0,
        14.0,
        theme::TEXT,
        body.bottom(),
        0.0,
    );
    place_right(
        &painter,
        &period.temperature,
        right,
        body.top() + 8.0,
        temp_width,
        14.0,
        theme::TEXT,
    );

    let precip_width = if period.precip.is_empty() { 0.0 } else { 90.0 };
    place_line(
        &painter,
        &period.short,
        egui::pos2(x, body.top() + 32.0),
        right - x - precip_width - 8.0,
        12.5,
        theme::TEXT_DIM,
        body.bottom(),
        0.0,
    );
    place_right(
        &painter,
        &period.precip,
        right,
        body.top() + 32.0,
        precip_width,
        12.5,
        theme::ACCENT_BRIGHT,
    );

    response.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weather::Alert;

    use std::sync::Arc;

    /// The dwell is a hold, not a slow start: nothing moves until it is over.
    #[test]
    fn a_running_line_holds_at_its_start_before_it_moves() {
        assert_eq!(marquee_offset(0.0, 500.0, 100.0, 2.5), 0.0);
        assert_eq!(marquee_offset(2.5, 500.0, 100.0, 2.5), 0.0);
        assert!(marquee_offset(2.6, 500.0, 100.0, 2.5) > 0.0);
    }

    /// Half a second into the run at 100px/s is 50px along, and four and a
    /// half seconds in is 450. The end of the travel is the wrap, which
    /// `a_running_line_wraps_without_a_seam` covers from both sides.
    #[test]
    fn a_running_line_travels_at_the_speed_it_was_given() {
        assert!((marquee_offset(3.0, 500.0, 100.0, 2.5) - 50.0).abs() < 0.01);
        assert!((marquee_offset(7.0, 500.0, 100.0, 2.5) - 450.0).abs() < 0.01);
    }

    /// The lap ends exactly where it began. The trailing copy is drawn one
    /// travel to the right, so an offset that overshot or stopped short would
    /// show a seam - a band of blank strip once a lap.
    #[test]
    fn a_running_line_wraps_without_a_seam() {
        let (travel, speed, dwell) = (500.0, 100.0, 2.5);
        let cycle = dwell + f64::from(travel) / f64::from(speed);

        // The instant before the wrap is the end of the travel; the instant
        // after is the start of the next dwell.
        assert!((marquee_offset(cycle - 0.001, travel, speed, dwell) - travel).abs() < 0.2);
        assert_eq!(marquee_offset(cycle, travel, speed, dwell), 0.0);

        // And it never leaves the lap, however long the clock has been running.
        for step in 0..400 {
            let offset = marquee_offset(f64::from(step) * 0.37, travel, speed, dwell);
            assert!(
                (0.0..=travel).contains(&offset),
                "offset {offset} outside the lap"
            );
        }
    }

    /// Degenerate inputs park the line at its start rather than dividing by
    /// zero. A strip collapsed to nothing still gets laid out.
    #[test]
    fn a_running_line_with_nowhere_to_go_stays_put() {
        assert_eq!(marquee_offset(9.0, 0.0, 100.0, 2.5), 0.0);
        assert_eq!(marquee_offset(9.0, 500.0, 0.0, 2.5), 0.0);
        assert_eq!(marquee_offset(9.0, f32::NAN, 100.0, 2.5), 0.0);
    }

    fn good(temp: &str) -> Arc<Model> {
        Arc::new(Model {
            ok: true,
            station: "Agawam".into(),
            temp_big: temp.into(),
            ..Model::empty()
        })
    }

    fn failed(message: &str) -> Arc<Model> {
        Arc::new(Model::failure(message))
    }

    /// A failed poll does not clear the strip. The last reading is still the
    /// best answer available; it is said in the corner rather than by blanking
    /// the numbers.
    #[test]
    fn a_failed_poll_keeps_the_last_good_reading() {
        let mut view = WeatherView::default();
        assert!(!view.has_data);

        view.absorb(good("71°F"));
        assert!(view.has_data);
        assert!(!view.stale);
        assert_eq!(view.model.temp_big, "71°F");

        view.absorb(failed("the server is not answering"));
        assert!(view.has_data, "the reading must survive the failure");
        assert!(view.stale);
        assert_eq!(view.model.temp_big, "71°F");
        assert_eq!(view.note, "the server is not answering");

        // And a reading that comes back clears the warning.
        view.absorb(good("69°F"));
        assert!(!view.stale);
        assert!(view.note.is_empty());
        assert_eq!(view.model.temp_big, "69°F");
    }

    /// Before anything has arrived there is nothing to keep, so the failure
    /// itself is what the strip shows.
    #[test]
    fn a_failure_before_any_reading_is_shown_as_the_reading() {
        let mut view = WeatherView::default();
        view.absorb(failed("no ZIP code is set"));
        assert!(!view.has_data);
        assert!(view.stale);
        assert_eq!(view.note, "no ZIP code is set");
        assert_eq!(view.model.error, "no ZIP code is set");
    }

    /// A shorter forecast must not leave the selection pointing past the end.
    #[test]
    fn the_selected_period_survives_a_shorter_forecast() {
        let mut view = WeatherView::default();
        view.absorb(Arc::new(Model {
            periods: vec![Period::default(); 8],
            ..(*good("71°F")).clone()
        }));
        view.period = 7;

        view.absorb(Arc::new(Model {
            periods: vec![Period::default(); 3],
            ..(*good("70°F")).clone()
        }));
        assert!(view.period < 3, "selection ran off the end: {}", view.period);
    }

    /// A reading with no periods at all is the case that would index into an
    /// empty list.
    #[test]
    fn a_forecast_with_no_periods_selects_nothing_out_of_range() {
        let mut view = WeatherView::default();
        view.period = 4;
        view.absorb(good("71°F"));
        assert_eq!(view.period, 0);
    }

    #[test]
    fn switching_the_weather_off_forgets_the_reading() {
        let mut view = WeatherView::default();
        view.absorb(good("71°F"));
        view.showing = PaneTab::Radar;

        view.reset();
        assert!(!view.has_data);
        assert_eq!(view.showing, PaneTab::Conditions);
        assert!(view.note.contains("Waiting"));
    }

    /// Alerts are bounded so the stack cannot push the reading off the pane,
    /// and what is dropped is counted rather than silently lost.
    #[test]
    fn the_alert_stack_is_bounded_and_says_so() {
        let alerts: Vec<Alert> = (0..7)
            .map(|index| Alert {
                event: format!("Watch {index}"),
                ..Alert::default()
            })
            .collect();
        let model = Model {
            alerts,
            ..(*good("71°F")).clone()
        };
        let shown = model.alerts.len().min(4);
        assert_eq!(shown, 4);
        assert_eq!(model.alerts.len() - shown, 3, "three go into the count");
        // The strip, which has one line, still carries every one of them.
        assert!(model.alerts_line(false).contains("Watch 6"));
    }

    /// Sizes scale with the strip's height but stay readable at either end.
    #[test]
    fn text_sizes_stay_between_their_bounds() {
        for height in [40.0f32, 132.0, 400.0] {
            let size = size_for(height, 0.345, 22.0, 56.0);
            assert!((22.0..=56.0).contains(&size), "at {height}px got {size}");
        }
        assert_eq!(size_for(132.0, 0.345, 22.0, 56.0), 132.0 * 0.345);
    }
}
