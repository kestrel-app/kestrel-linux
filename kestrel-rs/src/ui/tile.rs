//! One live video tile.
//!
//! Reads a stream and uploads its newest frame as a texture. Frames are pulled
//! rather than pushed: the worker keeps only the latest one, so a repaint
//! always shows current video and a slow UI can never build a backlog.
//!
//! A tile *reads* its stream rather than owning it, because a camera can be on
//! the wall more than once. A virtual camera is a tile holding a view of its
//! parent's worker and a saved rectangle to draw out of it, so the picture is
//! decoded once however many crops of it are on screen.

use std::sync::Arc;

use eframe::egui::{self, Color32, ColorImage, CornerRadius, Rect, Sense, Stroke, StrokeKind, TextureHandle, Vec2};

use super::theme;
use crate::api::{Channel, EventKind, SourceId, StreamType};
use crate::config::PictureFill;

/// Past roughly this the picture is more artefact than detail.
const MAX_DIGITAL_ZOOM: f32 = 8.0;

/// The whole of a picture, in texture coordinates.
const WHOLE_PICTURE: Rect = Rect {
    min: egui::pos2(0.0, 0.0),
    max: egui::pos2(1.0, 1.0),
};

/// The smallest a framing box may be drawn, as a fraction of the picture.
///
/// The reciprocal of the zoom ceiling, so the box cannot ask for a crop the
/// tile would refuse to draw.
const MIN_FRAMING: f32 = 1.0 / MAX_DIGITAL_ZOOM;

/// Half-width of a corner handle's grab area, in points.
const HANDLE_GRAB: f32 = 14.0;
/// Half-width of the square drawn at each corner.
const HANDLE_DRAW: f32 = 5.0;

/// Where a region of the picture lands on screen.
///
/// `picture` is the rectangle the whole camera is painted into, so this is the
/// one place the two coordinate systems meet — everything either side of it
/// works in its own and neither has to know about zoom or fill.
fn to_screen(uv: Rect, picture: Rect) -> Rect {
    Rect::from_min_max(
        egui::pos2(
            picture.left() + uv.left() * picture.width(),
            picture.top() + uv.top() * picture.height(),
        ),
        egui::pos2(
            picture.left() + uv.right() * picture.width(),
            picture.top() + uv.bottom() * picture.height(),
        ),
    )
}

/// A crop being chosen by hand on the picture.
///
/// Choosing part of a picture means seeing the whole of it, so a tile that is
/// framing shows its camera whole — the zoom is set aside and the fill mode
/// with it — and the box is drawn over that.
///
/// The crop is always the picture's own shape, because that is all the stored
/// form can express: a magnification and a centre. So the box needs one number
/// for its size rather than two, and dragging a corner cannot get the shape
/// wrong.
#[derive(Clone, Copy, Debug)]
struct Framing {
    /// Side of the box as a fraction of the picture.
    size: f32,
    centre: egui::Pos2,
    /// The corner being dragged, as (right, bottom), while one is held.
    grabbed: Option<(bool, bool)>,
    /// Whether a drag is moving the whole box rather than a corner.
    moving: bool,
}

impl Framing {
    /// The box in the picture's own coordinates, kept inside it.
    ///
    /// Clamping the centre rather than the edges is the same rule the visible
    /// region uses, so a box pushed at the border stops there instead of
    /// shrinking or showing padding.
    fn rect(&self) -> Rect {
        let size = self.size.clamp(MIN_FRAMING, 1.0);
        let half = size / 2.0;
        let centre = egui::pos2(
            self.centre.x.clamp(half, 1.0 - half),
            self.centre.y.clamp(half, 1.0 - half),
        );
        Rect::from_center_size(centre, Vec2::splat(size))
    }

    /// What this box would be saved as: a magnification and a centre.
    fn framing(&self) -> (f32, (f32, f32)) {
        let rect = self.rect();
        (1.0 / rect.width(), (rect.center().x, rect.center().y))
    }

    /// Resize about the corner opposite the one being dragged.
    ///
    /// The shape is fixed, so the pointer decides one number: the larger of the
    /// two distances from the fixed corner, which keeps the box under the hand
    /// whichever way it is being pulled.
    fn drag_corner(&mut self, corner: (bool, bool), pointer: egui::Pos2) {
        let rect = self.rect();
        let fixed = egui::pos2(
            if corner.0 { rect.left() } else { rect.right() },
            if corner.1 { rect.top() } else { rect.bottom() },
        );
        let across = (pointer.x - fixed.x).abs();
        let down = (pointer.y - fixed.y).abs();
        let size = across.max(down).clamp(MIN_FRAMING, 1.0);
        let sx = if pointer.x >= fixed.x { 1.0 } else { -1.0 };
        let sy = if pointer.y >= fixed.y { 1.0 } else { -1.0 };
        self.size = size;
        self.centre = egui::pos2(fixed.x + sx * size / 2.0, fixed.y + sy * size / 2.0);
    }
}

/// Where the whole source frame lands in a cell, for each way of filling it.
///
/// The rectangle the *frame* would occupy, which is not always the rectangle
/// that gets painted: filling by covering puts the frame's edges outside the
/// cell on purpose. Everything else — the crop, the pan, the zoom anchor —
/// is measured off this, so the three modes need no cases of their own.
fn frame_rect(stage: Rect, source: Vec2, fill: PictureFill) -> Rect {
    if source.x <= 0.0 || source.y <= 0.0 {
        return stage;
    }
    let scale = match fill {
        // Nothing is lost and no space is wasted; the shape is not kept.
        PictureFill::Stretch => return stage,
        // The larger dimension decides, so the whole frame is inside the cell.
        PictureFill::Fit => (stage.width() / source.x).min(stage.height() / source.y),
        // The smaller one, so the cell is covered and the rest hangs over.
        PictureFill::Fill => (stage.width() / source.x).max(stage.height() / source.y),
    };
    Rect::from_center_size(stage.center(), source * scale)
}

/// The part of the source that lands in `drawn`, given the whole frame would
/// have covered `frame`.
///
/// `view` is the region digital zoom has selected; this narrows it by however
/// much of the frame the cell does not show, about the same centre — so
/// cropping and zooming compose instead of fighting.
fn crop(view: Rect, frame: Rect, drawn: Rect) -> Rect {
    if frame.width() <= 0.0 || frame.height() <= 0.0 {
        return view;
    }
    Rect::from_center_size(
        view.center(),
        view.size()
            * Vec2::new(
                drawn.width() / frame.width(),
                drawn.height() / frame.height(),
            ),
    )
}

/// Height of the name strip above each picture.
///
/// Public because the forecast tiles that fill the wall's spare cells carry the
/// same strip in the same place — a caption a few pixels off the line the
/// cameras keep reads as a mistake without being possible to name.
pub const TITLE_HEIGHT: f32 = 22.0;

/// How long a stream may go without a clean picture before the tile says so, in
/// seconds.
///
/// Comfortably past any keyframe interval worth using, so an ordinary resync —
/// a moment of damaged data, dropped rather than drawn — passes without a word.
/// Past this the source is losing more than keyframes can rebuild, which is
/// worth knowing about.
const RECOVERING_NOTICE: f32 = 4.0;

/// Detection badges: box size, spacing, and inset from the right.
const BADGE_SIZE: f32 = 16.0;
const BADGE_GAP: f32 = 3.0;
const BADGE_MARGIN: f32 = 6.0;
use crate::video::{StreamState, StreamView};

pub struct Tile {
    /// Which camera this is, and which view of it.
    pub id: SourceId,
    pub channel: Channel,
    pub title: String,
    /// Which stream this tile is pulling, so the grid can tell whether a
    /// rebuild actually requires a reconnect.
    pub active_stream: StreamType,
    /// For a dual-lens camera, whether the telephoto channel is the one being
    /// streamed behind this tile.
    pub on_tele: bool,
    /// The stream this tile draws from. A reader's handle, not an owner: a
    /// camera and every virtual camera cropped out of it hold a view of the
    /// same worker, and [`Streams`] decides when that worker stops.
    ///
    /// [`Streams`]: super::grid::Streams
    pub stream: Option<StreamView>,

    texture: Option<TextureHandle>,
    last_sequence: u64,
    motion_until: Option<std::time::Instant>,
    /// What the camera is detecting, shown as badges along the bottom.
    detections: Vec<EventKind>,
    /// Whether to print the frame rate and bitrate beside the name.
    pub show_stats: bool,
    /// How the picture sits in the cell. Set from preferences each frame beside
    /// the rest of them, rather than pushed at every place the setting can
    /// change.
    pub fill: PictureFill,
    /// Whether follow-motion has been told to leave this camera alone, *and* is
    /// running - so the name says so exactly when somebody is wondering why the
    /// view is not moving, and says nothing the rest of the time.
    ///
    /// Without this the exemption is invisible: an excluded camera and a quiet
    /// one look identical, which makes one wrong right-click look like the
    /// whole mode having stopped working.
    pub exempt_from_follow: bool,
    /// Whether this camera is hidden from the wall and only on it because
    /// hidden cameras are currently being revealed.
    ///
    /// Revealing is how hiding is undone, so the one thing it has to do is make
    /// the hidden ones tellable from the rest - otherwise it puts them back on
    /// the wall and leaves you guessing which is which.
    pub hidden_but_shown: bool,
    /// Whether the name floats over the picture instead of taking its own
    /// strip. Set when names auto-hide: reserving a strip that comes and goes
    /// would resize the video every time the mouse stopped.
    pub overlay_title: bool,
    /// Opacity of the name strip, 0 when hidden.
    pub title_alpha: f32,

    /// The framing this tile starts and returns to, for a virtual camera:
    /// its saved zoom and centre.
    ///
    /// `None` on an ordinary camera, whose home is the whole picture. It is
    /// the difference between the two kinds of tile, and the only one that
    /// reaches the drawing code — everything below this line treats a virtual
    /// camera as a tile that happens to have started zoomed in.
    saved_view: Option<(f32, egui::Pos2)>,

    /// A crop being chosen by hand on this tile, if one is being chosen.
    framing: Option<Framing>,
    /// Set when the box has been accepted, for the grid to collect.
    framing_taken: bool,

    /// Digital zoom factor, 1.0 being the whole picture.
    zoom: f32,
    /// Centre of the visible region in texture coordinates, so panning survives
    /// a zoom change and a resize.
    focus: egui::Pos2,
    /// Scroll the tile could not consume itself, in wheel notches — positive is
    /// zoom in. Read by the grid, which owns the client needed to drive a real
    /// lens or optical zoom.
    zoom_request: f32,
}

impl Tile {
    pub fn new(id: SourceId, channel: Channel, title: String) -> Self {
        Tile {
            id,
            channel,
            title,
            active_stream: StreamType::Sub,
            on_tele: false,
            stream: None,
            texture: None,
            last_sequence: 0,
            motion_until: None,
            detections: Vec::new(),
            show_stats: false,
            fill: PictureFill::default(),
            exempt_from_follow: false,
            hidden_but_shown: false,
            overlay_title: false,
            title_alpha: 1.0,
            saved_view: None,
            framing: None,
            framing_taken: false,
            zoom: 1.0,
            focus: egui::pos2(0.5, 0.5),
            zoom_request: 0.0,
        }
    }

    /// A tile that starts life looking at one part of its camera's picture.
    ///
    /// The framing is where it opens and where [`Tile::reset_zoom`] returns it
    /// to. Scrolling and dragging still work exactly as they do on any other
    /// tile — a saved framing is a starting point, not a cage — and what they
    /// leave behind is thrown away with the tile unless it is deliberately
    /// saved.
    pub fn cropped(id: SourceId, channel: Channel, title: String, zoom: f32, centre: (f32, f32)) -> Self {
        let mut tile = Tile::new(id, channel, title);
        let framing = (zoom.clamp(1.0, MAX_DIGITAL_ZOOM), egui::pos2(centre.0, centre.1));
        tile.saved_view = Some(framing);
        tile.zoom = framing.0;
        tile.focus = framing.1;
        tile
    }

    pub fn key(&self) -> SourceId {
        self.id.clone()
    }

    /// What the connection behind this tile is keyed by.
    pub fn stream_key(&self) -> (String, u32) {
        self.id.stream_key()
    }

    pub fn device_id(&self) -> &str {
        &self.id.device
    }

    pub fn is_virtual(&self) -> bool {
        self.id.is_virtual()
    }

    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    pub fn attach(&mut self, stream: StreamView, kind: StreamType) {
        self.active_stream = kind;
        self.stream = Some(stream);
    }

    /// The framing this tile would be saved with: its zoom and its centre,
    /// clamped the way the picture is actually drawn so what is written down
    /// is what was on screen.
    pub fn framing(&self) -> (f32, (f32, f32)) {
        let view = self.view();
        (self.zoom, (view.center().x, view.center().y))
    }

    /// Move this tile's framing directly, for tests that need it somewhere
    /// specific without driving the pointer.
    #[cfg(test)]
    pub fn zoom_to_for_test(&mut self, zoom: f32, centre: (f32, f32)) {
        self.zoom = zoom;
        self.focus = egui::pos2(centre.0, centre.1);
    }

    /// Adopt a new saved framing and go to it, after the configuration behind
    /// this tile has been edited.
    pub fn set_saved_view(&mut self, zoom: f32, centre: (f32, f32)) {
        self.saved_view = Some((zoom.clamp(1.0, MAX_DIGITAL_ZOOM), egui::pos2(centre.0, centre.1)));
        self.reset_zoom();
    }

    // ------------------------------------------------------- choosing a crop

    /// Start choosing a crop of this camera by hand.
    ///
    /// Seeded from whatever the tile is already showing, so a picture somebody
    /// has just zoomed into opens with the box exactly around it and needs only
    /// to be accepted. An unzoomed tile has nothing to seed from and opens with
    /// a box half the picture across, which is a size you can see and grab
    /// rather than a guess at what anybody wanted.
    pub fn begin_framing(&mut self) {
        let view = self.view();
        let size = if self.zoom > 1.0 { view.width() } else { 0.5 };
        self.framing = Some(Framing {
            size,
            centre: view.center(),
            grabbed: None,
            moving: false,
        });
        self.framing_taken = false;
    }

    pub fn cancel_framing(&mut self) {
        self.framing = None;
        self.framing_taken = false;
    }

    pub fn is_framing(&self) -> bool {
        self.framing.is_some()
    }

    /// Whether there is a picture to choose part of.
    ///
    /// A camera still waiting for its first frame has nothing to put a box on,
    /// and a box over a placeholder would be a rectangle somebody is asked to
    /// position against a grey panel.
    pub fn has_picture(&self) -> bool {
        self.texture.is_some()
    }

    /// Accept the box, ending the interaction.
    pub fn accept_framing(&mut self) -> Option<(f32, (f32, f32))> {
        let chosen = self.framing.take().map(|framing| framing.framing());
        self.framing_taken = false;
        chosen
    }

    /// Whether the box was accepted on the picture itself, by double-click.
    pub fn take_framing_accepted(&mut self) -> bool {
        std::mem::take(&mut self.framing_taken)
    }

    /// Record what the camera is currently detecting, for the badges.
    pub fn set_detections(&mut self, kinds: Vec<EventKind>) {
        self.detections = kinds;
    }

    pub fn flag_motion(&mut self, active: bool) {
        if active {
            // Linger briefly so a short blip is still visible.
            self.motion_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
        }
    }

    fn motion_active(&self) -> bool {
        self.motion_until
            .map(|until| std::time::Instant::now() < until)
            .unwrap_or(false)
    }

    /// Upload the newest frame if there is one we have not shown yet.
    fn sync_texture(&mut self, ctx: &egui::Context) {
        let Some(stream) = &self.stream else { return };
        let Some(frame) = stream.latest_frame() else { return };
        if frame.sequence == self.last_sequence {
            return;
        }
        self.last_sequence = frame.sequence;

        let image = ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match &mut self.texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture(&self.title, image, egui::TextureOptions::LINEAR))
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, rect: Rect, selected: bool) -> egui::Response {
        self.sync_texture(ui.ctx());
        // Dragging pans a zoomed picture; at 1x it does nothing, and clicks and
        // double-clicks still work either way.
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, theme::INK);

        // With names always on, the title takes its own strip: overlaying it
        // permanently hid the top of every picture, which on a camera aimed
        // along a driveway is the part you care about. When names auto-hide it
        // floats instead, so the video keeps the whole tile and does not resize
        // every time the mouse goes still.
        let bar = Rect::from_min_size(rect.min, Vec2::new(rect.width(), TITLE_HEIGHT));
        let stage = if self.overlay_title {
            rect
        } else {
            Rect::from_min_max(
                egui::pos2(rect.left(), rect.top() + TITLE_HEIGHT),
                rect.max,
            )
        };

        // Read out of the texture before zooming, which needs &mut self.
        match self.texture.as_ref().map(|t| (t.id(), t.size_vec2())) {
            Some((id, size)) => {
                // Choosing part of a picture means seeing all of it, so a tile
                // that is framing sets its zoom aside — and sets aside a fill
                // mode that covers the cell by cropping the edges away, since
                // those edges are exactly where somebody may want the box.
                // Stretching is left alone: it shows the whole picture already,
                // and the box drawn through the same distortion is the truth
                // about what the crop will look like.
                let framing = self.is_framing();
                let fill = if framing && self.fill == PictureFill::Fill {
                    PictureFill::Fit
                } else {
                    self.fill
                };
                // Where the whole frame *would* land, and the part of the cell
                // that actually gets painted. They differ only when the picture
                // is filling the cell by covering it, and then the difference
                // is the edges being cropped away.
                let frame = frame_rect(stage, size, fill);
                let drawn = stage.intersect(frame);
                let view = if framing { WHOLE_PICTURE } else { self.view() };
                if !framing {
                    // Zoom and pan are measured against the whole frame rather
                    // than against what is on screen, so a cropped picture
                    // still pans by the distance the hand moved.
                    self.handle_zoom(ui, &response, frame);
                }
                painter.image(id, drawn, crop(view, frame, drawn), Color32::WHITE);
                // The picture on screen is the last one that arrived, which may
                // no longer be the truth.
                self.paint_staleness(&painter, drawn);
                if framing {
                    self.handle_framing(ui, &response, drawn);
                    self.paint_framing(&painter, drawn);
                }
            }
            None => {
                // Nothing to zoom into yet, but still absorb the scroll so it
                // does not act on whatever is behind the tile.
                self.handle_zoom(ui, &response, stage);
                self.paint_placeholder(&painter, stage)
            }
        }

        self.paint_overlay(&painter, bar, selected);
        response
    }

    /// Drive the framing box: drag it, drag a corner, or scroll to size it.
    ///
    /// `picture` is where the whole camera is painted, which while framing is
    /// the whole of it — so the box's coordinates and the screen's are one
    /// scale factor apart and nothing here has to know about zoom or fill.
    fn handle_framing(&mut self, ui: &egui::Ui, response: &egui::Response, picture: Rect) {
        let Some(mut framing) = self.framing else { return };
        if picture.width() <= 0.0 || picture.height() <= 0.0 {
            return;
        }
        let to_picture = |at: egui::Pos2| {
            egui::pos2(
                (at.x - picture.left()) / picture.width(),
                (at.y - picture.top()) / picture.height(),
            )
        };

        // Scroll sizes the box about its own centre. It is the same gesture
        // that zooms a tile, doing the same thing to the thing being chosen.
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                framing.size = (framing.size / (scroll * 0.004).exp()).clamp(MIN_FRAMING, 1.0);
            }
        }

        let pointer = response
            .interact_pointer_pos()
            .or_else(|| ui.input(|i| i.pointer.hover_pos()));

        if response.drag_started() {
            // Which corner, if the drag began on one. Measured in points rather
            // than in the picture's own units so the grab area is the same size
            // on a cell of any shape.
            framing.grabbed = None;
            framing.moving = false;
            if let Some(at) = pointer {
                let box_rect = framing.rect();
                let corners = [
                    (false, false, egui::pos2(box_rect.left(), box_rect.top())),
                    (true, false, egui::pos2(box_rect.right(), box_rect.top())),
                    (false, true, egui::pos2(box_rect.left(), box_rect.bottom())),
                    (true, true, egui::pos2(box_rect.right(), box_rect.bottom())),
                ];
                for (right, bottom, corner) in corners {
                    let on_screen =
                        to_screen(Rect::from_min_max(corner, corner), picture).min;
                    if (on_screen - at).length() <= HANDLE_GRAB {
                        framing.grabbed = Some((right, bottom));
                        break;
                    }
                }
                if framing.grabbed.is_none() {
                    // Anywhere else moves the box, including outside it: a box
                    // that can only be picked up by its middle is a box that
                    // gets away from you at the edge of the picture.
                    framing.moving = true;
                }
            }
        }

        if response.dragged() {
            if let (Some(corner), Some(at)) = (framing.grabbed, pointer) {
                framing.drag_corner(corner, to_picture(at));
            } else if framing.moving {
                let drag = response.drag_delta();
                framing.centre.x += drag.x / picture.width();
                framing.centre.y += drag.y / picture.height();
            }
        }

        if response.drag_stopped() {
            framing.grabbed = None;
            framing.moving = false;
        }

        // Accepting on the picture itself, where the hands already are. The
        // grid does not expand a tile that is framing, so this gesture is free.
        if response.double_clicked() {
            self.framing_taken = true;
        }

        self.framing = Some(framing);
    }

    /// Draw the box, dim everything it leaves out, and say what it would be.
    fn paint_framing(&self, painter: &egui::Painter, picture: Rect) {
        let Some(framing) = self.framing else { return };
        let on_screen = to_screen(framing.rect(), picture);

        // Dimming what is left out says what the crop is without a caption, and
        // says it in the one place somebody is already looking.
        let shade = Color32::from_black_alpha(170);
        for outside in [
            Rect::from_min_max(picture.min, egui::pos2(picture.right(), on_screen.top())),
            Rect::from_min_max(egui::pos2(picture.left(), on_screen.bottom()), picture.max),
            Rect::from_min_max(
                egui::pos2(picture.left(), on_screen.top()),
                egui::pos2(on_screen.left(), on_screen.bottom()),
            ),
            Rect::from_min_max(
                egui::pos2(on_screen.right(), on_screen.top()),
                egui::pos2(picture.right(), on_screen.bottom()),
            ),
        ] {
            if outside.width() > 0.0 && outside.height() > 0.0 {
                painter.rect_filled(outside, CornerRadius::ZERO, shade);
            }
        }

        painter.rect_stroke(
            on_screen,
            CornerRadius::ZERO,
            Stroke::new(2.0_f32, theme::ACCENT_BRIGHT),
            StrokeKind::Inside,
        );
        for corner in [
            egui::pos2(on_screen.left(), on_screen.top()),
            egui::pos2(on_screen.right(), on_screen.top()),
            egui::pos2(on_screen.left(), on_screen.bottom()),
            egui::pos2(on_screen.right(), on_screen.bottom()),
        ] {
            painter.rect_filled(
                Rect::from_center_size(corner, Vec2::splat(HANDLE_DRAW * 2.0)),
                CornerRadius::ZERO,
                theme::ACCENT_BRIGHT,
            );
        }

        // What it would be, kept against the box so the number and the thing it
        // describes are never apart. Below it by preference: the hint sits
        // along the top, and a box near the top of the picture would otherwise
        // put the two of them on the same line.
        let (zoom, centre) = framing.framing();
        let readout = format!("{zoom:.1}x  ·  {:.0}%, {:.0}%", centre.0 * 100.0, centre.1 * 100.0);
        let below = on_screen.bottom() + 11.0;
        let anchor = if below <= picture.bottom() - 6.0 {
            egui::pos2(on_screen.center().x, below)
        } else {
            egui::pos2(on_screen.center().x, on_screen.top() - 11.0)
        };
        painter.text(
            anchor,
            egui::Align2::CENTER_CENTER,
            readout,
            egui::FontId::proportional(12.0),
            theme::ACCENT_BRIGHT,
        );

        // The gestures, said once where they are needed: a box nobody can tell
        // how to accept is a box that gets abandoned. On a chip along the top,
        // because the bottom of a picture already belongs to the stream notice
        // — and a camera being framed is exactly one that might be saying
        // "Reconnecting…" underneath.
        // Measured and shortened rather than left to overflow: a wall of
        // sixteen gives each camera a narrow cell, and a hint running off both
        // sides of it is worse than a shorter one.
        let mut galley = None;
        for wording in [
            "Drag to place  ·  corners to size  ·  double-click to keep  ·  Esc to cancel",
            "Drag  ·  corners to size  ·  double-click to keep",
            "Double-click to keep  ·  Esc",
        ] {
            let measured = painter.layout_no_wrap(
                wording.to_string(),
                egui::FontId::proportional(11.0),
                theme::TEXT,
            );
            let fits = measured.size().x + 16.0 <= picture.width();
            galley = Some(measured);
            if fits {
                break;
            }
        }
        let Some(galley) = galley else { return };
        let chip = Rect::from_center_size(
            egui::pos2(picture.center().x, picture.top() + galley.size().y / 2.0 + 10.0),
            galley.size() + Vec2::new(16.0, 8.0),
        );
        painter.rect_filled(chip, CornerRadius::same(5), theme::INK.gamma_multiply(0.85));
        painter.galley(chip.min + Vec2::new(8.0, 4.0), galley, theme::TEXT);
    }

    /// The visible region of the texture, in UV coordinates.
    fn view(&self) -> Rect {
        let half = 0.5 / self.zoom;
        // Clamping the centre rather than the edges keeps the window inside the
        // picture, so panning stops at the border instead of showing padding.
        let centre = egui::pos2(
            self.focus.x.clamp(half, 1.0 - half),
            self.focus.y.clamp(half, 1.0 - half),
        );
        Rect::from_center_size(centre, Vec2::splat(2.0 * half))
    }

    /// Digital zoom is what happens when the camera cannot zoom itself.
    ///
    /// Scroll that this tile cannot act on — a camera with real optical zoom, or
    /// the two lenses of a dual-lens camera — is handed up instead, because
    /// driving those needs the API client the grid owns.
    ///
    /// A virtual camera never hands it up, whatever the lens can do. Its whole
    /// definition is a rectangle of the picture, and moving the lens would move
    /// that rectangle onto something else — on every other tile of the same
    /// camera at the same time. Scrolling a crop adjusts the crop.
    /// Whether scroll on this tile belongs to the camera rather than to the
    /// picture: a zoom motor, or the second lens of a dual-lens camera.
    fn drives_lens(&self) -> bool {
        !self.is_virtual()
            && (self.channel.is_dual_lens() || self.channel.optical_zoom_supported)
    }

    fn handle_zoom(&mut self, ui: &egui::Ui, response: &egui::Response, target: Rect) {
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                if self.drives_lens() {
                    self.zoom_request += scroll;
                } else {
                    // A fixed multiplier per notch: zooming feels linear only if
                    // each step scales, rather than adds.
                    let factor = (scroll * 0.004).exp();
                    self.zoom_at(ui, response, target, factor);
                }
            }
        }

        if self.zoom > 1.0 && response.dragged() {
            let drag = response.drag_delta();
            if target.width() > 0.0 && target.height() > 0.0 {
                // Drag the picture, not the window onto it: moving the mouse
                // right should bring what is on the left into view.
                self.focus.x -= drag.x / target.width() / self.zoom;
                self.focus.y -= drag.y / target.height() / self.zoom;
            }
        }
    }

    /// Scale about the cursor, so whatever is under the pointer stays there.
    fn zoom_at(&mut self, ui: &egui::Ui, response: &egui::Response, target: Rect, factor: f32) {
        let Some(pointer) = response.hover_pos().or_else(|| ui.input(|i| i.pointer.latest_pos()))
        else {
            return;
        };
        if target.width() <= 0.0 || target.height() <= 0.0 {
            return;
        }
        // Where the cursor sits within the picture as currently drawn.
        let t = egui::vec2(
            ((pointer.x - target.left()) / target.width()).clamp(0.0, 1.0),
            ((pointer.y - target.top()) / target.height()).clamp(0.0, 1.0),
        );
        self.apply_zoom(t, factor);
    }

    /// Scale about a point, given as a fraction across the drawn picture.
    ///
    /// Separate from the event handling so the ordering it depends on — read the
    /// visible region, *then* change the zoom — is covered by tests.
    fn apply_zoom(&mut self, t: Vec2, factor: f32) {
        let view_before = self.view();
        let before = self.zoom;
        self.zoom = (self.zoom * factor).clamp(1.0, MAX_DIGITAL_ZOOM);
        if self.zoom == before {
            return;
        }
        if self.zoom == 1.0 {
            self.focus = egui::pos2(0.5, 0.5);
            return;
        }
        self.focus = Self::anchored_focus(view_before, t, self.zoom);
    }

    /// Centre the visible region must take so the image point under the cursor
    /// stays under the cursor.
    ///
    /// `t` is where the pointer sits across the currently drawn picture, 0 to 1.
    /// The point it covers is `view.min + t * view.size()`; for that to land in
    /// the same place once the region is `1/zoom` across, the centre has to move
    /// by `(0.5 - t)` of the new width.
    fn anchored_focus(view: Rect, t: Vec2, zoom: f32) -> egui::Pos2 {
        let anchor = view.min + Vec2::new(t.x * view.width(), t.y * view.height());
        let size = 1.0 / zoom;
        egui::pos2(
            anchor.x + (0.5 - t.x) * size,
            anchor.y + (0.5 - t.y) * size,
        )
    }

    /// Scroll this tile could not act on, in wheel notches. Taking it clears it.
    pub fn take_zoom_request(&mut self) -> f32 {
        std::mem::take(&mut self.zoom_request)
    }

    pub fn digital_zoom(&self) -> f32 {
        self.zoom
    }

    /// Return to where this tile started.
    ///
    /// For an ordinary camera that is the whole picture. For a virtual camera
    /// it is the framing it was saved with — going to the whole picture there
    /// would be going to the *parent*, which is not what anyone means by
    /// resetting a camera whose entire definition is a rectangle.
    pub fn reset_zoom(&mut self) {
        let (zoom, focus) = self.saved_view.unwrap_or((1.0, egui::pos2(0.5, 0.5)));
        self.zoom = zoom;
        self.focus = focus;
    }

    /// Whether this tile is anywhere other than its saved framing, which is
    /// what makes resetting worth offering.
    pub fn is_off_framing(&self) -> bool {
        let (zoom, focus) = self.saved_view.unwrap_or((1.0, egui::pos2(0.5, 0.5)));
        (self.zoom - zoom).abs() > 1e-3 || (self.focus - focus).length() > 1e-3
    }

    /// Say so when the picture being shown is not live.
    ///
    /// A tile that has decoded a frame keeps showing it, which is right — a
    /// held picture beats a black rectangle while a stream comes back. What is
    /// wrong is showing it *silently*: a camera whose connection died looks
    /// exactly like a camera pointed at something that is not moving, and there
    /// is no way to tell them apart from across a room. This is the difference.
    fn paint_staleness(&self, painter: &egui::Painter, stage: Rect) {
        let Some(stream) = self.stream.as_ref() else { return };
        let (state, detail) = stream.state();
        let text = match state {
            // Connected and reading, but nothing clean enough to show. The
            // decoder is holding the picture back until a keyframe rebuilds it,
            // which is right — and saying nothing about it would leave a frozen
            // frame looking like a still scene, which is the exact fault this
            // whole overlay exists for.
            StreamState::Playing
                if stream.stats().recovering_for >= RECOVERING_NOTICE =>
            {
                "Recovering the picture…".to_string()
            }
            StreamState::Playing | StreamState::Idle => return,
            StreamState::Connecting => "Connecting…".to_string(),
            StreamState::Reconnecting => "Reconnecting…".to_string(),
            StreamState::Error => {
                // The reason belongs here rather than only in the log: "camera
                // unreachable" and "not authorised" want different actions.
                if detail.is_empty() {
                    "Reconnecting…".to_string()
                } else {
                    format!("Reconnecting… {detail}")
                }
            }
            StreamState::Stopped => "Stopped".to_string(),
        };

        // Knocked back enough to read as suspended, not so far that the last
        // picture stops being useful — it may well be the one worth seeing.
        painter.rect_filled(stage, CornerRadius::ZERO, theme::INK.gamma_multiply(0.35));

        let galley = painter.layout_no_wrap(
            text,
            egui::FontId::proportional(12.0),
            theme::WARN,
        );
        let pill = Rect::from_min_size(
            egui::pos2(stage.left() + 8.0, stage.bottom() - galley.size().y - 16.0),
            galley.size() + Vec2::new(16.0, 8.0),
        );
        painter.rect_filled(pill, CornerRadius::same(5), theme::INK.gamma_multiply(0.85));
        painter.galley(pill.min + Vec2::new(8.0, 4.0), galley, theme::WARN);
    }

    fn paint_placeholder(&self, painter: &egui::Painter, rect: Rect) {
        let (state, detail) = self
            .stream
            .as_ref()
            .map(|s| s.state())
            .unwrap_or((StreamState::Idle, String::new()));
        let text = match state {
            StreamState::Idle => "Not streaming".to_string(),
            StreamState::Connecting => "Connecting…".to_string(),
            StreamState::Reconnecting => format!("Reconnecting… {detail}"),
            StreamState::Error => format!("Stream error\n{detail}"),
            StreamState::Stopped => "Stopped".to_string(),
            StreamState::Playing => "Waiting for video…".to_string(),
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(13.0),
            theme::PLACEHOLDER,
        );
    }

    /// Badges for what the camera is detecting, at the right of the name strip.
    ///
    /// They live in the strip rather than over the video for the same reason
    /// the name does: the picture is the point, and a camera that is detecting
    /// something is exactly the one you do not want a corner obscured on.
    ///
    /// Returns the width taken, so the readout beside them can be placed clear.
    ///
    /// Drawn as vectors rather than characters: the bundled font has no person
    /// or vehicle symbol, and a missing glyph renders as a box — which is what
    /// happened the last time this UI reached for a decorative character.
    fn paint_detections(&self, painter: &egui::Painter, bar: Rect, chip_alpha: f32) {
        if self.badge_count() == 0 {
            return;
        }
        let mut right = bar.right() - BADGE_MARGIN;
        for kind in self.detections.iter().rev() {
            let box_rect = Rect::from_center_size(
                egui::pos2(right - BADGE_SIZE / 2.0, bar.center().y),
                Vec2::splat(BADGE_SIZE),
            );
            if box_rect.left() < bar.left() {
                break;
            }
            if chip_alpha > 0.0 {
                // Only needed once the strip behind them has gone: over raw
                // video a bare stroke disappears against a bright scene.
                painter.rect_filled(
                    box_rect.expand(2.0),
                    CornerRadius::same(4),
                    theme::INK.gamma_multiply(0.8 * chip_alpha),
                );
            }
            Self::paint_icon(painter, box_rect.shrink(1.5), *kind);
            right -= BADGE_SIZE + BADGE_GAP;
        }
    }

    fn badge_count(&self) -> usize {
        if self.motion_active() {
            self.detections.len()
        } else {
            0
        }
    }

    /// Width the badges occupy, so the readout beside them can be placed clear.
    fn badge_width(&self) -> f32 {
        let n = self.badge_count();
        if n == 0 {
            0.0
        } else {
            n as f32 * (BADGE_SIZE + BADGE_GAP)
        }
    }

    fn paint_icon(painter: &egui::Painter, area: Rect, kind: EventKind) {
        let colour = theme::WARN;
        let stroke = Stroke::new(1.4, colour);
        let (w, h) = (area.width(), area.height());
        let x = area.left();
        let y = area.top();

        match kind {
            EventKind::Person => {
                // Head over shoulders.
                painter.circle_filled(egui::pos2(x + w * 0.5, y + h * 0.22), h * 0.19, colour);
                painter.rect_filled(
                    Rect::from_min_max(
                        egui::pos2(x + w * 0.26, y + h * 0.52),
                        egui::pos2(x + w * 0.74, y + h),
                    ),
                    CornerRadius {
                        nw: (h * 0.28) as u8,
                        ne: (h * 0.28) as u8,
                        sw: 0,
                        se: 0,
                    },
                    colour,
                );
            }
            EventKind::Face => {
                // Outline of a head, with eyes: distinct from a solid person.
                painter.circle_stroke(area.center(), h * 0.42, stroke);
                painter.circle_filled(
                    egui::pos2(x + w * 0.38, y + h * 0.42),
                    h * 0.07,
                    colour,
                );
                painter.circle_filled(
                    egui::pos2(x + w * 0.62, y + h * 0.42),
                    h * 0.07,
                    colour,
                );
                painter.line_segment(
                    [
                        egui::pos2(x + w * 0.38, y + h * 0.66),
                        egui::pos2(x + w * 0.62, y + h * 0.66),
                    ],
                    stroke,
                );
            }
            EventKind::Vehicle => {
                // Cabin, body and two wheels.
                painter.rect_filled(
                    Rect::from_min_max(
                        egui::pos2(x + w * 0.22, y + h * 0.22),
                        egui::pos2(x + w * 0.78, y + h * 0.5),
                    ),
                    CornerRadius::same((h * 0.14) as u8),
                    colour,
                );
                painter.rect_filled(
                    Rect::from_min_max(
                        egui::pos2(x, y + h * 0.46),
                        egui::pos2(x + w, y + h * 0.72),
                    ),
                    CornerRadius::same((h * 0.12) as u8),
                    colour,
                );
                painter.circle_filled(egui::pos2(x + w * 0.24, y + h * 0.8), h * 0.13, colour);
                painter.circle_filled(egui::pos2(x + w * 0.76, y + h * 0.8), h * 0.13, colour);
            }
            EventKind::Pet => {
                // A head with two ears. The ears have to stay small relative to
                // the head, or the mark reads as a pair of horns.
                painter.circle_filled(egui::pos2(x + w * 0.5, y + h * 0.6), h * 0.36, colour);
                for side in [0.26f32, 0.74] {
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            egui::pos2(x + w * side, y + h * 0.06),
                            egui::pos2(x + w * (side - 0.13), y + h * 0.42),
                            egui::pos2(x + w * (side + 0.13), y + h * 0.42),
                        ],
                        colour,
                        Stroke::NONE,
                    ));
                }
                // Eyes, punched out of the head so it reads as a face.
                for side in [0.38f32, 0.62] {
                    painter.circle_filled(
                        egui::pos2(x + w * side, y + h * 0.55),
                        h * 0.06,
                        theme::INK,
                    );
                }
            }
            EventKind::Package => {
                // A parcel: box with a tape band across it.
                let body = Rect::from_min_max(
                    egui::pos2(x + w * 0.08, y + h * 0.24),
                    egui::pos2(x + w * 0.92, y + h * 0.9),
                );
                painter.rect_filled(body, CornerRadius::same(2), colour);
                // The band is punched out so the shape reads as a package
                // rather than a plain rectangle.
                painter.rect_filled(
                    Rect::from_min_max(
                        egui::pos2(x + w * 0.42, body.top()),
                        egui::pos2(x + w * 0.58, body.bottom()),
                    ),
                    CornerRadius::ZERO,
                    theme::INK,
                );
                painter.line_segment(
                    [
                        egui::pos2(body.left(), y + h * 0.45),
                        egui::pos2(body.right(), y + h * 0.45),
                    ],
                    Stroke::new(1.2, theme::INK),
                );
            }
            EventKind::Motion => {
                // Plain movement: a mark with radiating rings.
                painter.circle_filled(area.center(), h * 0.16, colour);
                painter.circle_stroke(area.center(), h * 0.34, Stroke::new(1.2, colour));
                painter.circle_stroke(
                    area.center(),
                    h * 0.5,
                    Stroke::new(1.2, colour.gamma_multiply(0.55)),
                );
            }
        }
    }

    /// What the name strip says about following, which is nothing at all unless
    /// this camera has been left out of a mode that is currently running.
    fn follow_note(&self) -> &'static str {
        if self.exempt_from_follow {
            "  ·  not followed"
        } else {
            ""
        }
    }

    fn paint_overlay(&self, painter: &egui::Painter, bar: Rect, selected: bool) {
        let alpha = self.title_alpha.clamp(0.0, 1.0);

        // Detections keep full strength whatever the strip is doing: they are an
        // alert, not a label, and the point is to catch the eye of someone who
        // is not touching the mouse. Their backing chip fades *in* as the strip
        // fades out, so they stay legible once the band has gone.
        self.paint_detections(painter, bar, 1.0 - alpha);
        if alpha <= 0.0 {
            return;
        }

        // The selected camera is marked along its title rather than by a ring
        // around the picture: the whole strip lights up, which reads from across
        // a room without taking a border off the video.
        painter.rect_filled(
            bar,
            CornerRadius::ZERO,
            if selected {
                theme::ACCENT_DEEP.gamma_multiply(alpha)
            } else {
                theme::PANEL.gamma_multiply(alpha)
            },
        );

        let mut label = self.title.clone();
        if self.channel.is_dual_lens() {
            // Say which lens is live: the two look different enough that not
            // labelling it is confusing.
            label.push_str(if self.on_tele { "  ·  Tele" } else { "  ·  Wide" });
        }
        if self.zoom > 1.0 {
            // Without this a cropped picture just looks like a camera pointed
            // somewhere odd.
            label.push_str(&format!("  ·  {:.1}×", self.zoom));
        }
        if self.hidden_but_shown {
            label.push_str("  ·  hidden");
        }
        label.push_str(self.follow_note());
        painter.text(
            bar.left_center() + Vec2::new(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(12.0),
            theme::TEXT.gamma_multiply(alpha),
        );

        if let Some(stream) = self.stream.as_ref().filter(|_| self.show_stats) {
            let stats = stream.stats();
            if stats.fps > 0.0 {
                painter.text(
                    bar.right_center() - Vec2::new(6.0 + self.badge_width(), 0.0),
                    egui::Align2::RIGHT_CENTER,
                    format!("{:.0} fps · {:.1} Mb/s", stats.fps, stats.bitrate_kbps / 1000.0),
                    egui::FontId::proportional(11.0),
                    // Secondary to the camera name on either ground.
                    if selected {
                        theme::TEXT.gamma_multiply(0.7 * alpha)
                    } else {
                        theme::TEXT_DIM.gamma_multiply(alpha)
                    },
                );
            }
        }
    }

}

/// Keep the texture alive only while the tile exists.
impl Drop for Tile {
    fn drop(&mut self) {
        self.texture = None;
    }
}

pub type SharedTile = Arc<std::sync::Mutex<Tile>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn tile() -> Tile {
        Tile::new(SourceId::camera("dev", 0), Channel::new(0), "Camera".into())
    }

    /// A tile for a camera that is a crop of another, framed as given.
    fn cropped_tile(zoom: f32, centre: (f32, f32)) -> Tile {
        Tile::cropped(
            SourceId::virtual_camera("dev", 0, "gate"),
            Channel::new(0),
            "Gate".into(),
            zoom,
            centre,
        )
    }

    /// The three ways a picture can meet a cell, each giving up the one thing
    /// the others keep: bars, the shape, or the edges of the frame.
    ///
    /// Measured against the case that prompted the setting — a 16:9 camera in a
    /// cell off a 3x2 wall, which is about 1.19:1 and the worst mismatch an
    /// ordinary wall produces.
    #[test]
    fn a_picture_meets_its_cell_the_way_the_setting_says() {
        let cell = Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(640.0, 540.0));
        let source = Vec2::new(1920.0, 1080.0);

        // Stretch: the cell exactly, nothing cropped.
        let frame = frame_rect(cell, source, PictureFill::Stretch);
        assert_eq!(frame, cell);
        let drawn = cell.intersect(frame);
        assert_eq!(crop(whole(), frame, drawn), whole(), "the whole frame is shown");

        // Fit: inside the cell, its shape kept, so bars above and below.
        let frame = frame_rect(cell, source, PictureFill::Fit);
        assert!((frame.width() - cell.width()).abs() < 0.01, "as wide as the cell");
        assert!(frame.height() < cell.height(), "and shorter, which is the bars");
        assert!((aspect(frame) - aspect_of(source)).abs() < 1e-4, "the shape is kept");
        assert_eq!(cell.intersect(frame), frame, "all of it is on screen");

        // Fill: covers the cell, its shape kept, so the sides go.
        let frame = frame_rect(cell, source, PictureFill::Fill);
        assert!((frame.height() - cell.height()).abs() < 0.01, "as tall as the cell");
        assert!(frame.width() > cell.width(), "and wider, which is the crop");
        assert!((aspect(frame) - aspect_of(source)).abs() < 1e-4, "the shape is kept");

        let drawn = cell.intersect(frame);
        let shown = crop(whole(), frame, drawn);
        assert!((shown.height() - 1.0).abs() < 1e-4, "the full height is kept");
        assert!(shown.width() < 1.0, "and only part of the width");
        // Centred, so what goes is the two sides equally.
        assert!((shown.center().x - 0.5).abs() < 1e-4, "{shown:?}");
        assert!((shown.center().y - 0.5).abs() < 1e-4, "{shown:?}");
    }

    /// The crop composes with digital zoom rather than replacing it: zooming a
    /// cropped picture must narrow the window it was already looking through.
    #[test]
    fn cropping_narrows_whatever_the_zoom_had_selected() {
        let cell = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(640.0, 540.0));
        let source = Vec2::new(1920.0, 1080.0);
        let frame = frame_rect(cell, source, PictureFill::Fill);
        let drawn = cell.intersect(frame);

        // Zoomed 2x on a point off to one side.
        let zoomed = Rect::from_center_size(egui::pos2(0.3, 0.6), Vec2::splat(0.5));
        let shown = crop(zoomed, frame, drawn);
        assert!(shown.width() < zoomed.width(), "narrower than the zoom alone");
        assert!((shown.height() - zoomed.height()).abs() < 1e-4, "and no shorter");
        assert!(
            (shown.center() - zoomed.center()).length() < 1e-4,
            "about the point the zoom was looking at"
        );
    }

    /// A cell with no size, or a frame with none, must not divide by zero and
    /// hand the painter a rectangle of NaN.
    #[test]
    fn a_cell_with_no_size_is_survivable() {
        let empty = Rect::from_min_size(egui::pos2(4.0, 4.0), Vec2::ZERO);
        for mode in PictureFill::ALL {
            let frame = frame_rect(empty, Vec2::new(1920.0, 1080.0), mode);
            assert!(frame.width().is_finite() && frame.height().is_finite(), "{mode:?}");
            let shown = crop(whole(), frame, empty.intersect(frame));
            assert!(shown.width().is_finite() && shown.height().is_finite(), "{mode:?}");
        }
        // And a texture that reports no size at all.
        assert_eq!(frame_rect(empty, Vec2::ZERO, PictureFill::Fit), empty);
    }

    fn whole() -> Rect {
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
    }
    fn aspect(rect: Rect) -> f32 {
        rect.width() / rect.height()
    }
    fn aspect_of(size: Vec2) -> f32 {
        size.x / size.y
    }

    /// An exempted camera has to be readable as exempted, and only while the
    /// mode it is exempt from is running. Invisible state is what turned one
    /// wrong right-click into "follow motion is broken".
    #[test]
    fn a_camera_left_out_of_following_says_so_on_its_name() {
        let mut tile = tile();
        assert!(!tile.exempt_from_follow, "nothing is exempt until it is said");

        // The suffix joins the ones already there rather than replacing them,
        // so a zoomed exempt camera still reports its zoom.
        tile.exempt_from_follow = true;
        assert!(tile.follow_note().contains("not followed"), "{}", tile.follow_note());
        tile.exempt_from_follow = false;
        assert!(tile.follow_note().is_empty(), "and says nothing otherwise");
    }

    /// A revealed camera has to be tellable from the rest, or revealing puts
    /// hidden cameras back on the wall and leaves you guessing which to undo.
    /// The two markers are independent and can both apply at once.
    #[test]
    fn a_revealed_camera_is_tellable_from_the_rest() {
        let mut tile = tile();
        assert!(!tile.hidden_but_shown, "nothing is revealed until it is said");

        // Both at once: a camera can be hidden *and* left out of following, and
        // undoing one is not undoing the other.
        tile.hidden_but_shown = true;
        tile.exempt_from_follow = true;
        assert!(tile.follow_note().contains("not followed"));
        assert!(tile.hidden_but_shown, "and still reads as hidden");
    }

    #[test]
    fn an_unzoomed_tile_shows_the_whole_picture() {
        let view = tile().view();
        assert_eq!(view.min, egui::pos2(0.0, 0.0));
        assert_eq!(view.max, egui::pos2(1.0, 1.0));
    }

    /// Panning must stop at the edge of the picture. Letting the window run off
    /// would show padding where video should be.
    #[test]
    fn the_visible_region_stays_inside_the_picture() {
        let mut t = tile();
        t.zoom = 4.0;
        for focus in [
            egui::pos2(0.0, 0.0),
            egui::pos2(1.0, 1.0),
            egui::pos2(-5.0, 7.0),
            egui::pos2(0.5, 0.5),
        ] {
            t.focus = focus;
            let view = t.view();
            assert!(view.min.x >= -f32::EPSILON, "{focus:?} -> {view:?}");
            assert!(view.min.y >= -f32::EPSILON, "{focus:?} -> {view:?}");
            assert!(view.max.x <= 1.0 + f32::EPSILON, "{focus:?} -> {view:?}");
            assert!(view.max.y <= 1.0 + f32::EPSILON, "{focus:?} -> {view:?}");
            // And it is the size the zoom asked for.
            assert!((view.width() - 0.25).abs() < 1e-5);
        }
    }

    #[test]
    fn resetting_returns_to_the_full_picture() {
        let mut t = tile();
        t.zoom = 6.0;
        t.focus = egui::pos2(0.2, 0.9);
        t.reset_zoom();
        assert_eq!(t.digital_zoom(), 1.0);
        assert_eq!(t.view(), Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)));
    }

    /// A virtual camera is a saved rectangle, so its tile has to open on that
    /// rectangle rather than on the whole picture and then move.
    #[test]
    fn a_virtual_camera_opens_at_its_saved_framing() {
        let t = cropped_tile(2.5, (0.62, 0.38));
        assert_eq!(t.digital_zoom(), 2.5);
        let view = t.view();
        assert!((view.center().x - 0.62).abs() < 1e-5, "{view:?}");
        assert!((view.center().y - 0.38).abs() < 1e-5, "{view:?}");
        assert!((view.width() - 1.0 / 2.5).abs() < 1e-5, "{view:?}");
        assert!(!t.is_off_framing(), "it has not been moved yet");
    }

    /// Resetting a crop means going back to the crop. Going to the whole
    /// picture would be going to the *parent* — a different camera, under this
    /// camera's name.
    #[test]
    fn resetting_a_virtual_camera_returns_to_its_own_framing() {
        let mut t = cropped_tile(2.5, (0.62, 0.38));
        t.zoom = 6.0;
        t.focus = egui::pos2(0.2, 0.9);
        assert!(t.is_off_framing(), "moved, so there is something to reset");
        t.reset_zoom();
        assert_eq!(t.digital_zoom(), 2.5);
        assert!((t.view().center().x - 0.62).abs() < 1e-5);
        assert!(!t.is_off_framing());
    }

    /// A framing saved off a tile has to be the one that was on screen, which
    /// is the clamped one — otherwise a crop dragged into the corner is written
    /// down as running off the edge and opens somewhere else next time.
    #[test]
    fn the_framing_saved_is_the_framing_shown() {
        let mut t = cropped_tile(4.0, (0.5, 0.5));
        t.focus = egui::pos2(-3.0, 9.0);
        let (zoom, centre) = t.framing();
        assert_eq!(zoom, 4.0);
        assert!((centre.0 - 0.125).abs() < 1e-5, "{centre:?}");
        assert!((centre.1 - 0.875).abs() < 1e-5, "{centre:?}");

        // And adopting it back is a fixed point: what was saved is where it
        // reopens.
        let mut again = cropped_tile(zoom, centre);
        again.reset_zoom();
        assert_eq!(again.framing(), (zoom, centre));
    }

    /// A saved framing is a starting point, not a cage: the cell-fill crop
    /// still narrows it, exactly as it narrows a zoom set by hand.
    #[test]
    fn a_saved_framing_composes_with_the_cell_crop() {
        let t = cropped_tile(2.0, (0.5, 0.5));
        let frame = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(200.0, 100.0));
        let drawn = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(100.0, 100.0));
        let shown = crop(t.view(), frame, drawn);
        assert!(shown.width() < t.view().width(), "narrower than the framing alone");
        assert!((shown.height() - t.view().height()).abs() < 1e-4, "and no shorter");
        assert!((shown.center() - t.view().center()).length() < 1e-4, "about the same point");
    }

    /// An unzoomed tile is exactly the case the box exists for, so it has to
    /// open with something you can see and grab rather than nothing.
    #[test]
    fn framing_an_unzoomed_tile_opens_a_box_you_can_grab() {
        let mut t = tile();
        assert_eq!(t.digital_zoom(), 1.0);
        t.begin_framing();
        assert!(t.is_framing());

        let framing = t.framing.expect("a box");
        let rect = framing.rect();
        assert!((rect.width() - 0.5).abs() < 1e-5, "half the picture: {rect:?}");
        assert!((rect.center().x - 0.5).abs() < 1e-5);
        assert_eq!(framing.framing().0, 2.0, "which is a 2x crop");
    }

    /// A tile somebody has already zoomed opens with the box around what they
    /// were looking at, so accepting it is the only thing left to do.
    #[test]
    fn framing_a_zoomed_tile_opens_around_what_is_on_screen() {
        let mut t = tile();
        t.zoom_to_for_test(2.5, (0.62, 0.38));
        t.begin_framing();

        let (zoom, centre) = t.framing.expect("a box").framing();
        assert!((zoom - 2.5).abs() < 1e-4, "{zoom}");
        assert!((centre.0 - 0.62).abs() < 1e-4, "{centre:?}");
        assert!((centre.1 - 0.38).abs() < 1e-4, "{centre:?}");
    }

    /// The box may not leave the picture, however it is pushed — the stored
    /// form cannot express a crop that runs off the edge, and a box drawn
    /// somewhere the crop cannot go is a box that lies.
    #[test]
    fn the_box_stays_inside_the_picture() {
        for (size, centre) in [
            (0.5, egui::pos2(-4.0, 9.0)),
            (0.5, egui::pos2(1.5, 1.5)),
            (0.25, egui::pos2(0.0, 0.0)),
            (2.0, egui::pos2(0.5, 0.5)),
            (0.001, egui::pos2(0.5, 0.5)),
        ] {
            let framing = Framing { size, centre, grabbed: None, moving: false };
            let rect = framing.rect();
            assert!(rect.min.x >= -f32::EPSILON, "{size} {centre:?} -> {rect:?}");
            assert!(rect.min.y >= -f32::EPSILON, "{size} {centre:?} -> {rect:?}");
            assert!(rect.max.x <= 1.0 + f32::EPSILON, "{size} {centre:?} -> {rect:?}");
            assert!(rect.max.y <= 1.0 + f32::EPSILON, "{size} {centre:?} -> {rect:?}");
            // And never past the magnification the tile would refuse to draw.
            let (zoom, _) = framing.framing();
            assert!(zoom <= MAX_DIGITAL_ZOOM + 1e-3, "{zoom}");
            assert!(zoom >= 1.0 - 1e-3, "{zoom}");
        }
    }

    /// Dragging a corner leaves the opposite one where it was, which is what
    /// every crop tool does and what makes the gesture predictable.
    #[test]
    fn dragging_a_corner_pins_the_opposite_one() {
        let mut framing = Framing {
            size: 0.4,
            centre: egui::pos2(0.5, 0.5),
            grabbed: None,
            moving: false,
        };
        let before = framing.rect();
        let pinned = egui::pos2(before.left(), before.top());

        // Pull the bottom-right corner further out.
        framing.drag_corner((true, true), egui::pos2(0.9, 0.9));
        let after = framing.rect();
        assert!((after.left() - pinned.x).abs() < 1e-4, "{after:?}");
        assert!((after.top() - pinned.y).abs() < 1e-4, "{after:?}");
        assert!(after.width() > before.width(), "and it grew");
        assert!((after.width() - after.height()).abs() < 1e-5, "still the picture's shape");
    }

    /// The shape is fixed, so a corner dragged well off one axis still follows
    /// the hand rather than stalling on the smaller distance.
    #[test]
    fn a_corner_follows_the_longer_pull() {
        let mut framing = Framing {
            size: 0.2,
            centre: egui::pos2(0.5, 0.5),
            grabbed: None,
            moving: false,
        };
        let fixed = framing.rect().min;
        framing.drag_corner((true, true), egui::pos2(fixed.x + 0.6, fixed.y + 0.05));
        assert!((framing.rect().width() - 0.6).abs() < 1e-4, "{:?}", framing.rect());
    }

    /// Accepting hands back the crop and ends the interaction; cancelling ends
    /// it and hands back nothing.
    #[test]
    fn a_box_is_kept_once_or_thrown_away() {
        let mut t = tile();
        t.begin_framing();
        let kept = t.accept_framing().expect("a crop");
        assert_eq!(kept.0, 2.0);
        assert!(!t.is_framing(), "accepting ends it");
        assert!(t.accept_framing().is_none(), "and there is nothing left to take");

        t.begin_framing();
        t.cancel_framing();
        assert!(!t.is_framing());
        assert!(t.accept_framing().is_none());
    }

    /// The box's coordinates and the screen's meet in exactly one place, so
    /// that place is worth pinning down: a crop of the middle quarter lands in
    /// the middle quarter of wherever the picture was painted, whatever shape
    /// the cell is and wherever on screen it sits.
    #[test]
    fn the_box_lands_where_the_picture_is() {
        let picture = Rect::from_min_size(egui::pos2(100.0, 40.0), Vec2::new(640.0, 360.0));
        let middle = Rect::from_center_size(egui::pos2(0.5, 0.5), Vec2::splat(0.5));
        let on_screen = to_screen(middle, picture);

        assert!((on_screen.center() - picture.center()).length() < 1e-3);
        assert!((on_screen.width() - 320.0).abs() < 1e-3, "{on_screen:?}");
        assert!((on_screen.height() - 180.0).abs() < 1e-3, "{on_screen:?}");

        // The whole picture maps to the whole picture, which is the identity
        // the dimming relies on to leave no seam at the edges.
        let all = to_screen(WHOLE_PICTURE, picture);
        assert!((all.min - picture.min).length() < 1e-3);
        assert!((all.max - picture.max).length() < 1e-3);
    }

    /// A camera with no frame yet has nothing to put a box on, and the menu
    /// asks this before it offers.
    #[test]
    fn a_tile_with_no_picture_has_nothing_to_frame() {
        assert!(!tile().has_picture());
    }

    /// Scroll on a crop must never reach the lens. Driving a zoom motor would
    /// move the framing this camera is defined by — and move it on every other
    /// view of the same camera at the same time.
    #[test]
    fn scroll_on_a_virtual_camera_never_drives_the_lens() {
        let mut channel = Channel::new(0);
        channel.optical_zoom_supported = true;

        // The same picture, as the camera itself, does hand it up.
        let parent = Tile::new(SourceId::camera("dev", 0), channel.clone(), "Drive".into());
        assert!(parent.drives_lens());

        let crop = Tile::cropped(
            SourceId::virtual_camera("dev", 0, "gate"),
            channel.clone(),
            "Gate".into(),
            2.0,
            (0.5, 0.5),
        );
        assert!(!crop.drives_lens(), "a crop zooms itself");

        // And the same for a dual-lens camera, whose lenses *are* the zoom
        // steps: switching them under a crop would reframe it onto something
        // else entirely.
        let mut dual = Channel::new(0);
        dual.lens = crate::api::Lens::Wide;
        dual.lens_partner = Some(1);
        assert!(Tile::new(SourceId::camera("dev", 0), dual.clone(), "Drive".into()).drives_lens());
        assert!(!Tile::cropped(
            SourceId::virtual_camera("dev", 0, "gate"),
            dual,
            "Gate".into(),
            2.0,
            (0.5, 0.5)
        )
        .drives_lens());
    }

    /// The whole point of zooming at the cursor: whatever pixel is under the
    /// pointer before the scroll is under it afterwards.
    #[test]
    fn zooming_keeps_the_point_under_the_cursor() {
        let mut t = tile();
        for (cursor, steps) in [
            (egui::vec2(0.25, 0.75), [2.0f32, 2.0, 1.5]),
            (egui::vec2(0.5, 0.5), [3.0, 1.25, 2.0]),
            (egui::vec2(0.4, 0.35), [1.6, 2.5, 1.1]),
        ] {
            t.reset_zoom();
            for factor in steps {
                let view = t.view();
                // The image point the cursor covers right now.
                let anchor = view.min
                    + egui::vec2(cursor.x * view.width(), cursor.y * view.height());

                t.apply_zoom(cursor, factor);

                // Where that same screen position now lands.
                let after = t.view();
                let landed = after.min
                    + egui::vec2(cursor.x * after.width(), cursor.y * after.height());
                assert!(
                    (landed.x - anchor.x).abs() < 1e-4 && (landed.y - anchor.y).abs() < 1e-4,
                    "cursor {cursor:?} at {:.2}x: {anchor:?} drifted to {landed:?}",
                    t.zoom
                );
            }
        }
    }

    #[test]
    fn scroll_for_a_camera_that_zooms_itself_is_handed_up_once() {
        let mut t = tile();
        t.zoom_request = 120.0;
        assert_eq!(t.take_zoom_request(), 120.0);
        assert_eq!(t.take_zoom_request(), 0.0, "taking it must clear it");
    }
}
