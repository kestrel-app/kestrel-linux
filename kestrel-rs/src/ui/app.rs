//! The application shell: header, camera sidebar, and the live grid.

use log::debug;
use eframe::egui::{self, Rect, RichText, Vec2};

use super::device_dialog::{DeviceDialog, Outcome};
use super::grid::{Key, Streams};
use super::playback::PlaybackView;
use super::preferences::{Outcome as PrefsOutcome, PreferencesDialog};
use super::rail::RailState;
use super::theme;
use super::tile::Tile;
use super::weather::WeatherView;
use crate::api::{SourceId, StreamType};
use crate::config::{ConfigStore, DeviceConfig, VirtualCamera};
use crate::events::{EventPoller, Follower};
use crate::notify::Notifier;
use crate::manager::{DeviceManager, Source};
use crate::weather::poller::{RadarPoller, RadarSettings, WeatherPoller};

pub const HEADER_HEIGHT: f32 = 54.0;

/// Supported grid densities, as tile counts.
const LAYOUTS: [usize; 5] = [1, 4, 6, 9, 16];

/// A sweep lands every couple of minutes, and somebody watching weather come in
/// is exactly the person who wants the new one. This only refreshes the list of
/// times — the pictures follow it, per tile, as they are needed.
const RADAR_INTERVAL: std::time::Duration = std::time::Duration::from_secs(150);

/// Turn a termination signal into a normal window close.
///
/// Reolink holds a login session until its lease expires and does not release
/// one when the socket dies, so a process killed with SIGTERM — a logout, a
/// system shutdown, `timeout`, a service manager — leaks a session for an hour.
/// Enough of those and the device refuses new logins entirely. Closing the
/// window properly runs the normal shutdown path, which logs out.
fn install_signal_handler(ctx: eframe::egui::Context) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let Ok(mut signals) = Signals::new([SIGINT, SIGTERM]) else { return };
            if signals.forever().next().is_some() {
                log::info!("termination signal received, shutting down cleanly");
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                // If the UI cannot close for some reason, do not hang forever;
                // a clean logout takes well under a second.
                std::thread::sleep(std::time::Duration::from_secs(6));
                log::warn!("clean shutdown timed out, exiting anyway");
                std::process::exit(0);
            }
        })
        .ok();
}

/// What a tile's context menu asked for.
/// How much of the window a toast may take before its text wraps.
///
/// Toasts are mostly a few words and want one line; the ones that are not are
/// the ones naming a file just written, which are worth reading whole. So the
/// bound is the window rather than a fixed width — on a 4K wall that is room
/// for a long path, and on a small window it is still most of what there is.
const TOAST_SHARE: f32 = 0.6;
/// The chip's own padding, taken off the share so the *chip* fits the share
/// rather than the text inside it.
const TOAST_PADDING: f32 = 32.0;
/// Never wrap narrower than this, however small the window is.
const TOAST_LEAST: f32 = 220.0;

/// How wide a toast's text may run before it wraps, in a window this wide.
fn toast_room(window: f32) -> f32 {
    (window * TOAST_SHARE - TOAST_PADDING).max(TOAST_LEAST)
}

/// Scroll banked before a camera-driven zoom fires. One wheel notch is about
/// 50 units of smooth scroll, so this is roughly a single deliberate click.
const SCROLL_ZOOM_THRESHOLD: f32 = 40.0;

#[derive(Clone, Copy)]
enum TileAction {
    ToggleExpand,
    Fullscreen,
    Snapshot,
    Record(bool),
    Reconnect,
    Lens(bool),
    ResetZoom,
    /// Take this camera off the wall, or put it back.
    SetHidden(bool),
    /// Let follow-motion bring this camera up, or leave it out.
    SetFollowed(bool),
    /// Scroll on a camera that can zoom itself, in wheel notches.
    OpticalZoom(f32),
    /// Keep this tile's framing as a camera of its own.
    CreateVirtual,
    /// Write the crop this tile is showing back to the camera it belongs to.
    SaveFraming,
    /// Rename this virtual camera, or type its framing in.
    EditVirtual,
    DeleteVirtual,
}

/// Which stream each camera on the wall should be pulling.
///
/// Decided per camera rather than per tile, because a camera and its crops
/// share one connection and so cannot want different halves of it. A crop asks
/// for the main stream: it exists to magnify, and there is nothing in a 640x360
/// sub stream to magnify — a 4x view of one is 160x90 filling a cell.
///
/// Main wins wherever the two disagree. Giving a crop the sub stream to spare
/// its parent the bandwidth would make the crop useless, which is the one thing
/// the parent was already fine without.
fn wanted_streams(
    wall: &[Source],
    grid: StreamType,
    promote: bool,
) -> std::collections::HashMap<Key, StreamType> {
    let mut out: std::collections::HashMap<Key, StreamType> = std::collections::HashMap::new();
    for source in wall {
        let asked = if source.is_virtual() && promote {
            StreamType::Main
        } else {
            grid
        };
        let slot = out.entry(source.stream_key()).or_insert(asked);
        if asked == StreamType::Main {
            *slot = StreamType::Main;
        }
    }
    out
}

/// Cut a JPEG down to one part of itself, for a virtual camera's snapshot.
///
/// The rectangle is the same one the tile is drawing: a magnification and a
/// centre in the picture's own coordinates, clamped so the crop stays inside
/// the frame the way the picture on screen does.
///
/// `None` when the bytes are not a picture this can read, which the caller
/// treats as a reason to save the whole still rather than as a failure.
fn crop_jpeg(bytes: &[u8], zoom: f32, centre: (f32, f32)) -> Option<Vec<u8>> {
    use image::ImageEncoder;

    let decoded = image::load_from_memory(bytes).ok()?;
    let (width, height) = (decoded.width(), decoded.height());
    if width == 0 || height == 0 {
        return None;
    }

    let half = 0.5 / zoom.max(1.0);
    let cx = centre.0.clamp(half, 1.0 - half);
    let cy = centre.1.clamp(half, 1.0 - half);
    // At least one pixel each way: a crop rounded down to nothing is not a
    // picture, and every zoom this can be asked for is far from that anyway.
    let w = (((2.0 * half) * width as f32).round() as u32).clamp(1, width);
    let h = (((2.0 * half) * height as f32).round() as u32).clamp(1, height);
    let x = (((cx - half) * width as f32).round() as u32).min(width - w);
    let y = (((cy - half) * height as f32).round() as u32).min(height - h);

    let cropped = decoded.crop_imm(x, y, w, h).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92)
        .write_image(
            cropped.as_raw(),
            cropped.width(),
            cropped.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(out)
}

/// How the wall lays out, and which cell the radar takes.
///
/// Returns the columns, the rows, and the radar's slot if it gets one — always
/// straight after the last camera, so the cameras keep the order the sidebar
/// lists them in.
///
/// The modes differ in whether what they show is counted before the shape is
/// chosen. `Always` counts it, which is what turns four cameras into a 3x2
/// rather than a 2x2: no camera is dropped, they are all a little smaller.
/// `Spare` does not, and takes a cell only where the cameras' own layout left
/// one over — so no camera ever changes size for it, and a wall that tiles
/// exactly shows nothing extra at all.
///
/// `reserved` is how many cells the forecast has been promised, which is zero
/// unless it is set to always take them. It is counted the same way the radar's
/// `Always` is — and it is counted *against* the radar's `Spare`, because a
/// cell that only exists because the forecast asked for it is not a cell the
/// cameras left over. The radar's promise is that nothing shrinks a camera for
/// *it*, and that still holds.
///
/// Split out from the drawing because that is the whole of the rule, and the
/// answers are easy to get the wrong way round.
fn wall_layout(
    cameras: usize,
    mode: crate::config::RadarTile,
    reserved: usize,
) -> (usize, usize, Option<usize>) {
    use crate::config::RadarTile;

    let promised = usize::from(mode == RadarTile::Always) + reserved;
    let count = (cameras + promised).max(1);
    // 6-up reads better as 3x2 than 2x3 on a widescreen display.
    let columns = if count == 6 {
        3
    } else {
        (count as f32).sqrt().ceil() as usize
    }
    .max(1);
    let rows = count.div_ceil(columns);

    let slot = match mode {
        RadarTile::Never => None,
        RadarTile::Always => Some(cameras),
        RadarTile::Spare => (columns * rows > cameras + reserved).then_some(cameras),
    };
    (columns, rows, slot)
}

/// How many cells the cameras and the radar left over — and so how many
/// forecast periods the wall can show.
///
/// The forecast is what goes in them, one period per cell, in the order the
/// service sends them: the spare cell on a five-camera wall is tonight, and a
/// seven-camera wall gets tonight and tomorrow. Nine cameras in a 3x3 leave
/// nothing and nothing is what appears.
///
/// Split out from the drawing for the same reason [`wall_layout`] is: it is the
/// whole of the rule, and the alternative is arithmetic buried in a loop that
/// nothing can check.
fn spare_cells(columns: usize, rows: usize, cameras: usize, radar: bool) -> usize {
    (columns * rows).saturating_sub(cameras + usize::from(radar))
}

/// Why follow-motion could never bring a camera up, given how it is set up.
///
/// Two ways to switch it off without meaning to, and neither leaves a mark:
/// clearing the detection types in preferences, and leaving every camera out of
/// it one right-click at a time. The preferences pane warns about the first
/// where it is set; the second had nowhere to be said at all, and a mode that is
/// on and cannot fire looks exactly like a quiet night.
///
/// Split out from the app for the same reason [`wall_layout`] is: it is the
/// whole of the rule, and it is the sentence somebody reads at the moment they
/// are wondering why nothing is happening.
fn nothing_to_follow(kinds: usize, cameras: usize, left_out: usize) -> Option<String> {
    if kinds == 0 {
        return Some("no detection types are chosen for it in preferences".into());
    }
    // An empty wall is not this feature's problem to report.
    if cameras == 0 || left_out < cameras {
        return None;
    }
    Some(if cameras == 1 {
        "the only camera is left out of it".into()
    } else {
        format!("all {cameras} cameras are left out of it")
    })
}

/// Fold a device into a sorted list of folded ones, or take it out. Returns
/// whether anything changed.
///
/// The same shape hiding a channel and exempting one from following use: a
/// sorted list of exceptions with no duplicates, so the config file reads
/// sensibly and two runs that fold the same boxes write the same document.
///
/// Split out from the app because it is the whole of the rule and the state it
/// edits outlives the session - the alternative is arithmetic on a preference
/// that nothing can check.
fn fold_device(list: &mut Vec<String>, device: &str, folded: bool) -> bool {
    let had = list.iter().any(|id| id == device);
    if folded == had {
        return false;
    }
    if folded {
        list.push(device.to_string());
        list.sort();
    } else {
        list.retain(|id| id != device);
    }
    true
}

/// The gap between a device's disclosure chevron and its name, in points.
///
/// Set against the 14px the channel rows are indented by rather than picked by
/// eye: the chevron marks the level the name sits at, and a gap wider than the
/// indent below it would read as a bigger step than the one it describes.
const CHEVRON_GAP: f32 = 7.0;

/// A short fade rather than a cut: names blinking out of sixteen tiles at once
/// catches the eye exactly when nothing has happened.
const TITLE_FADE: f32 = 0.25;

/// Whether names float over the picture, and how visible they are.
///
/// Split out from the app so the timing is testable without a running UI.
fn title_visibility(
    auto_hide: bool,
    idle_seconds: f32,
    delay: f32,
    pointer_present: bool,
) -> (bool, f32) {
    if !auto_hide {
        return (false, 1.0);
    }
    // A pointer that has left the window cannot be "about to do something", so
    // there is nothing to label: hide at once rather than waiting out the delay.
    if !pointer_present {
        return (true, 0.0);
    }
    let delay = delay.max(0.0);
    let alpha = if idle_seconds <= delay {
        1.0
    } else {
        1.0 - ((idle_seconds - delay) / TITLE_FADE)
    };
    (true, alpha.clamp(0.0, 1.0))
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Live,
    Playback,
    /// Only reachable while the weather is switched on — see
    /// [`KestrelApp::tabs`].
    Weather,
}

/// Naming a new virtual camera, or editing one that exists.
///
/// One dialog for both, because they ask the same questions: what to call this
/// framing, and what the framing is. Creating fills the numbers in from the
/// tile that was right-clicked; editing fills them in from what was saved, and
/// lets them be typed.
struct VirtualDialog {
    /// The camera this crops into.
    parent: SourceId,
    /// The virtual camera being edited, or `None` while making a new one.
    editing: Option<String>,
    name: String,
    zoom: f32,
    centre: (f32, f32),
    /// Whether the framing can be typed rather than only read.
    ///
    /// Creating shows it read-only: it was just set by hand on the picture, and
    /// a number field beside a framing somebody can see is an invitation to
    /// argue with their own eyes.
    editable: bool,
}

impl VirtualDialog {
    fn title(&self) -> &'static str {
        if self.editing.is_some() {
            "Virtual camera"
        } else {
            "New virtual camera"
        }
    }
}

pub struct KestrelApp {
    store: ConfigStore,
    manager: DeviceManager,
    streams: Streams,

    tab: Tab,
    capacity: usize,
    page: usize,
    tiles: Vec<Tile>,
    selected: Option<SourceId>,
    /// One camera filling the pane, by double-click.
    maximized: Option<SourceId>,
    sources_revision: usize,
    status: Option<(String, std::time::Instant)>,
    rail: RailState,
    dialog: DeviceDialog,

    poller: EventPoller,
    follower: Follower,
    /// Whether the camera list is showing. A mode rather than a setting - the
    /// preference decides where it starts, not where it stays.
    sidebar_open: bool,
    notifier: Notifier,
    /// Cameras follow-motion wants shown, overriding paging.
    ///
    /// Held as views rather than channels: a channel that detects may be
    /// wanted on the wall as itself, as one of its crops, or as several of
    /// them, and that choice is made once here rather than at every place the
    /// spotlight is read.
    spotlight: Vec<SourceId>,
    playback: PlaybackView,
    fullscreen: bool,
    prefs: PreferencesDialog,
    about_open: bool,
    /// The virtual camera being made or edited, if either is happening.
    virtual_dialog: Option<VirtualDialog>,
    /// Requested from a context menu, applied once we have the egui context.
    pending_fullscreen: Option<bool>,
    /// For dual-lens cameras: which physical channel a source is streaming.
    /// Keyed by the wide channel, since that is the one the grid lists.
    lens_override: std::collections::HashMap<Key, u32>,
    /// Scroll notches banked per camera, so a flick of the wheel becomes one
    /// action rather than a dozen.
    zoom_accumulator: std::collections::HashMap<Key, f32>,
    /// The camera the sound is currently coming from, tracked only so a change
    /// can be logged. What is audible is always the selected camera.
    listening: Option<SourceId>,
    /// Screenshot aid: pretend every camera is detecting everything.
    demo_badges: bool,
    /// When the pointer last moved, for fading camera names out.
    last_pointer_move: std::time::Instant,
    /// Whether the pointer is over the window at all.
    pointer_present: bool,
    /// The area the camera grid occupies, so the pointer can be hidden over the
    /// pictures without hiding it over the surrounding controls.
    grid_rect: Option<Rect>,
    /// Capture the window to a file after N frames, then exit. Used to review
    /// the interface without a person having to look at it.
    screenshot: Option<(std::path::PathBuf, u32)>,

    /// The weather, which is not a device: one poller for the app, whatever is
    /// showing. `None` when it is switched off or has nowhere to point.
    weather: WeatherView,
    weather_poller: Option<WeatherPoller>,
    /// Only running while the radar is actually on screen — a pass is over a
    /// megabyte, which is worth it for something being watched and not for
    /// something nobody is looking at.
    radar_poller: Option<RadarPoller>,
    /// The radar drawn in a cell on the camera wall. Its own view of the same
    /// map: the tab and the wall want different sizes, and a tile grid does not
    /// care, so the only thing they share is where they are looking.
    radar_tile_view: super::radar::RadarView,
    /// Where the radar is pointed. Shared, so dragging the wall cell and
    /// opening the tab show the same place — and session state rather than a
    /// preference, because looking at the next county along is not a statement
    /// about where you live. `None` until there is somewhere to point it.
    radar_view: Option<crate::weather::tiles::Viewport>,
    /// Asks the desktop not to blank the screen while this is fullscreen.
    inhibitor: crate::power::Inhibitor,
    /// Whether the wall drew the radar this frame, set while the grid is drawn
    /// and read once it has been.
    radar_on_the_wall: bool,
}

impl KestrelApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        install_signal_handler(cc.egui_ctx.clone());

        let store = ConfigStore::load();
        store.ensure_media_dirs();

        let manager = DeviceManager::default();
        *manager.show_offline.lock().unwrap() = store.preferences.show_offline_channels;
        manager.set_configs(store.devices.clone());
        manager.connect_all();

        let mut streams = Streams::default();
        streams.warm_enabled = store.preferences.warm_streams;
        streams.max_warm = store.preferences.max_warm_streams;

        let capacity = if LAYOUTS.contains(&store.preferences.grid_size) {
            store.preferences.grid_size
        } else {
            4
        };

        let poller = EventPoller::start(
            manager.clone(),
            std::time::Duration::from_secs_f32(store.preferences.event_poll_seconds.max(0.5)),
        );
        let starts_open = store.preferences.sidebar_open;
        let mut follower = Follower::new(std::time::Duration::from_secs_f32(
            store.preferences.follow_dwell_seconds,
        ));
        // Deliberately starts off: following is a mode, not a saved preference.
        let notifier = Notifier::new(store.preferences.desktop_notifications);

        KestrelApp {
            store,
            manager,
            streams,
            tab: Tab::Live,
            capacity,
            page: 0,
            tiles: Vec::new(),
            selected: None,
            maximized: None,
            sources_revision: 0,
            status: None,
            rail: RailState::new(),
            dialog: DeviceDialog::default(),
            poller,
            follower,
            sidebar_open: starts_open,
            notifier,
            spotlight: Vec::new(),
            playback: PlaybackView::default(),
            fullscreen: false,
            prefs: PreferencesDialog::default(),
            about_open: false,
            virtual_dialog: None,
            pending_fullscreen: None,
            lens_override: std::collections::HashMap::new(),
            zoom_accumulator: std::collections::HashMap::new(),
            listening: None,
            demo_badges: std::env::var_os("KESTREL_DEMO_BADGES").is_some(),
            last_pointer_move: std::time::Instant::now(),
            pointer_present: true,
            grid_rect: None,
            weather: WeatherView::default(),
            // Started by the first reconcile, so there is one place that
            // decides whether it should be running.
            weather_poller: None,
            radar_poller: None,
            radar_tile_view: super::radar::RadarView::default(),
            radar_view: None,
            inhibitor: crate::power::Inhibitor::new(),
            radar_on_the_wall: false,
            screenshot: std::env::var("KESTREL_SCREENSHOT")
                .ok()
                .map(|path| {
                    // Long enough for streams to be showing video, overridable
                    // when a capture needs to be later or earlier.
                    let frames = std::env::var("KESTREL_SCREENSHOT_FRAMES")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(260);
                    (std::path::PathBuf::from(path), frames)
                }),
        }
    }

    fn notify(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), std::time::Instant::now()));
    }

    // ---------------------------------------------------------------- grid

    fn expanded_stream(&self) -> StreamType {
        if self.store.preferences.expanded_stream == "sub" {
            StreamType::Sub
        } else {
            StreamType::Main
        }
    }

    fn wanted_stream(&self, single_view: bool) -> StreamType {
        if single_view {
            self.expanded_stream()
        } else if self.store.preferences.live_substream {
            StreamType::Sub
        } else {
            StreamType::Main
        }
    }

    fn page_count(&self, total: usize) -> usize {
        total.div_ceil(self.capacity).max(1)
    }

    /// Position of the expanded camera in the full source list.
    fn expanded_index(&self) -> Option<usize> {
        let key = self.maximized.as_ref()?;
        self.manager
            .sources()
            .iter()
            .position(|s| &s.id == key)
    }

    /// Step forward or back: between cameras when one fills the pane, between
    /// pages otherwise.
    ///
    /// Expanding deliberately does not trap you on one camera — stepping
    /// crosses page boundaries freely, because paging is meaningless while a
    /// single camera is shown.
    fn step(&mut self, delta: isize) {
        let sources = self.manager.sources();
        if sources.is_empty() {
            return;
        }

        if self.maximized.is_some() {
            let Some(index) = self.expanded_index() else { return };
            let count = sources.len() as isize;
            let next = ((index as isize + delta) % count + count) % count;
            self.maximized = Some(sources[next as usize].id.clone());
            self.selected = self.maximized.clone();
        } else {
            let pages = self.page_count(sources.len()) as isize;
            self.page = (((self.page as isize + delta) % pages + pages) % pages) as usize;
        }
        self.rebuild();
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        self.fullscreen = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    /// Escape unwinds one level at a time: fullscreen, then the expanded view.
    fn on_escape(&mut self, ctx: &egui::Context) {
        // Innermost thing first, the way the rest of this already unwinds. A
        // box on a picture is the smallest thing on screen and the one Esc is
        // most likely aimed at while it is there.
        if self.cancel_framing() {
            return;
        }
        if self.fullscreen {
            self.set_fullscreen(ctx, false);
        } else if self.maximized.is_some() {
            self.maximized = None;
            self.rebuild();
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (escape, enter, f11, left, right, ctrl_b, ctrl_s, ctrl_r, one, two, three) = ctx
            .input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::F11),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.modifiers.ctrl && i.key_pressed(egui::Key::B),
                i.modifiers.ctrl && i.key_pressed(egui::Key::S),
                i.modifiers.ctrl && i.key_pressed(egui::Key::R),
                i.modifiers.ctrl && i.key_pressed(egui::Key::Num1),
                i.modifiers.ctrl && i.key_pressed(egui::Key::Num2),
                i.modifiers.ctrl && i.key_pressed(egui::Key::Num3),
            )
        });

        if escape {
            self.on_escape(ctx);
        }
        if f11 {
            let target = !self.fullscreen;
            self.set_fullscreen(ctx, target);
        }
        // Not inside the Live branch: the camera list is on every tab, and the
        // top bar can be set to take itself away, in which case this is the
        // only way back to it.
        if ctrl_b {
            self.sidebar_open = !self.sidebar_open;
        }
        if self.tab == Tab::Live {
            if enter && self.tiles.iter().any(|t| t.is_framing()) {
                self.accept_framing();
            }
            if left {
                self.step(-1);
            }
            if right {
                self.step(1);
            }
            if ctrl_s {
                self.take_snapshot();
            }
            if ctrl_r {
                let recording = self
                    .selected
                    .as_ref()
                    .and_then(|key| self.tiles.iter().find(|t| &t.key() == key))
                    .and_then(|t| t.stream.as_ref())
                    .map(|s| s.recording_path().is_some())
                    .unwrap_or(false);
                self.toggle_recording(!recording);
            }
        }
        if one {
            self.tab = Tab::Live;
        }
        if two {
            self.tab = Tab::Playback;
        }
        // Only where the tab exists, so the shortcut cannot reach a pane the
        // switch above it does not offer.
        if three && self.store.preferences.weather_usable() {
            self.tab = Tab::Weather;
        }
    }

    /// Rebuild the visible tiles, reusing every connection that survives.
    ///
    /// Reconnecting an RTSP stream costs seconds — measured at 6.1s to first
    /// frame on an RLN36 — so a camera that stays on screen through a layout
    /// change or a page turn must keep the stream it already has.
    fn rebuild(&mut self) {
        let sources = self.manager.sources();
        if sources.is_empty() {
            for tile in std::mem::take(&mut self.tiles) {
                self.streams.park_tile(tile);
            }
            self.streams.release(&std::collections::HashSet::new());
            return;
        }

        let single_view =
            self.maximized.is_some() || (self.spotlight.len() == 1 && !self.spotlight.is_empty());
        let wanted: Vec<Source> = if let Some(key) = &self.maximized {
            sources.iter().filter(|s| &s.id == key).cloned().collect()
        } else if !self.spotlight.is_empty() {
            // Follow-motion shows exactly the active cameras, ignoring paging.
            let wanted: Vec<Source> = sources
                .iter()
                .filter(|s| self.spotlight.contains(&s.id))
                .cloned()
                .collect();
            if wanted.is_empty() {
                self.spotlight.clear();
                sources
                    .iter()
                    .skip(self.page * self.capacity)
                    .take(self.capacity)
                    .cloned()
                    .collect()
            } else {
                wanted
            }
        } else {
            let pages = self.page_count(sources.len());
            self.page = self.page.min(pages - 1);
            sources
                .iter()
                .skip(self.page * self.capacity)
                .take(self.capacity)
                .cloned()
                .collect()
        };

        let wanted_ids: Vec<SourceId> = wanted.iter().map(|s| s.id.clone()).collect();

        // Park tiles that are leaving the screen. Their connections are not
        // theirs to give up — that is settled once, below, against the whole
        // rebuilt wall, because a camera whose own tile just left may still
        // have a crop of it on screen.
        let mut kept: Vec<Tile> = Vec::new();
        for tile in std::mem::take(&mut self.tiles) {
            if wanted_ids.contains(&tile.key()) {
                kept.push(tile);
            } else {
                self.streams.park_tile(tile);
            }
        }

        let grid_stream = self.wanted_stream(single_view);
        let per_camera = wanted_streams(
            &wanted,
            grid_stream,
            self.store.preferences.virtual_cameras_use_main_stream,
        );

        let mut rebuilt = Vec::with_capacity(wanted.len());
        for source in wanted {
            let id = source.id.clone();
            let stream = per_camera
                .get(&source.stream_key())
                .copied()
                .unwrap_or(grid_stream);
            let mut tile = match kept.iter().position(|t| t.key() == id) {
                Some(index) => kept.remove(index),
                None => self
                    .streams
                    .unpark_tile(&id)
                    .unwrap_or_else(|| self.new_tile(&source)),
            };

            if tile.is_streaming() && tile.active_stream == stream {
                // Already showing exactly what this layout wants.
                rebuilt.push(tile);
                continue;
            }
            if stream != StreamType::Main {
                self.streams.cancel_upgrade(&source.stream_key());
            }
            self.connect_tile(&mut tile, stream);
            rebuilt.push(tile);
        }
        for leftover in kept {
            self.streams.park_tile(leftover);
        }
        self.tiles = rebuilt;

        // The wall is built; now settle what it left connected. The tiles on
        // screen are the refcount, which is why this is one call at the end
        // rather than a decision at each of the places a tile can leave.
        let live: std::collections::HashSet<Key> =
            self.tiles.iter().map(|t| t.stream_key()).collect();
        self.streams.release(&live);
        self.streams.schedule_warm();

        if self.selected.is_none() {
            // KESTREL_SELECT_CHANNEL preselects a channel, so a screenshot can
            // show controls that only appear for particular cameras.
            let wanted: Option<u32> = std::env::var("KESTREL_SELECT_CHANNEL")
                .ok()
                .and_then(|v| v.parse().ok());
            self.selected = wanted
                .and_then(|index| self.tiles.iter().find(|t| t.channel.index == index))
                .or_else(|| self.tiles.first())
                .map(|t| t.key());
        }
    }

    /// A fresh tile for one camera, opened at its saved crop when it has one.
    fn new_tile(&self, source: &Source) -> Tile {
        let title = self.manager.source_label(source);
        match &source.view {
            Some(view) => Tile::cropped(
                source.id.clone(),
                source.channel.clone(),
                title,
                view.zoom,
                view.centre,
            ),
            None => Tile::new(source.id.clone(), source.channel.clone(), title),
        }
    }

    /// Give a tile the stream it should be showing, as cheaply as possible.
    fn connect_tile(&mut self, tile: &mut Tile, stream: StreamType) {
        let key = tile.stream_key();

        // Already running for this camera and pulling what this tile wants —
        // which is how a virtual camera costs nothing: its parent, or a sibling
        // crop, has usually opened the connection already.
        if let Some((view, running)) = self.streams.hot_view(&key) {
            if running == stream {
                tile.attach(view, running);
                return;
            }
        }

        // A dual-lens tile may be showing its telephoto channel instead.
        let index = self
            .lens_override
            .get(&key)
            .copied()
            .unwrap_or(tile.channel.index);
        tile.on_tele = index != tile.channel.index;

        // Ask the device's own system where the video is. A channel that is a
        // number here is a name or a GUID elsewhere, so the channel goes over
        // whole rather than as an index.
        let mut lens_channel = tile.channel.clone();
        lens_channel.index = index;
        if index != tile.channel.index {
            // The telephoto half is a separate channel on the device.
            lens_channel.key = index.to_string();
        }
        let Some(source) = self
            .manager
            .stream_source(&key.0, &lens_channel, stream)
        else {
            return;
        };

        if stream == StreamType::Main {
            // Show the sub stream straight away — live, not a frozen frame —
            // and bring the main stream up behind it.
            // A warm stream is always the wide channel's sub stream, so it is
            // only usable when this tile is not on the telephoto lens.
            if !tile.on_tele {
                if let Some(warm) = self.streams.adopt_warm(&key) {
                    tile.attach(warm, StreamType::Sub);
                }
            }
            if let Some((view, StreamType::Sub)) = self.streams.hot_view(&key) {
                tile.attach(view, StreamType::Sub);
                self.streams.begin_upgrade(key, source, tile.title.clone());
                return;
            }
        }

        let view = self.streams.bind(key, stream, source, tile.title.clone());
        tile.attach(view, stream);
    }

    /// Swap in any main stream that has become ready behind its tile.
    ///
    /// Every tile of that camera moves across together — the camera itself and
    /// each crop of it — because they were all reading the one connection that
    /// is being replaced.
    fn apply_upgrades(&mut self) {
        for (key, worker) in self.streams.ready_upgrades() {
            if !self.tiles.iter().any(|t| t.stream_key() == key) {
                // The view moved on while it was connecting.
                self.streams.retirer.retire(worker);
                continue;
            }
            let view = self.streams.promote(key.clone(), worker);
            for tile in self.tiles.iter_mut().filter(|t| t.stream_key() == key) {
                tile.attach(view.clone(), StreamType::Main);
            }
        }
    }

    // ---------------------------------------------------------------- panels

    /// A short vertical rule between groups of controls.
    ///
    /// egui's own separator spans the full height of the bar, which cuts the
    /// toolbar into slabs. This is the same idea at half height and low
    /// contrast: enough to group, not enough to notice.
    fn divider(ui: &mut egui::Ui) {
        const HEIGHT: f32 = 16.0;
        ui.add_space(3.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, HEIGHT), egui::Sense::hover());
        let centre = rect.center();
        ui.painter().line_segment(
            [
                egui::pos2(centre.x, centre.y - HEIGHT / 2.0),
                egui::pos2(centre.x, centre.y + HEIGHT / 2.0),
            ],
            egui::Stroke::new(1.0, theme::BORDER_SOFT),
        );
        ui.add_space(3.0);
    }

    /// Live and Playback as one two-sided switch.
    ///
    /// As two separate labels the current tab was easy to miss. A single track
    /// with one lit half makes the choice and the state the same object, and
    /// clicking the dark half flips it.
    /// Which tabs exist right now.
    ///
    /// Weather is only one of them once it is switched on and has somewhere to
    /// read from: an empty pane behind a permanent tab would be an invitation
    /// to a screen that says nothing.
    fn tabs(&self) -> Vec<(&'static str, Tab)> {
        let mut tabs = vec![("Live", Tab::Live), ("Playback", Tab::Playback)];
        if self.store.preferences.weather_usable() {
            tabs.push(("Weather", Tab::Weather));
        }
        tabs
    }

    fn tab_switch(ui: &mut egui::Ui, current: Tab, options: &[(&str, Tab)]) -> Option<Tab> {
        const HEIGHT: f32 = 26.0;
        const PAD: f32 = 14.0;

        let font = egui::FontId::proportional(12.5);
        let widths: Vec<f32> = options
            .iter()
            .map(|(label, _)| {
                let galley = ui.fonts(|f| {
                    f.layout_no_wrap(label.to_string(), font.clone(), theme::TEXT)
                });
                galley.size().x + PAD * 2.0
            })
            .collect();

        let total: f32 = widths.iter().sum();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(total, HEIGHT), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        let radius = egui::CornerRadius::same((HEIGHT / 2.0) as u8);
        painter.rect_filled(rect, radius, theme::INK);

        let mut chosen = None;
        let mut x = rect.left();
        for (index, ((label, tab), width)) in options.iter().zip(&widths).enumerate() {
            let half = Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(*width, HEIGHT));
            x += width;

            let response = ui
                .interact(half, ui.id().with(("tab", index)), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let active = current == *tab;
            if active {
                painter.rect_filled(half, radius, theme::ACCENT);
            } else if response.hovered() {
                painter.rect_filled(half, radius, theme::PANEL);
            }
            painter.text(
                half.center(),
                egui::Align2::CENTER_CENTER,
                label,
                font.clone(),
                if active {
                    theme::INK
                } else if response.hovered() {
                    theme::TEXT
                } else {
                    theme::TEXT_DIM
                },
            );
            if response.clicked() && !active {
                chosen = Some(*tab);
            }
        }
        chosen
    }

    fn header(&mut self, ctx: &egui::Context) {
        let mut step = 0isize;
        let mut snapshot_requested = false;
        let mut record_request: Option<bool> = None;
        let mut listen_toggle = false;
        let mut follow_everything = false;
        let mut sidebar_toggle = false;

        // Snapshot and recording act on one camera, so they only make sense
        // once one is picked.
        let selected_tile = self
            .selected
            .as_ref()
            .and_then(|key| self.tiles.iter().find(|t| &t.key() == key));
        let camera_selected = self.tab == Tab::Live && selected_tile.is_some();
        let selected_recording = selected_tile
            .and_then(|t| t.stream.as_ref())
            .map(|s| s.recording_path().is_some())
            .unwrap_or(false);
        let selected_name = selected_tile.map(|t| t.title.clone()).unwrap_or_default();
        // Floodlight controls appear only on cameras that have one.
        let floodlight_channel = selected_tile
            .map(|t| t.channel.clone())
            .filter(|c| c.floodlight_supported);
        let floodlight_client = floodlight_channel
            .as_ref()
            .and_then(|_| self.selected.as_ref())
            .and_then(|key| self.manager.client(&key.device));
        let floodlight_device = self
            .selected
            .as_ref()
            .map(|key| key.device.clone())
            .unwrap_or_default();
        let audio_on = self.store.preferences.audio_enabled;
        let weather_on = self.store.preferences.weather_usable();
        let mut open_preferences = false;
        let mut open_about = false;
        let mut add_device = false;
        let mut reconnect_all = false;
        let mut settings_touched = false;
        let mut follow_toggle: Option<bool> = None;
        let mut quit = false;
        let mut fullscreen_request: Option<bool> = None;
        let mut layout_changed = false;
        let mut quality_changed = false;

        egui::TopBottomPanel::top("header")
            .exact_height(HEADER_HEIGHT)
            .frame(
                egui::Frame::none()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(14, 0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // Wordmark: drawn as text with wide tracking, matching the
                    // brand tool. A downscaled bitmap looks soft at this size.
                    ui.label(
                        RichText::new("K E S T R E L")
                            .strong()
                            .size(15.0)
                            .color(theme::TEXT),
                    );
                    ui.add_space(10.0);
                    Self::divider(ui);
                    ui.add_space(6.0);

                    let tabs = self.tabs();
                    if let Some(tab) = Self::tab_switch(ui, self.tab, &tabs) {
                        if self.tab == Tab::Playback {
                            // Stop the clip rather than leaving a decode and a
                            // device connection running out of sight.
                            self.playback.release();
                        }
                        self.tab = tab;
                    }

                    let row = egui::vec2(ui.available_width(), HEADER_HEIGHT);
                    ui.allocate_ui_with_layout(
                        row,
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                        // A ComboBox is naturally taller than a button, so its
                        // text drifts below the rest of the row. Pinning the
                        // interaction height makes every control share one
                        // centre line.
                        ui.spacing_mut().interact_size.y = 24.0;
                        ui.spacing_mut().button_padding = egui::vec2(8.0, 3.0);
                        let sources = self.manager.sources().len();
                        let pages = self.page_count(sources);
                        // Read before the menu, which takes preferences by
                        // &mut and cannot then ask the rest of self anything.
                        let left_out = self.unfollowed_count();

                        ui.menu_button("⋯", |ui| {
                            ui.set_min_width(210.0);
                            let _ = &selected_name;

                            if ui.button("Add camera or NVR…").clicked() {
                                add_device = true;
                                ui.close_menu();
                            }
                            if ui.button("Reconnect all").clicked() {
                                reconnect_all = true;
                                ui.close_menu();
                            }
                            ui.separator();

                            let prefs = &mut self.store.preferences;
                            let mut touched = false;

                            let mut following = self.follower.enabled;
                            if ui.checkbox(&mut following, "Follow motion").changed() {
                                follow_toggle = Some(following);
                            }
                            // Named with a count rather than left as a toggle
                            // nobody would think to try, the same way revealing
                            // hidden cameras is. Getting into this state is one
                            // stray click; getting out of it was one deliberate
                            // click per camera, with nothing on screen saying
                            // which.
                            if left_out > 0
                                && ui
                                    .button(format!(
                                        "Follow every camera again ({left_out} left out)"
                                    ))
                                    .clicked()
                            {
                                follow_everything = true;
                                ui.close_menu();
                            }

                            ui.menu_button("Grid stream quality", |ui| {
                                let mut sub = prefs.live_substream;
                                if ui
                                    .radio_value(&mut sub, true, "Sub stream (lighter)")
                                    .clicked()
                                    || ui
                                        .radio_value(&mut sub, false, "Main stream (sharper)")
                                        .clicked()
                                {
                                    prefs.live_substream = sub;
                                    layout_changed = true;
                                    touched = true;
                                    ui.close_menu();
                                }
                            });

                            touched |= ui
                                .checkbox(&mut prefs.warm_streams, "Keep cameras ready")
                                .on_hover_text(
                                    "Stay connected to off-screen cameras so they appear \
                                     instantly.",
                                )
                                .changed();

                            if ui
                                .checkbox(&mut prefs.show_offline_channels, "Show offline channels")
                                .changed()
                            {
                                layout_changed = true;
                                touched = true;
                            }

                            touched |= ui
                                .checkbox(&mut prefs.desktop_notifications, "Desktop notifications")
                                .changed();

                            // Only offered once there is a reading to put in
                            // it; the switch that turns the weather on lives in
                            // preferences, where the address goes.
                            if weather_on {
                                touched |= ui
                                    .checkbox(&mut prefs.weather_bar, "Weather strip")
                                    .on_hover_text(
                                        "A band of conditions above the cameras. The Weather \
                                         tab has the full reading either way.",
                                    )
                                    .changed();
                            }

                            if touched {
                                settings_touched = true;
                            }

                            ui.separator();
                            if ui.button("Preferences…").clicked() {
                                open_preferences = true;
                                ui.close_menu();
                            }
                            if ui.button("About Kestrel").clicked() {
                                open_about = true;
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui.button("Quit").clicked() {
                                quit = true;
                                ui.close_menu();
                            }
                        });

                        // Beside the follow and fullscreen buttons because it
                        // is the same kind of control: something about how the
                        // window is arranged rather than about a camera.
                        if ui
                            .selectable_label(self.sidebar_open, "◧")
                            .on_hover_text("Camera list  (Ctrl+B)")
                            .clicked()
                        {
                            sidebar_toggle = true;
                        }

                        let following = self.follower.enabled;
                        let mut hint = format!(
                            "Follow motion — show cameras that are detecting, \
                             holding each for {}s",
                            self.follower.dwell().as_secs()
                        );
                        // Said here because it is the only place the exclusion
                        // is visible at all: a camera left out simply never
                        // comes up, which is indistinguishable from a quiet one.
                        // A camera left out simply never comes up, which is
                        // indistinguishable from a quiet one - so this is the
                        // only place on screen the exclusion is visible at all,
                        // and it has to name the case where it accounts for
                        // everything rather than leave a count to be compared.
                        match self.why_following_cannot_fire() {
                            Some(why) => {
                                hint.push_str("\nIt cannot fire: ");
                                hint.push_str(&why);
                            }
                            None => match self.unfollowed_count() {
                                0 => {}
                                1 => hint.push_str("\n1 camera is left out"),
                                n => hint.push_str(&format!("\n{n} cameras are left out")),
                            },
                        }
                        if ui
                            .selectable_label(following, "◎")
                            .on_hover_text(hint)
                            .clicked()
                        {
                            follow_toggle = Some(!following);
                        }

                        // Fullscreen is available in either mode.
                        if ui
                            .selectable_label(self.fullscreen, "⇱")
                            .on_hover_text("Fullscreen  (F11)")
                            .clicked()
                        {
                            fullscreen_request = Some(!self.fullscreen);
                        }

                        let expanded = self.maximized.is_some();
                        if ui
                            .selectable_label(expanded, "▣")
                            .on_hover_text(if expanded {
                                "Back to grid  (double-click, or Esc)"
                            } else {
                                "Expand to fill  (double-click a camera)"
                            })
                            .clicked()
                        {
                            if expanded {
                                self.maximized = None;
                            } else {
                                self.spotlight.clear();
                                self.maximized =
                                    self.selected.clone().or_else(|| {
                                        self.tiles.first().map(|t| t.key())
                                    });
                            }
                            self.rebuild();
                        }

                        // The arrows step cameras while expanded and pages
                        // otherwise, so the counter and tooltips must say which.
                        let (label, steppable, forward, back) = if expanded {
                            let index = self.expanded_index().unwrap_or(0);
                            (
                                format!("{} / {}", index + 1, sources.max(1)),
                                sources > 1,
                                "Next camera",
                                "Previous camera",
                            )
                        } else {
                            (
                                format!("{} / {}", self.page + 1, pages),
                                pages > 1,
                                "Next page",
                                "Previous page",
                            )
                        };

                        if ui
                            .add_enabled(steppable, egui::Button::new("›"))
                            .on_hover_text(forward)
                            .clicked()
                        {
                            step = 1;
                        }
                        ui.label(RichText::new(label).color(theme::TEXT_DIM));
                        if ui
                            .add_enabled(steppable, egui::Button::new("‹"))
                            .on_hover_text(back)
                            .clicked()
                        {
                            step = -1;
                        }

                        if expanded {
                            // Layout is meaningless with one camera on screen;
                            // stream quality is what matters there instead.
                            let mut best = self.store.preferences.expanded_stream != "sub";
                            egui::ComboBox::from_id_salt("quality")
                                .selected_text(if best { "Best quality" } else { "Lower bandwidth" })
                                .show_ui(ui, |ui| {
                                    let mut changed = false;
                                    changed |= ui.selectable_value(&mut best, true, "Best quality").clicked();
                                    changed |= ui
                                        .selectable_value(&mut best, false, "Lower bandwidth")
                                        .clicked();
                                    if changed {
                                        self.store.preferences.expanded_stream =
                                            if best { "main".into() } else { "sub".into() };
                                        let _ = self.store.save();
                                        quality_changed = true;
                                    }
                                });
                        } else {
                            egui::ComboBox::from_id_salt("layout")
                                .selected_text(format!("{} up", self.capacity))
                                .show_ui(ui, |ui| {
                                    for option in LAYOUTS {
                                        if ui
                                            .selectable_value(
                                                &mut self.capacity,
                                                option,
                                                format!("{option} up"),
                                            )
                                            .clicked()
                                        {
                                            self.store.preferences.grid_size = option;
                                            let _ = self.store.save();
                                            self.page = 0;
                                            layout_changed = true;
                                        }
                                    }
                                });
                        }
                        if camera_selected {
                            Self::divider(ui);
                        }
                        if let (Some(channel), Some(client)) =
                            (floodlight_channel.as_ref(), floodlight_client.as_ref())
                        {
                            self.rail.floodlight_toolbar(
                                ui,
                                client,
                                &floodlight_device,
                                channel,
                            );
                            Self::divider(ui);
                        }
                        if camera_selected {
                            // The controls that act on the selected camera sit
                            // together, separated from the ones that change what
                            // the grid shows.
                            let (label, hint) = if selected_recording {
                                ("■ Stop", "Stop recording")
                            } else {
                                ("● Record", "Record this camera to a file")
                            };
                            if ui
                                .add(egui::Button::new(
                                    RichText::new(label)
                                        .color(if selected_recording { theme::WARN } else { theme::TEXT }),
                                ))
                                .on_hover_text(format!("{hint} — {selected_name}"))
                                .clicked()
                            {
                                record_request = Some(!selected_recording);
                            }
                            if ui
                                .button("Snapshot")
                                .on_hover_text(format!("Save a full-resolution still — {selected_name}"))
                                .clicked()
                            {
                                snapshot_requested = true;
                            }
                            // Sound follows the selection, and only ever comes
                            // from one camera: sixteen microphones at once is
                            // not something anyone wants.
                            if ui
                                .selectable_label(audio_on, if audio_on { "♫" } else { "♪" })
                                .on_hover_text(if audio_on {
                                    format!("Mute — currently playing {selected_name}")
                                } else {
                                    "Unmute the selected camera".to_string()
                                })
                                .clicked()
                            {
                                listen_toggle = true;
                            }
                        }
                    },
                    );
                });
            });

        if open_preferences {
            self.prefs.open(&self.store.preferences);
        }
        if open_about {
            self.about_open = true;
        }
        if add_device {
            self.dialog.add();
        }
        if reconnect_all {
            self.manager.connect_all();
            self.notify("Reconnecting all devices");
        }
        if sidebar_toggle {
            self.sidebar_open = !self.sidebar_open;
        }
        if let Some(on) = follow_toggle {
            self.set_following(on);
        }
        if follow_everything {
            self.follow_every_camera();
        }
        if listen_toggle {
            self.store.preferences.audio_enabled = !audio_on;
            let _ = self.store.save();
        }
        if snapshot_requested {
            self.take_snapshot();
        }
        if let Some(wanted) = record_request {
            self.toggle_recording(wanted);
        }
        if settings_touched {
            // The menu edits preferences in place, so re-apply the ones that
            // change running behaviour and persist the result.
            let updated = self.store.preferences.clone();
            self.apply_preferences(updated);
        }
        if quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if let Some(on) = fullscreen_request {
            self.set_fullscreen(ctx, on);
        }
        if step != 0 {
            self.step(step);
        }
        if layout_changed || quality_changed {
            self.rebuild();
        }
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        let mut add_requested = false;
        egui::SidePanel::left("cameras")
            .exact_width(230.0)
            .frame(egui::Frame::none().fill(theme::PANEL).inner_margin(8))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("CAMERAS")
                            .size(11.0)
                            .strong()
                            .color(theme::TEXT_DIM),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("+").on_hover_text("Add a camera or NVR").clicked() {
                            add_requested = true;
                        }
                    });
                });
                ui.add_space(6.0);

                let configs = self.manager.configs();
                let mut jump: Option<SourceId> = None;
                let mut hide_request: Option<(SourceId, bool)> = None;
                let mut follow_request: Option<(SourceId, bool)> = None;
                let mut delete_request: Option<SourceId> = None;
                let mut edit: Option<DeviceConfig> = None;
                let mut reveal_request: Option<(String, bool)> = None;
                let mut fold_request: Option<(String, bool)> = None;
                let mut reconnect: Option<DeviceConfig> = None;
                let mut full_grid = false;

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for config in &configs {
                        let client = self.manager.client(&config.id);
                        let (dot, tooltip) = match (&client, self.manager.is_connecting(&config.id))
                        {
                            (Some(_), _) => (theme::OK, config.host.clone()),
                            (None, true) => (theme::WARN, "Connecting…".to_string()),
                            (None, false) => (
                                theme::ERROR,
                                self.manager
                                    .error(&config.id)
                                    .unwrap_or_else(|| "Offline".into()),
                            ),
                        };

                        let device_active = client
                            .as_ref()
                            .map(|c| {
                                c.channels().iter().any(|ch| {
                                    self.poller.is_active(&(config.id.clone(), ch.index))
                                })
                            })
                            .unwrap_or(false);
                        let folded = self
                            .store
                            .preferences
                            .collapsed_devices
                            .contains(&config.id);
                        // How many of this device's cameras are hidden, which
                        // is what its menu offers to reveal. Counted against
                        // the channels it actually has, so an input that has
                        // since gone offers nothing.
                        let hidden_here = client
                            .as_ref()
                            .map(|c| {
                                c.channels()
                                    .iter()
                                    .filter(|ch| config.is_hidden(ch.index))
                                    .count()
                            })
                            .unwrap_or(0);
                        let revealing_here =
                            self.manager.reveal_hidden.lock().unwrap().contains(&config.id);

                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                            ui.painter().circle_filled(rect.center(), 4.0, dot);
                            if device_active {
                                ui.painter().circle_stroke(
                                    rect.center(),
                                    6.0,
                                    egui::Stroke::new(1.0, theme::WARN),
                                );
                            }
                            // The whole row, rather than a label beside a
                            // chevron: a name is a small target and everything
                            // here acts on the device it names, so the row is
                            // the thing to click.
                            //
                            // Laid out as one job of two runs rather than as
                            // one string. A single space between a chevron and
                            // a name sets them almost touching - the glyph has
                            // barely any side bearing of its own - and reads as
                            // though the mark were part of the name. This gives
                            // the gap a measurement instead of a character, and
                            // lets the chevron take the dimmer ink it should:
                            // it is furniture for the row, not part of what the
                            // device is called.
                            let mut name = egui::text::LayoutJob::default();
                            name.append(
                                if folded { "▸" } else { "▾" },
                                0.0,
                                egui::TextFormat {
                                    font_id: egui::FontId::proportional(10.0),
                                    color: theme::TEXT_DIM,
                                    valign: egui::Align::Center,
                                    ..Default::default()
                                },
                            );
                            name.append(
                                config.display_name(),
                                CHEVRON_GAP,
                                egui::TextFormat {
                                    font_id: egui::TextStyle::Body.resolve(ui.style()),
                                    color: theme::TEXT,
                                    valign: egui::Align::Center,
                                    ..Default::default()
                                },
                            );
                            let row = ui
                                .selectable_label(false, name)
                                .on_hover_text(tooltip);
                            if row.clicked() {
                                fold_request = Some((config.id.clone(), !folded));
                            }
                            if row.double_clicked() {
                                full_grid = true;
                            }
                            row.context_menu(|ui| {
                                ui.set_min_width(200.0);
                                if ui.button("Edit device…").clicked() {
                                    edit = Some(config.clone());
                                    ui.close_menu();
                                }
                                if ui.button("Reconnect this device").clicked() {
                                    reconnect = Some(config.clone());
                                    ui.close_menu();
                                }
                                ui.separator();
                                // The camera it asks about is the one thing not
                                // reachable from anywhere else: a hidden camera
                                // has no tile and no row, so the device that
                                // owns it is the only place left to ask.
                                if hidden_here > 0 {
                                    let label = if revealing_here {
                                        "Hide them again".to_string()
                                    } else if hidden_here == 1 {
                                        "Reveal 1 hidden camera".to_string()
                                    } else {
                                        format!("Reveal {hidden_here} hidden cameras")
                                    };
                                    if ui.button(label).clicked() {
                                        reveal_request =
                                            Some((config.id.clone(), !revealing_here));
                                        ui.close_menu();
                                    }
                                }
                                if ui
                                    .button(if folded {
                                        "Show its cameras"
                                    } else {
                                        "Fold its cameras away"
                                    })
                                    .clicked()
                                {
                                    fold_request = Some((config.id.clone(), !folded));
                                    ui.close_menu();
                                }
                            });
                        });

                        if let Some(client) = client.filter(|_| !folded) {
                            let show_offline = *self.manager.show_offline.lock().unwrap();
                            let reveal = revealing_here;
                            for channel in client.channels() {
                                if !channel.online && !show_offline {
                                    continue;
                                }
                                let is_hidden = config.is_hidden(channel.index);
                                if is_hidden && !reveal {
                                    continue;
                                }
                                ui.horizontal(|ui| {
                                    ui.add_space(14.0);
                                    let (rect, _) = ui.allocate_exact_size(
                                        Vec2::splat(8.0),
                                        egui::Sense::hover(),
                                    );
                                    // Amber while this channel is detecting,
                                    // so the sidebar shows activity even for
                                    // cameras that are not on the current page.
                                    let detecting = self
                                        .poller
                                        .is_active(&(config.id.clone(), channel.index));
                                    let dot = if !channel.online {
                                        theme::PLACEHOLDER
                                    } else if detecting {
                                        theme::WARN
                                    } else {
                                        theme::OK
                                    };
                                    ui.painter().circle_filled(rect.center(), 3.5, dot);
                                    if detecting {
                                        // A ring makes it legible at this size
                                        // rather than a slight hue shift.
                                        ui.painter().circle_stroke(
                                            rect.center(),
                                            5.5,
                                            egui::Stroke::new(1.0, theme::WARN),
                                        );
                                    }
                                    // A revealed-but-hidden camera is dimmed and
                                    // struck through, so it reads as "not on the
                                    // wall" rather than as an ordinary entry.
                                    let mut label = RichText::new(channel.display_name());
                                    if is_hidden {
                                        label = label.color(theme::PLACEHOLDER).strikethrough();
                                    }
                                    let id = SourceId::camera(&config.id, channel.index);
                                    let row = ui.selectable_label(
                                        self.selected.as_ref() == Some(&id),
                                        label,
                                    );
                                    if row.clicked() {
                                        jump = Some(id.clone());
                                    }
                                    row.context_menu(|ui| {
                                        if ui
                                            .button(if is_hidden {
                                                "Show this camera"
                                            } else {
                                                "Hide this camera"
                                            })
                                            .clicked()
                                        {
                                            hide_request = Some((id.clone(), !is_hidden));
                                            ui.close_menu();
                                        }
                                        ui.separator();
                                        let follows = config.follows_motion(channel.index);
                                        if ui
                                            .button(if follows {
                                                "Don't follow motion here"
                                            } else {
                                                "Follow motion here"
                                            })
                                            .clicked()
                                        {
                                            follow_request = Some((id.clone(), !follows));
                                            ui.close_menu();
                                        }
                                    });
                                });

                                // A camera's crops sit under it and a step
                                // further in, so the list says what they are
                                // without a word: they are that camera, seen
                                // more closely.
                                for view in config.views_of(channel.index) {
                                    if view.hidden && !reveal {
                                        continue;
                                    }
                                    let id = SourceId::virtual_camera(
                                        &config.id,
                                        channel.index,
                                        &view.id,
                                    );
                                    ui.horizontal(|ui| {
                                        ui.add_space(30.0);
                                        let mut label = RichText::new(format!(
                                            "{}  ·  {:.1}x",
                                            view.display_name(),
                                            view.zoom
                                        ))
                                        .size(12.0);
                                        if view.hidden {
                                            label = label
                                                .color(theme::PLACEHOLDER)
                                                .strikethrough();
                                        }
                                        let row = ui.selectable_label(
                                            self.selected.as_ref() == Some(&id),
                                            label,
                                        );
                                        if row.clicked() {
                                            jump = Some(id.clone());
                                        }
                                        row.context_menu(|ui| {
                                            if ui
                                                .button(if view.hidden {
                                                    "Show this camera"
                                                } else {
                                                    "Hide this camera"
                                                })
                                                .clicked()
                                            {
                                                hide_request =
                                                    Some((id.clone(), !view.hidden));
                                                ui.close_menu();
                                            }
                                            let follows =
                                                config.view_follows_motion(&view.id);
                                            if ui
                                                .button(if follows {
                                                    "Don't follow motion here"
                                                } else {
                                                    "Follow motion here"
                                                })
                                                .clicked()
                                            {
                                                follow_request =
                                                    Some((id.clone(), !follows));
                                                ui.close_menu();
                                            }
                                            ui.separator();
                                            if ui
                                                .button("Delete this virtual camera")
                                                .on_hover_text(
                                                    "The camera it was cut out of stays",
                                                )
                                                .clicked()
                                            {
                                                delete_request = Some(id.clone());
                                                ui.close_menu();
                                            }
                                        });
                                    });
                                }
                            }
                        }
                        ui.add_space(4.0);
                    }
                });

                if let Some((key, hidden)) = hide_request {
                    self.set_camera_hidden(key, hidden);
                }
                if let Some((key, follows)) = follow_request {
                    self.set_camera_followed(key, follows);
                }
                if let Some(key) = delete_request {
                    self.delete_virtual_camera(&key);
                }
                if let Some((device, reveal)) = reveal_request {
                    self.set_revealing(&device, reveal);
                }
                if let Some((device, folded)) = fold_request {
                    self.set_folded(&device, folded);
                }
                if let Some(device) = reconnect {
                    self.manager.connect(device.clone());
                    self.notify(format!("Reconnecting {}", device.display_name()));
                }
                if full_grid {
                    // What double-clicking a device has always been documented
                    // to do: put the whole wall back, whatever one camera or
                    // follow-motion had it showing.
                    self.maximized = None;
                    self.spotlight.clear();
                    self.rebuild();
                }
                if let Some(key) = jump {
                    self.focus(key);
                }
                if let Some(device) = edit {
                    self.dialog.edit(&device);
                }
            });

        if add_requested {
            self.dialog.add();
        }
    }

    /// Drive the screenshot countdown, then write the captured frame out.
    fn service_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, remaining)) = self.screenshot.as_mut() else { return };

        if *remaining > 0 {
            *remaining -= 1;
            if *remaining == 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::UserData::default(),
                ));
            }
            return;
        }

        let captured = ctx.input(|i| {
            i.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = captured {
            let path = path.clone();
            let (width, height) = (image.width() as u32, image.height() as u32);
            let rgba: Vec<u8> = image
                .pixels
                .iter()
                .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
                .collect();
            match image::RgbaImage::from_raw(width, height, rgba) {
                Some(buffer) => {
                    if let Err(err) = buffer.save(&path) {
                        log::error!("could not write {}: {err}", path.display());
                    } else {
                        log::info!("wrote {}", path.display());
                    }
                }
                None => log::error!("screenshot buffer had an unexpected size"),
            }
            self.screenshot = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Enter or leave follow-motion. Not persisted: it is a mode, not a setting.
    fn set_following(&mut self, on: bool) {
        self.follower.enabled = on;
        if !on {
            self.follower.clear();
            self.spotlight.clear();
            self.rebuild();
            self.notify("No longer following motion");
            return;
        }
        // Switched on and unable to fire is the one state worth interrupting
        // for. It looks exactly like a quiet night - nothing comes up, and
        // nothing anywhere says why - so it is said at the moment somebody
        // turns it on, which is the moment they are looking.
        self.notify(match self.why_following_cannot_fire() {
            Some(why) => format!("Following motion, but {why}"),
            None => "Following motion".to_string(),
        });
    }

    /// Reveal one device's hidden cameras, or put them away again.
    ///
    /// The only route back from hiding, and it has to live on the device: a
    /// hidden camera has no tile to right-click and no row in the sidebar, so
    /// the box that owns it is the last place left to ask. Not persisted - it
    /// is how you undo hiding, not a way to live.
    fn set_revealing(&mut self, device: &str, reveal: bool) {
        {
            let mut revealing = self.manager.reveal_hidden.lock().unwrap();
            if reveal {
                revealing.insert(device.to_string());
            } else {
                revealing.remove(device);
            }
        }
        self.rebuild();
    }

    /// Fold a device's channel list away in the sidebar, or open it.
    ///
    /// Remembered, unlike revealing: a 36-channel box you have folded once is
    /// one you want folded tomorrow, and the preference to hold that has been
    /// in the config all along with nothing reading it.
    fn set_folded(&mut self, device: &str, folded: bool) {
        if fold_device(&mut self.store.preferences.collapsed_devices, device, folded) {
            let _ = self.store.save();
        }
    }

    /// Put every camera back into follow-motion.
    ///
    /// One action rather than one right-click per camera. Nothing else here
    /// clears a per-device list wholesale, and this one earns it: the exclusion
    /// is invisible from the wall unless following is running, so somebody who
    /// has ended up with more of it than they meant has no way to find the
    /// cameras to visit.
    fn follow_every_camera(&mut self) {
        let mut freed = 0usize;
        for device in &mut self.store.devices {
            freed += device.unfollowed_channels.len();
            device.unfollowed_channels.clear();
        }
        if freed == 0 {
            return;
        }
        let _ = self.store.save();
        self.manager.set_configs(self.store.devices.clone());
        self.notify(if freed == 1 {
            "1 camera is following motion again".to_string()
        } else {
            format!("{freed} cameras are following motion again")
        });
    }

    /// Why follow-motion could never bring a camera up, if it could not.
    fn why_following_cannot_fire(&self) -> Option<String> {
        let cameras = self.manager.sources();
        let left_out = cameras
            .iter()
            .filter(|source| !self.follows_motion(&source.id))
            .count();
        nothing_to_follow(
            self.store.preferences.follow_kinds.len(),
            cameras.len(),
            left_out,
        )
    }

    /// Offer to keep the framing a tile is currently showing.
    /// Put a box on one camera's picture, to choose a crop of it by hand.
    ///
    /// Only ever one at a time: two boxes on two pictures is two answers to a
    /// question that has one, and the second would quietly abandon the first.
    fn begin_framing(&mut self, key: &SourceId) {
        for tile in &mut self.tiles {
            if &tile.key() == key {
                tile.begin_framing();
            } else {
                tile.cancel_framing();
            }
        }
    }

    fn cancel_framing(&mut self) -> bool {
        let mut had = false;
        for tile in &mut self.tiles {
            if tile.is_framing() {
                tile.cancel_framing();
                had = true;
            }
        }
        had
    }

    /// Keep the box that is on screen, and ask what to call it.
    fn accept_framing(&mut self) {
        let Some(tile) = self.tiles.iter_mut().find(|t| t.is_framing()) else { return };
        let key = tile.key();
        let name = format!("{} detail", tile.channel.display_name());
        let Some((zoom, centre)) = tile.accept_framing() else { return };
        // Prefilled from the camera it comes out of, so a crop made in a hurry
        // still lands on the wall with something better than a blank strip.
        self.virtual_dialog = Some(VirtualDialog {
            parent: key.stream_key_id(),
            editing: None,
            name,
            zoom,
            centre,
            // Editable, because the box has already been placed by hand and
            // these are the same two numbers: somebody who wants the crop
            // exactly centred can say so rather than nudge towards it.
            editable: true,
        });
    }

    /// Open one that already exists, to rename it or type its framing in.
    fn open_virtual_settings(&mut self, key: SourceId) {
        let Some(id) = key.view.clone() else { return };
        let Some(view) = self
            .store
            .devices
            .iter()
            .find(|d| d.id == key.device)
            .and_then(|d| d.virtual_camera(&id))
        else {
            return;
        };
        self.virtual_dialog = Some(VirtualDialog {
            parent: key.stream_key_id(),
            editing: Some(id),
            name: view.name.clone(),
            zoom: view.zoom,
            centre: view.centre,
            editable: true,
        });
    }

    fn virtual_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.virtual_dialog.as_mut() else { return };

        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new(dialog.title())
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("virtual-camera")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        let name = ui.add(
                            egui::TextEdit::singleline(&mut dialog.name).desired_width(220.0),
                        );
                        if dialog.editing.is_none() {
                            name.request_focus();
                        }
                        ui.end_row();

                        ui.label("Zoom");
                        if dialog.editable {
                            ui.add(
                                egui::DragValue::new(&mut dialog.zoom)
                                    .speed(0.05)
                                    .range(1.0..=8.0)
                                    .suffix("x"),
                            );
                        } else {
                            ui.label(
                                RichText::new(format!("{:.1}x", dialog.zoom))
                                    .color(theme::TEXT_DIM),
                            );
                        }
                        ui.end_row();

                        ui.label("Centre");
                        if dialog.editable {
                            ui.horizontal(|ui| {
                                let mut x = dialog.centre.0 * 100.0;
                                let mut y = dialog.centre.1 * 100.0;
                                ui.add(
                                    egui::DragValue::new(&mut x)
                                        .speed(0.5)
                                        .range(0.0..=100.0)
                                        .suffix("%"),
                                );
                                ui.add(
                                    egui::DragValue::new(&mut y)
                                        .speed(0.5)
                                        .range(0.0..=100.0)
                                        .suffix("%"),
                                );
                                dialog.centre = (x / 100.0, y / 100.0);
                            });
                        } else {
                            ui.label(
                                RichText::new(format!(
                                    "{:.0}%, {:.0}%",
                                    dialog.centre.0 * 100.0,
                                    dialog.centre.1 * 100.0
                                ))
                                .color(theme::TEXT_DIM),
                            );
                        }
                        ui.end_row();
                    });

                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "It goes on the wall beside the camera it comes from, and shares \
                         its connection.",
                    )
                    .size(11.0)
                    .color(theme::PLACEHOLDER),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    let named = !dialog.name.trim().is_empty();
                    if ui
                        .add_enabled(
                            named,
                            egui::Button::new(if dialog.editing.is_some() {
                                "Save"
                            } else {
                                "Create"
                            }),
                        )
                        .on_disabled_hover_text("Give it a name first")
                        .clicked()
                    {
                        save = true;
                    }
                });
            });

        if save {
            if let Some(dialog) = self.virtual_dialog.take() {
                self.commit_virtual(dialog);
            }
            return;
        }
        if cancel || !open {
            self.virtual_dialog = None;
        }
    }

    /// Write a made or edited virtual camera to its device.
    fn commit_virtual(&mut self, dialog: VirtualDialog) {
        let parent = dialog.parent.clone();
        let Some(device) = self.store.devices.iter_mut().find(|d| d.id == parent.device) else {
            return;
        };
        let name = dialog.name.trim().to_string();
        let editing = dialog.editing.clone();
        let id = editing.clone().unwrap_or_else(crate::config::new_view_id);
        let view = VirtualCamera {
            id: id.clone(),
            channel: parent.channel,
            name: name.clone(),
            zoom: dialog.zoom.clamp(1.0, 8.0),
            centre: dialog.centre,
            hidden: editing
                .as_ref()
                .and_then(|id| device.virtual_camera(id))
                .map(|v| v.hidden)
                .unwrap_or(false),
        };
        let existed = device.update_virtual_camera(view.clone());
        if !existed {
            device.add_virtual_camera(view.clone());
        }
        let _ = self.store.save();
        self.manager.set_configs(self.store.devices.clone());

        let id = SourceId::virtual_camera(&parent.device, parent.channel, &view.id);
        if existed {
            // The tile is already on the wall; move it to what was just typed
            // rather than waiting for it to be rebuilt from nothing.
            if let Some(tile) = self.tiles.iter_mut().find(|t| t.key() == id) {
                tile.title = name.clone();
                tile.set_saved_view(view.zoom, view.centre);
            }
            self.notify(format!("Saved {name}"));
        } else {
            self.notify(format!("Added {name}"));
        }
        self.rebuild();
        // Straight onto the thing that was just made, so it is obvious which
        // of the cells it is.
        self.selected = Some(id);
    }

    /// Keep the crop a virtual camera's tile is currently showing.
    fn save_framing(&mut self, key: &SourceId) {
        let Some(view_id) = key.view.clone() else { return };
        let Some(tile) = self.tiles.iter().find(|t| t.key() == *key) else { return };
        let (zoom, centre) = tile.framing();

        let Some(device) = self.store.devices.iter_mut().find(|d| d.id == key.device) else {
            return;
        };
        let Some(mut view) = device.virtual_camera(&view_id).cloned() else { return };
        view.zoom = zoom;
        view.centre = centre;
        let name = view.display_name();
        device.update_virtual_camera(view.clone());
        let _ = self.store.save();
        self.manager.set_configs(self.store.devices.clone());

        if let Some(tile) = self.tiles.iter_mut().find(|t| t.key() == *key) {
            tile.set_saved_view(view.zoom, view.centre);
        }
        self.notify(format!("{name} now opens here"));
    }

    /// Forget a virtual camera. The camera it was cut out of is untouched.
    fn delete_virtual_camera(&mut self, key: &SourceId) {
        let Some(view_id) = key.view.clone() else { return };
        let name = self.camera_name(key);
        let Some(device) = self.store.devices.iter_mut().find(|d| d.id == key.device) else {
            return;
        };
        if !device.remove_virtual_camera(&view_id) {
            return;
        }
        let _ = self.store.save();
        self.manager.set_configs(self.store.devices.clone());

        // Selection and expansion would otherwise point at a camera that no
        // longer exists, leaving the toolbar acting on nothing.
        if self.selected.as_ref() == Some(key) {
            self.selected = None;
        }
        if self.maximized.as_ref() == Some(key) {
            self.maximized = None;
        }
        // The spotlight is edited here, so the follower has to be told its last
        // answer is stale. It only reports a selection when that selection
        // changes, and deleting a crop changes what is on the wall without
        // changing what is detecting — so without this the view sits on a
        // spotlight nothing will refresh, and following goes quiet until the
        // detections happen to change by themselves.
        self.spotlight.retain(|id| id != key);
        self.follower.resend();
        self.notify(format!("Deleted {name}"));
        self.rebuild();
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let mut open = true;
        egui::Window::new("About Kestrel")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(RichText::new("K E S T R E L").strong().size(16.0));
                ui.label(
                    RichText::new("Live camera wall and PTZ control for your NVR")
                        .color(theme::TEXT_DIM),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!(
                        "Version {} · built {}",
                        env!("CARGO_PKG_VERSION"),
                        env!("KESTREL_BUILD_DATE")
                    ))
                    .size(11.0)
                    .color(theme::TEXT_DIM),
                );
                ui.add_space(8.0);
                egui::Grid::new("about").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                    ui.label(RichText::new("Config").size(11.0).color(theme::PLACEHOLDER));
                    ui.label(RichText::new(self.store.path.display().to_string()).size(11.0));
                    ui.end_row();
                    ui.label(RichText::new("Media").size(11.0).color(theme::PLACEHOLDER));
                    ui.label(RichText::new(&self.store.preferences.media_dir).size(11.0));
                    ui.end_row();
                    ui.label(RichText::new("Passwords").size(11.0).color(theme::PLACEHOLDER));
                    ui.label(
                        RichText::new(if self.store.keyring_available() {
                            "system keyring"
                        } else {
                            "config file (mode 0600)"
                        })
                        .size(11.0),
                    );
                    ui.end_row();
                    // Asking to stay awake and being granted it are different
                    // things, and there is no other way to tell which happened.
                    ui.label(RichText::new("Screen").size(11.0).color(theme::PLACEHOLDER));
                    ui.label(
                        RichText::new(if self.inhibitor.is_active() {
                            "kept awake"
                        } else if self.store.preferences.keep_awake_fullscreen {
                            "sleeps normally — kept awake while fullscreen"
                        } else {
                            "sleeps normally"
                        })
                        .size(11.0),
                    );
                    ui.end_row();
                });
            });
        self.about_open = open;
    }

    /// Adopt edited preferences, applying the ones that change live behaviour.
    fn apply_preferences(&mut self, updated: crate::config::Preferences) {
        let layout_changed = updated.grid_size != self.store.preferences.grid_size
            || updated.live_substream != self.store.preferences.live_substream
            || updated.expanded_stream != self.store.preferences.expanded_stream
            || updated.show_offline_channels != self.store.preferences.show_offline_channels;

        self.store.preferences = updated;
        let prefs = &self.store.preferences;

        *self.manager.show_offline.lock().unwrap() = prefs.show_offline_channels;
        self.streams.warm_enabled = prefs.warm_streams;
        self.streams.max_warm = prefs.max_warm_streams;
        if !prefs.warm_streams {
            self.streams.clear_warm();
        }
        // How following behaves is a preference; whether it is running is not.
        self.follower
            .set_dwell(std::time::Duration::from_secs_f32(prefs.follow_dwell_seconds));
        self.notifier.set_enabled(prefs.desktop_notifications);
        if LAYOUTS.contains(&prefs.grid_size) {
            self.capacity = prefs.grid_size;
        }
        self.store.ensure_media_dirs();

        if let Err(err) = self.store.save() {
            self.notify(format!("Could not save preferences: {err}"));
            return;
        }
        if layout_changed {
            self.rebuild();
        }
        self.notify("Preferences saved");
    }

    fn save_device(&mut self, device: DeviceConfig) {
        let label = device.display_name().to_string();
        match self.store.devices.iter_mut().find(|d| d.id == device.id) {
            Some(existing) => *existing = device.clone(),
            None => self.store.devices.push(device.clone()),
        }
        if let Err(err) = self.store.save() {
            self.notify(format!("Could not save the device list: {err}"));
            return;
        }
        self.manager.set_configs(self.store.devices.clone());
        // Reconnect so an edited address or password takes effect at once.
        self.manager.connect(device);
        self.notify(format!("Saved {label}"));
    }

    fn remove_device(&mut self, id: &str) {
        self.manager.disconnect(id);
        if let Some(device) = self.store.devices.iter().find(|d| d.id == id).cloned() {
            crate::config::forget_secret(&device);
            self.notify(format!("Removed {}", device.display_name()));
        }
        self.store.devices.retain(|d| d.id != id);
        let _ = self.store.save();
        self.manager.set_configs(self.store.devices.clone());

        // Drop any tile belonging to the device that just went away. Its
        // connections go with the rebuild below, which finds nothing on screen
        // still reading them.
        self.tiles.retain(|tile| tile.device_id() != id);
        self.selected = None;
        self.rebuild();
    }

    /// Move detections into the feed, notifications and follow-motion.
    fn pump_events(&mut self) {
        // Notifications are per *event*: one alert when something starts, not
        // one per poll for as long as it continues.
        for event in self.poller.take_new() {
            // The feed still records everything; this only decides what is
            // worth interrupting someone for.
            let wanted = self
                .store
                .preferences
                .notify_kinds
                .iter()
                .any(|k| k == event.kind.device_key());
            if self.notifier.is_enabled() && wanted {
                self.notifier.notify(event.kind.label(), &event.channel_name);
            }
        }

        // Following works off the current *state*, so a camera stays on screen
        // while the motion lasts rather than for one dwell period after it began.
        for key in self
            .poller
            .active_keys_for(&self.store.preferences.follow_kinds)
        {
            // A camera can be left out of following without being taken off the
            // wall. The two are different requests — one is "I do not want to
            // see this", the other "I do not want to be *taken* to it" — and a
            // drive that catches every car on the road is the case for the
            // second: worth watching, not worth jumping to.
            if self.may_follow(&key) {
                self.follower.note(key);
            }
        }

        // Highlight tiles that are detecting right now.
        let show_stats = self.store.preferences.show_stream_stats;
        let fill = self.store.preferences.picture_fill();
        let (overlay_title, title_alpha) = self.title_visibility();
        let following = self.follower.enabled;
        let revealing = self.manager.reveal_hidden.lock().unwrap().clone();
        for tile in &mut self.tiles {
            let key = tile.key();
            tile.show_stats = show_stats;
            tile.fill = fill;
            // Read off the device rather than through `follows_motion`, which
            // borrows all of self while the tiles are borrowed mutably. Only
            // while following is actually running: the rest of the time it is a
            // fact about a mode nobody has switched on.
            let device = self.store.devices.iter().find(|d| d.id == key.device);
            tile.exempt_from_follow = following
                && !device
                    .map(|d| match &key.view {
                        Some(view) => d.view_follows_motion(view),
                        None => d.follows_motion(key.channel),
                    })
                    .unwrap_or(true);
            // Only ever true while revealing, because that is the only way a
            // hidden camera reaches the wall at all.
            tile.hidden_but_shown = revealing.contains(&key.device)
                && device
                    .map(|d| match &key.view {
                        Some(view) => {
                            d.virtual_camera(view).map(|v| v.hidden).unwrap_or(false)
                                || d.is_hidden(key.channel)
                        }
                        None => d.is_hidden(key.channel),
                    })
                    .unwrap_or(false);
            tile.overlay_title = overlay_title;
            tile.title_alpha = title_alpha;
            if self.demo_badges {
                // KESTREL_DEMO_BADGES lights every detection type, so the
                // badges can be checked without waiting for something to walk
                // past a camera. Same purpose as KESTREL_SCREENSHOT.
                tile.flag_motion(true);
                tile.set_detections(crate::api::EventKind::ALL.to_vec());
            } else {
                // Detections belong to the lens, so every view of a camera —
                // the camera itself and each crop of it — badges together.
                let channel = key.stream_key();
                tile.flag_motion(self.poller.is_active(&channel));
                tile.set_detections(self.poller.active_kinds(&channel));
            }
        }

        if let Some(selection) = self.follower.evaluate() {
            // An explicit expand outranks follow-motion.
            if self.maximized.is_none() {
                // A detecting camera brings up whichever of its views are
                // followed. Usually that is the camera; where somebody has said
                // otherwise it is the crops they said instead.
                let selection: Vec<SourceId> = selection
                    .iter()
                    .flat_map(|key| self.followed_views(key).collect::<Vec<_>>())
                    .collect();
                self.spotlight = selection.clone();
                if !selection.is_empty() {
                    self.tab = Tab::Live;
                }
                self.rebuild();
                if selection.len() == 1 {
                    let name = self
                        .tiles
                        .first()
                        .map(|t| t.title.clone())
                        .unwrap_or_default();
                    self.notify(format!("Motion: {name}"));
                } else if !selection.is_empty() {
                    self.notify(format!("Motion on {} cameras", selection.len()));
                }
            }
        }
    }

    /// Keep off-screen cameras connected so they can be shown instantly.
    fn top_up_warm(&mut self) {
        let sources = self.manager.sources();
        // Once per camera, not once per view of it: a crop shares its parent's
        // connection, so warming one for it would warm the same camera twice.
        let mut candidates: Vec<(Key, String)> = Vec::new();
        for source in sources.iter().filter(|s| !s.is_virtual() && s.channel.online) {
            candidates.push((source.stream_key(), self.manager.source_label(source)));
        }

        let manager = self.manager.clone();
        self.streams.top_up_warm(&candidates, |key| {
            // Warm streams are always the wide channel's sub stream, so the
            // channel is looked up rather than synthesised.
            let channel = manager
                .sources()
                .into_iter()
                .find(|s| !s.is_virtual() && s.stream_key() == *key)
                .map(|s| s.channel)?;
            manager.stream_source(&key.0, &channel, StreamType::Sub)
        });
    }

    // ---------------------------------------------------------------- weather

    /// Where the radar starts, as a view rather than a span.
    ///
    /// The zoom is worked out from the span in preferences and the height it is
    /// being drawn into, so "200 km across" means the same thing in a wall cell
    /// as it does in the tab — the tile grid has levels rather than spans, and
    /// this is where one becomes the other.
    fn radar_home_for(&self, height: f32) -> crate::weather::tiles::Viewport {
        use crate::weather::tiles::Viewport;
        match self.store.preferences.radar_home() {
            Some(home) => Viewport::new(
                home.lat,
                home.lon,
                Viewport::zoom_for_span(home.lat, home.span_km, height.max(120.0)),
            ),
            // Nowhere to point it; the caller only draws when there is.
            None => self
                .radar_view
                .unwrap_or(Viewport::new(39.83, -98.58, 4.0)),
        }
    }

    /// Whether there is a poller to draw from. The strip goes up as soon as
    /// there is, saying it is waiting — a band that appeared only once the
    /// first reading landed would resize the grid a minute after launch.
    fn weather_showing(&self) -> bool {
        self.weather_poller.is_some()
    }

    /// Start, stop or restart the weather poller to match what is configured,
    /// then take in whatever it last got.
    ///
    /// Run every frame rather than at each of the places preferences can
    /// change. Deciding it in one place is what keeps "switched on with no
    /// address" and "switched off while the pane is open" from each needing
    /// their own handling, and comparing the settings is a handful of string
    /// compares against a thread that is asleep.
    fn reconcile_weather(&mut self) {
        let wanted = self.store.preferences.weather_usable();

        if !wanted {
            if self.weather_poller.take().is_some() {
                self.weather.reset();
                // The tab has gone with it, so nothing may be left showing a
                // pane that no longer has a switch.
                if self.tab == Tab::Weather {
                    self.tab = Tab::Live;
                }
            }
            return;
        }

        let settings = self.store.preferences.weather_settings();
        let restart = match &self.weather_poller {
            Some(poller) => *poller.settings() != settings,
            None => true,
        };
        if restart {
            // Dropped first: the old loop is asleep on a five-minute timer and
            // holding both would leave two threads polling the same station.
            self.weather_poller = None;
            self.weather.reset();
            self.weather_poller = Some(WeatherPoller::start(settings));
        }

        if let Some(poller) = &self.weather_poller {
            self.weather.absorb(poller.latest());
        }
    }

    /// Which sweeps exist, and what to call the place.
    ///
    /// One poller now, not two. The pictures are fetched per tile on demand by
    /// the map itself, so what is left to poll is the handful of facts a tile
    /// URL cannot be built without — and those are the same wherever the radar
    /// is being drawn.
    fn reconcile_radar(&mut self, wanted: bool) {
        if !wanted {
            self.radar_poller = None;
            return;
        }
        let prefs = &self.store.preferences;
        let Some(home) = prefs.radar_home() else {
            self.radar_poller = None;
            return;
        };

        let settings = RadarSettings {
            lat: format!("{:.5}", home.lat),
            lon: format!("{:.5}", home.lon),
            frames: crate::weather::radar::FRAMES,
            interval: RADAR_INTERVAL,
        };
        let restart = match self.radar_poller.as_ref() {
            Some(running) => *running.settings() != settings,
            None => true,
        };
        if restart {
            self.radar_poller = None;
            self.radar_poller = Some(RadarPoller::start(settings));
        }
    }

    fn focus(&mut self, key: SourceId) {
        let sources = self.manager.sources();
        if let Some(index) = sources.iter().position(|s| s.id == key) {
            self.tab = Tab::Live;
            self.maximized = None;
            self.page = index / self.capacity;
            self.selected = Some(key);
            self.rebuild();
        }
    }

    fn control_rail(&mut self, ctx: &egui::Context) {
        let selected = self.selected.clone();
        let tile = selected
            .as_ref()
            .and_then(|key| self.tiles.iter().find(|t| &t.key() == key));
        let channel = tile.map(|t| t.channel.clone());
        let label = tile.map(|t| t.title.clone()).unwrap_or_default();
        let client = selected.as_ref().and_then(|key| self.manager.client(&key.device));

        // An empty pane is worse than no pane: it takes 260px of the grid to
        // say nothing. With snapshot and recording now in the toolbar, a fixed
        // camera has no controls at all.
        if !RailState::has_controls(channel.as_ref()) {
            return;
        }

        let mut lens_request: Option<bool> = None;

        egui::SidePanel::right("control")
            .exact_width(260.0)
            .frame(egui::Frame::none().fill(theme::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.rail.show(
                        ui,
                        client,
                        selected.as_ref().map(|k| k.device.as_str()).unwrap_or(""),
                        channel.as_ref(),
                        &label,
                        &mut |tele| lens_request = Some(tele),
                    );
                });
            });

        if let (Some(tele), Some(key)) = (lens_request, self.selected.clone()) {
            self.switch_lens(key.stream_key(), tele);
        }
    }

    /// Take the pointer off screen once it has been still over the pictures.
    ///
    /// Scoped to the grid: a pointer that vanishes over the toolbar or the
    /// camera list would just be lost. Any movement brings it straight back,
    /// because the same timer drives it as the camera names.
    fn service_pointer(&self, ctx: &egui::Context) {
        let prefs = &self.store.preferences;
        if !prefs.hide_pointer_when_idle || !self.pointer_present {
            return;
        }
        if self.last_pointer_move.elapsed().as_secs_f32() <= prefs.title_hide_seconds.max(0.0) {
            return;
        }
        let over_grid = ctx
            .input(|i| i.pointer.latest_pos())
            .zip(self.grid_rect)
            .map(|(pos, grid)| grid.contains(pos))
            .unwrap_or(false);
        if over_grid {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
    }

    /// Whether camera names float over the picture, and how visible they are.
    ///
    /// Returns (overlay, alpha). With auto-hide off the name keeps a permanent
    /// strip and never covers the video. With it on the name floats and fades:
    /// reserving a strip that came and went would resize every picture each time
    /// the mouse stopped, which is far more distracting than the label.
    /// Whether the top bar is on screen.
    ///
    /// Always, unless it has been asked to hide itself - and then it follows
    /// exactly the rule the camera names follow, including leaving at once when
    /// the pointer goes out of the window entirely. Any movement brings it
    /// back, and Ctrl+B still reaches the camera list while it is away.
    fn header_showing(&self) -> bool {
        if !self.store.preferences.auto_hide_header {
            return true;
        }
        let (hiding, alpha) = self.title_visibility_with(true);
        !hiding || alpha > 0.0
    }

    fn title_visibility(&self) -> (bool, f32) {
        self.title_visibility_with(self.store.preferences.auto_hide_titles)
    }

    fn title_visibility_with(&self, auto_hide: bool) -> (bool, f32) {
        let prefs = &self.store.preferences;
        title_visibility(
            auto_hide,
            self.last_pointer_move.elapsed().as_secs_f32(),
            prefs.title_hide_seconds,
            self.pointer_present,
        )
    }

    /// Whether follow-motion may bring this camera up at all.
    ///
    /// Two ways to say no, and they are different requests. Hiding a camera
    /// takes it off the wall; exempting one leaves it there and stops it
    /// pulling the view. Following has to honour both, and honouring hiding
    /// here rather than downstream is the whole point: the detection poller
    /// reports every online channel, hidden or not, so a hidden camera was
    /// being noted, ranked and selected - and then filtered out again when the
    /// grid was rebuilt from the visible sources.
    ///
    /// That filtering is what broke it. A selection of nothing but hidden
    /// cameras rebuilt to an empty wall and cleared the spotlight, so nothing
    /// came up; and because the cap is nine cameras taken most-recent-first, a
    /// hidden camera watching a road could hold every slot and keep the one
    /// you wanted off the list indefinitely. Refusing them here means they
    /// never enter the reckoning at all.
    fn may_follow(&self, key: &Key) -> bool {
        // A channel is worth noting if *anything* it can put on the wall would
        // be followed. Excluding the camera while following one crop of it is a
        // sensible thing to ask for — a drive that catches the road, with the
        // gate worth being taken to — and gating on the channel alone would
        // make that combination silently do nothing.
        self.followed_views(key).next().is_some()
    }

    /// Which views of one camera follow-motion may bring up, in wall order.
    ///
    /// The camera itself unless it has been left out, then whichever of its
    /// crops have not been. A camera nobody has ever mentioned answers with
    /// itself, which is how following behaved before crops existed.
    fn followed_views(&self, key: &Key) -> impl Iterator<Item = SourceId> + '_ {
        let device = self.store.devices.iter().find(|d| d.id == key.0);
        let channel = key.1;
        let revealing = self
            .manager
            .reveal_hidden
            .lock()
            .map(|set| set.contains(&key.0))
            .unwrap_or(false);
        let hidden = device.map(|d| d.is_hidden(channel)).unwrap_or(false);
        let device_id = key.0.clone();

        let camera = (!hidden || revealing)
            .then(|| device.map(|d| d.follows_motion(channel)).unwrap_or(true))
            .unwrap_or(false)
            .then(|| SourceId::camera(&device_id, channel));

        let views: Vec<SourceId> = match device {
            Some(device) if !hidden || revealing => device
                .views_of(channel)
                .filter(|view| !view.hidden || revealing)
                .filter(|view| device.view_follows_motion(&view.id))
                .map(|view| SourceId::virtual_camera(&device_id, channel, &view.id))
                .collect(),
            _ => Vec::new(),
        };
        camera.into_iter().chain(views)
    }

    /// Whether follow-motion has been told to leave this camera alone.
    ///
    /// The exemption on its own, without the hiding — this is what the menus
    /// offer and what the counts report, and a hidden camera is not "left out
    /// of following" in the sense anybody means by it.
    fn follows_motion(&self, key: &SourceId) -> bool {
        self.store
            .devices
            .iter()
            .find(|d| d.id == key.device)
            .map(|d| match &key.view {
                Some(view) => d.view_follows_motion(view),
                None => d.follows_motion(key.channel),
            })
            .unwrap_or(true)
    }

    /// How many cameras on the wall have been left out of following.
    ///
    /// Counted against what is actually there rather than off the stored lists,
    /// so a channel that has since gone does not show up as an exclusion
    /// nobody can find.
    fn unfollowed_count(&self) -> usize {
        // Read every frame for the header's tooltip, and nothing excluded is by
        // far the common case — so answer that one without asking the manager
        // for a copy of its channel list.
        if self
            .store
            .devices
            .iter()
            .all(|d| d.unfollowed_channels.is_empty() && d.unfollowed_views.is_empty())
        {
            return 0;
        }
        self.manager
            .sources()
            .into_iter()
            .filter(|source| !self.follows_motion(&source.id))
            .count()
    }

    /// What to call a camera in a message, whether or not it is on the wall.
    ///
    /// The tile knows its own title and is the right answer when there is one.
    /// But these menus are reachable from the sidebar too, where the camera in
    /// question is regularly on another page and has no tile at all — and the
    /// device's own channel list still knows its name.
    fn camera_name(&self, key: &SourceId) -> String {
        if let Some(tile) = self.tiles.iter().find(|t| t.key() == *key) {
            return tile.title.clone();
        }
        // A virtual camera's name is its own, and is known without asking the
        // device anything: it was written down when the crop was made.
        if let Some(view) = key.view.as_ref() {
            if let Some(view) = self
                .store
                .devices
                .iter()
                .find(|d| d.id == key.device)
                .and_then(|d| d.virtual_camera(view))
            {
                return view.display_name();
            }
        }
        self.manager
            .client(&key.device)
            .and_then(|client| {
                client
                    .channels()
                    .iter()
                    .find(|channel| channel.index == key.channel)
                    .map(|channel| self.manager.channel_label(&key.device, channel))
            })
            .unwrap_or_else(|| "This camera".to_string())
    }

    /// Include a camera in follow-motion, or leave it out.
    ///
    /// Persisted per device beside hiding, and for the same reason: it is a
    /// fact about what the camera is pointed at rather than a mode. Nothing is
    /// rebuilt — the camera stays exactly where it is on the wall, which is the
    /// whole distinction from hiding it.
    fn set_camera_followed(&mut self, key: SourceId, follows: bool) {
        let name = self.camera_name(&key);
        let Some(device) = self.store.devices.iter_mut().find(|d| d.id == key.device) else {
            return;
        };
        let changed = match &key.view {
            Some(view) => device.set_view_follows_motion(view, follows),
            None => device.set_follows_motion(key.channel, follows),
        };
        if !changed {
            return;
        }
        let _ = self.store.save();
        self.manager.set_configs(self.store.devices.clone());

        if follows {
            self.notify(format!("Following motion on {name} again"));
        } else {
            // Out of the selection now rather than when its dwell runs out: the
            // camera may well be on screen *because* of the detection that just
            // prompted somebody to switch this off.
            self.follower.forget(&key.stream_key());
            self.notify(format!("{name} will no longer steer the view"));
        }
    }

    /// Hide a camera from the wall, or put it back.
    ///
    /// Hiding is per device and persisted, so it survives a restart the way the
    /// device list does. The grid is rebuilt rather than filtered in place,
    /// because a hidden camera should also stop streaming.
    fn set_camera_hidden(&mut self, key: SourceId, hidden: bool) {
        let name = self.camera_name(&key);

        let Some(device) = self.store.devices.iter_mut().find(|d| d.id == key.device) else {
            return;
        };
        let changed = match &key.view {
            Some(view) => device.set_view_hidden(view, hidden),
            None => device.set_hidden(key.channel, hidden),
        };
        if !changed {
            return;
        }
        let _ = self.store.save();
        self.manager.set_configs(self.store.devices.clone());

        if hidden {
            // Selection would otherwise point at a camera that is no longer
            // shown, leaving the toolbar acting on something invisible.
            if self.selected.as_ref() == Some(&key) {
                self.selected = None;
            }
            if self.maximized.as_ref() == Some(&key) {
                self.maximized = None;
            }
            // And the same staleness as deleting one: a camera hidden out from
            // under the spotlight leaves a selection nobody is showing, which
            // the follower cannot tell from one already on screen.
            if self.spotlight.contains(&key) {
                self.spotlight.retain(|id| id != &key);
                self.follower.resend();
            }
            self.notify(format!("Hid {name} — show it again from the ⋯ menu"));
        } else {
            self.notify(format!("Showing {name}"));
            // Nothing left to reveal on this device, so stop revealing it.
            // Left on, the next camera hidden there would appear not to go
            // anywhere.
            let still_hidden = self
                .store
                .devices
                .iter()
                .find(|d| d.id == key.device)
                .map(|d| {
                    !d.hidden_channels.is_empty()
                        || d.virtual_cameras.iter().any(|v| v.hidden)
                })
                .unwrap_or(false);
            if !still_hidden {
                self.manager.reveal_hidden.lock().unwrap().remove(&key.device);
            }
        }
        self.rebuild();
    }

    /// Point the sound at whichever camera is selected.
    ///
    /// Run every frame rather than at each of the places selection can change:
    /// setting a worker's flag is an atomic store, so reconciling unconditionally
    /// costs nothing and cannot drift. It also covers a rebuild, which hands the
    /// tiles new workers that start silent.
    ///
    /// Audio is decoded only for the camera it is playing from, so this is one
    /// AAC stream regardless of how many cameras are on screen.
    fn reconcile_audio(&mut self) {
        let target = if self.store.preferences.audio_enabled {
            self.selected.clone()
        } else {
            None
        };
        // Set on the connection rather than on the tile, because a camera and
        // its crops share one: the question is which camera is audible, and
        // whether you are listening to the whole picture or a corner of it is
        // not a question sound has an answer to.
        let audible = target.as_ref().map(|key| key.stream_key());
        self.streams.set_audio(audible.as_ref());
        if target != self.listening {
            if let Some(key) = target.as_ref() {
                if let Some(tile) = self.tiles.iter().find(|t| &t.key() == key) {
                    debug!("audio now follows {}", tile.title);
                }
            }
            self.listening = target;
        }
    }

    /// Ask the camera for a full-resolution still rather than saving the
    /// decoded sub-stream frame the grid happens to be showing.
    ///
    /// A virtual camera saves the same still cut down to its own framing, which
    /// is the sharpest that crop can be: a 2.5x view of a 4K picture is still
    /// over 1600 across, where the stream it is drawn from is a fraction of
    /// that.
    fn take_snapshot(&mut self) {
        let Some(key) = self.selected.clone() else { return };
        let Some(client) = self.manager.client(&key.device) else { return };
        let Some(tile) = self.tiles.iter().find(|t| t.key() == key) else { return };

        let directory = self.store.preferences.snapshots_dir();
        let safe: String = tile
            .title
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = directory.join(format!("{safe}-{stamp}.jpg"));
        // The vendor needs the whole channel: its own identifier is what the
        // snapshot URL is built from, and that is not always the index.
        let channel = tile.channel.clone();
        // The framing as it is on screen, not as it was saved: what somebody
        // asks a picture of is what they are looking at.
        let crop = tile.is_virtual().then(|| tile.framing());

        self.notify(format!("Saving {}", path.display()));
        std::thread::spawn(move || {
            let _ = std::fs::create_dir_all(&directory);
            match client.snapshot(&channel) {
                Ok(bytes) => {
                    let bytes = match crop {
                        Some((zoom, centre)) => match crop_jpeg(&bytes, zoom, centre) {
                            Some(cropped) => cropped,
                            None => {
                                // A still that cannot be read is still a still.
                                // Saving the whole picture beats saving nothing
                                // and explaining why in a log nobody reads.
                                log::warn!("could not crop snapshot; saving the full picture");
                                bytes
                            }
                        },
                        None => bytes,
                    };
                    if let Err(err) = std::fs::write(&path, bytes) {
                        log::warn!("could not write snapshot: {err}");
                    }
                }
                Err(err) => log::warn!("snapshot failed: {err}"),
            }
        });
    }

    /// Record the stream to MP4, without re-encoding it.
    ///
    /// Recording a virtual camera records its parent's whole picture. Kestrel
    /// remuxes packets rather than re-encoding them — the vendored ffmpeg is a
    /// decode-only LGPL build, which is the whole reason the binary can be
    /// distributed the way it is — and there is no way to narrow a picture
    /// without encoding a new one. It is also the better failure: you can crop
    /// a recording afterwards and you can never widen one.
    fn toggle_recording(&mut self, start: bool) {
        let Some(key) = self.selected.clone() else { return };
        let directory = self.store.preferences.recordings_dir();
        let Some(tile) = self.tiles.iter().find(|t| t.key() == key) else { return };
        let title = tile.title.clone();
        let cropped = tile.is_virtual();
        let Some(worker) = self.streams.hot_worker(&key.stream_key()) else {
            self.notify("Cannot record: stream is not running");
            return;
        };

        if !start {
            worker.stop_recording();
            self.notify("Recording stopped");
            return;
        }

        let safe: String = title
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = directory.join(format!("{safe}-{stamp}.mp4"));
        let _ = std::fs::create_dir_all(&directory);
        worker.start_recording(path.clone());
        if cropped {
            // Said at the moment it starts, which is the moment somebody is
            // looking. Finding out afterwards that the file is wider than the
            // camera that made it is finding out too late.
            self.notify(format!(
                "Recording {title} — the whole picture, not the crop"
            ));
        } else {
            self.notify(format!("Recording to {}", path.display()));
        }
    }

    /// Move a dual-lens camera between its wide and telephoto channels.
    ///
    /// The two lenses are two channels on one device, so switching means
    /// streaming the other one behind the same tile. Only that tile reconnects.
    /// Scroll on a camera that can zoom without cropping.
    ///
    /// A wheel emits many small deltas, and neither target tolerates one command
    /// per delta: a dual-lens camera would tear its stream down and rebuild it
    /// repeatedly, and a zoom motor would be handed a queue it cannot keep up
    /// with. So the notches accumulate and only cross into an action once they
    /// pass a threshold.
    fn optical_zoom(&mut self, key: Key, notches: f32) {
        let accumulated = self.zoom_accumulator.entry(key.clone()).or_insert(0.0);
        *accumulated += notches;
        if accumulated.abs() < SCROLL_ZOOM_THRESHOLD {
            return;
        }
        let inward = *accumulated > 0.0;
        *accumulated = 0.0;

        let Some(tile) = self.tiles.iter().find(|t| t.stream_key() == key) else { return };
        if tile.channel.is_dual_lens() {
            // The two lenses *are* the zoom steps on these cameras.
            self.switch_lens(key, inward);
            return;
        }

        let channel = tile.channel.index;
        let Some(client) = self.manager.client(&key.0) else { return };
        let direction = if inward { "zoom_in" } else { "zoom_out" };
        let speed = self.rail.speed();
        std::thread::spawn(move || {
            if client.ptz_move(channel, direction, speed).is_ok() {
                // A notch is a nudge: the firmware runs the move until told to
                // stop, so a scroll that never stopped would run away.
                std::thread::sleep(std::time::Duration::from_millis(300));
                let _ = client.ptz_stop(channel);
            }
        });
    }

    fn switch_lens(&mut self, key: Key, to_tele: bool) {
        let Some(index) = self
            .tiles
            .iter()
            .position(|t| !t.is_virtual() && t.stream_key() == key)
        else {
            return;
        };
        let partner = self.tiles[index].channel.lens_partner;
        let (Some(partner), true) = (partner, self.tiles[index].channel.is_dual_lens()) else {
            return;
        };

        let target = if to_tele { partner } else { key.1 };
        let current = self.lens_override.get(&key).copied().unwrap_or(key.1);
        if current == target {
            return;
        }
        if to_tele {
            self.lens_override.insert(key.clone(), target);
        } else {
            self.lens_override.remove(&key);
        }

        let stream = self.wanted_stream(self.maximized.is_some());
        let mut tile = self.tiles.remove(index);
        // The old connection is on the wrong lens for every tile of this
        // camera, so it goes rather than being handed on.
        self.streams.drop_hot(&key);
        self.connect_tile(&mut tile, stream);
        let title = tile.title.clone();
        self.tiles.insert(index, tile);
        self.notify(format!(
            "{title}: {} lens",
            if to_tele { "telephoto" } else { "wide" }
        ));
    }

    /// Apply a choice from a tile's context menu.
    fn tile_action(&mut self, key: SourceId, what: TileAction) {
        match what {
            TileAction::ToggleExpand => {
                self.maximized = if self.maximized.is_some() {
                    None
                } else {
                    self.spotlight.clear();
                    Some(key)
                };
                self.rebuild();
            }
            TileAction::Fullscreen => self.pending_fullscreen = Some(true),
            TileAction::Snapshot => self.take_snapshot(),
            TileAction::Record(start) => self.toggle_recording(start),
            TileAction::SetHidden(hidden) => self.set_camera_hidden(key, hidden),
            TileAction::SetFollowed(follows) => self.set_camera_followed(key, follows),
            TileAction::ResetZoom => {
                if let Some(tile) = self.tiles.iter_mut().find(|t| t.key() == key) {
                    tile.reset_zoom();
                }
            }
            TileAction::OpticalZoom(notches) => self.optical_zoom(key.stream_key(), notches),
            TileAction::Reconnect => {
                if let Some(index) = self.tiles.iter().position(|t| t.key() == key) {
                    let stream = self.wanted_stream(self.maximized.is_some());
                    let mut tile = self.tiles.remove(index);
                    // Every tile of this camera loses its picture, because
                    // there was only ever one connection under them.
                    self.streams.drop_hot(&key.stream_key());
                    for other in self.tiles.iter_mut() {
                        if other.stream_key() == key.stream_key() {
                            other.stream = None;
                        }
                    }
                    self.connect_tile(&mut tile, stream);
                    self.tiles.insert(index, tile);
                    self.notify("Reconnecting");
                    // Bring the camera's other views back onto the new
                    // connection rather than leaving them black until the next
                    // rebuild happens to touch them.
                    self.rebuild();
                }
            }
            TileAction::Lens(to_tele) => self.switch_lens(key.stream_key(), to_tele),
            TileAction::CreateVirtual => self.begin_framing(&key),
            TileAction::SaveFraming => self.save_framing(&key),
            TileAction::EditVirtual => self.open_virtual_settings(key),
            TileAction::DeleteVirtual => self.delete_virtual_camera(&key),
        }
    }

    fn live_grid(&mut self, ui: &mut egui::Ui) {
        // The radar takes a cell on the wall on the terms the setting names.
        // Not while one camera fills the pane: expanding is a request to see
        // that camera and nothing else.
        let alone = self.maximized.is_some() || !self.spotlight.is_empty();
        let radar_mode = if alone {
            crate::config::RadarTile::Never
        } else {
            self.store.preferences.radar_tile_mode()
        };
        let wants_radar = radar_mode != crate::config::RadarTile::Never;

        // The forecast follows the same rule, and is switched off in the same
        // place: expanding a camera is a request to see that camera.
        let forecast_mode = if alone {
            crate::config::ForecastTiles::Never
        } else {
            self.store.preferences.forecast_tile_mode()
        };
        let reserved = if alone {
            0
        } else {
            self.store.preferences.forecast_reserved()
        };
        let wants_forecast = forecast_mode != crate::config::ForecastTiles::Never;

        if self.tiles.is_empty() && !wants_radar && !wants_forecast {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new("No cameras yet\n\nAdd a camera or NVR to get started")
                        .size(15.0)
                        .color(theme::PLACEHOLDER),
                );
            });
            return;
        }

        let area = ui.available_rect_before_wrap();
        // Remembered so the pointer can be hidden over the pictures without
        // hiding it over the toolbar or the camera list.
        self.grid_rect = Some(area);
        let cameras = self.tiles.len();

        let (columns, rows, radar_slot) = wall_layout(cameras, radar_mode, reserved);

        let gap = 2.0;
        let cell = Vec2::new(
            (area.width() - gap * (columns as f32 - 1.0)) / columns as f32,
            (area.height() - gap * (rows as f32 - 1.0)) / rows as f32,
        );

        let selected = self.selected.clone();
        let mut clicked: Option<SourceId> = None;
        let mut activated: Option<SourceId> = None;
        let mut action: Option<(SourceId, TileAction)> = None;
        let mut accepted_framing = false;

        // Taken before the tiles are borrowed mutably below.
        let radar_info = self.radar_poller.as_ref().map(|poller| poller.latest());

        let expanded_now = self.maximized.is_some();
        for (index, tile) in self.tiles.iter_mut().enumerate() {
            let (row, column) = (index / columns, index % columns);
            let origin = area.min
                + Vec2::new(
                    column as f32 * (cell.x + gap),
                    row as f32 * (cell.y + gap),
                );
            let rect = Rect::from_min_size(origin, cell);
            let is_selected = selected.as_ref() == Some(&tile.key());
            let response = tile.show(ui, rect, is_selected);
            // Scroll the tile could not act on itself: a real zoom motor, or a
            // dual-lens camera whose lenses are the zoom steps.
            let scrolled = tile.take_zoom_request();
            if scrolled != 0.0 {
                action = Some((tile.key(), TileAction::OpticalZoom(scrolled)));
            }
            if response.clicked() {
                clicked = Some(tile.key());
            }
            // A tile choosing a crop spends its double-click on accepting the
            // box, so expanding is out of the way until the box is gone.
            if tile.take_framing_accepted() {
                accepted_framing = true;
            } else if response.double_clicked() && !tile.is_framing() {
                activated = Some(tile.key());
            }

            let key = tile.key();
            let recording = tile
                .stream
                .as_ref()
                .map(|s| s.recording_path().is_some())
                .unwrap_or(false);
            // A crop never drives the lens: the framing it was made to hold
            // still is the first thing a lens command would move.
            let is_virtual = tile.is_virtual();
            let dual_lens = tile.channel.is_dual_lens() && !is_virtual;
            let on_tele = tile.on_tele;
            let zoomed = tile.digital_zoom() > 1.0;
            let off_framing = tile.is_off_framing();
            let has_picture = tile.has_picture();
            let device = self.store.devices.iter().find(|d| d.id == tile.device_id());
            let hidden_now = device
                .map(|d| match &key.view {
                    Some(view) => d.virtual_camera(view).map(|v| v.hidden).unwrap_or(false),
                    None => d.is_hidden(tile.channel.index),
                })
                .unwrap_or(false);
            // Read off the device rather than through `follows_motion`: the
            // tiles are borrowed mutably for the length of this loop, and the
            // device list is a field of its own.
            let follows_now = device
                .map(|d| match &key.view {
                    Some(view) => d.view_follows_motion(view),
                    None => d.follows_motion(tile.channel.index),
                })
                .unwrap_or(true);
            response.context_menu(|ui| {
                if ui
                    .button(if expanded_now { "Back to grid" } else { "Expand to fill" })
                    .clicked()
                {
                    action = Some((key.clone(), TileAction::ToggleExpand));
                    ui.close_menu();
                }
                if ui.button("Fullscreen").clicked() {
                    action = Some((key.clone(), TileAction::Fullscreen));
                    ui.close_menu();
                }
                // The two kinds of tile want opposite things from the same
                // gesture. On a camera, a zoom is a temporary look at a corner
                // and the only questions are whether to undo it or keep it. On
                // a crop, the framing *is* the camera, so the questions are
                // about editing a thing that already exists.
                ui.separator();
                if is_virtual {
                    if off_framing && ui.button("Save this framing").clicked() {
                        action = Some((key.clone(), TileAction::SaveFraming));
                        ui.close_menu();
                    }
                    if off_framing && ui.button("Reset to saved framing").clicked() {
                        action = Some((key.clone(), TileAction::ResetZoom));
                        ui.close_menu();
                    }
                    if ui.button("Virtual camera settings…").clicked() {
                        action = Some((key.clone(), TileAction::EditVirtual));
                        ui.close_menu();
                    }
                    if ui
                        .button("Delete this virtual camera")
                        .on_hover_text("The camera it was cut out of stays")
                        .clicked()
                    {
                        action = Some((key.clone(), TileAction::DeleteVirtual));
                        ui.close_menu();
                    }
                } else {
                    if zoomed && ui.button("Reset zoom").clicked() {
                        action = Some((key.clone(), TileAction::ResetZoom));
                        ui.close_menu();
                    }
                    // Always offered. It used to want the tile zoomed in
                    // first, on the reasoning that a crop of the whole picture
                    // is just the camera again — but that put the feature
                    // behind a gesture, and behind one that some cameras do not
                    // even have: scroll on a camera with a zoom motor drives
                    // the lens, so the item could never light up at all there.
                    // It opens a box on the picture instead, which is the
                    // question it was always asking.
                    if ui
                        .add_enabled(
                            has_picture,
                            egui::Button::new("Make a virtual camera…"),
                        )
                        .on_hover_text(if zoomed {
                            "Keep this framing as a camera of its own"
                        } else {
                            "Choose part of the picture to keep as a camera"
                        })
                        // The one honest reason to refuse: you cannot choose
                        // part of a picture that is not there yet.
                        .on_disabled_hover_text("Waiting for the picture")
                        .clicked()
                    {
                        action = Some((key.clone(), TileAction::CreateVirtual));
                        ui.close_menu();
                    }
                }
                ui.separator();
                if ui
                    .button(if hidden_now { "Show this camera" } else { "Hide this camera" })
                    .on_hover_text(if hidden_now {
                        "Put it back on the wall"
                    } else {
                        "Keep it off the wall until you show it again"
                    })
                    .clicked()
                {
                    action = Some((key.clone(), TileAction::SetHidden(!hidden_now)));
                    ui.close_menu();
                }
                // Behind a separator rather than flush under "Hide this
                // camera". They are the same kind of decision about the same
                // camera, which is the argument for adjacency - and the
                // argument against is stronger: a right-click-drag-release
                // lands on whatever item the pointer drifted onto, and an item
                // one row below a familiar one gets hit by muscle memory. The
                // two do different things and only one of them is visible
                // afterwards.
                ui.separator();
                if ui
                    .button(if follows_now {
                        "Don't follow motion here"
                    } else {
                        "Follow motion here"
                    })
                    .on_hover_text(if follows_now {
                        "Leave it out when the view follows motion — it stays on the wall"
                    } else {
                        "Let it bring the view here again"
                    })
                    .clicked()
                {
                    action = Some((key.clone(), TileAction::SetFollowed(!follows_now)));
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Snapshot").clicked() {
                    action = Some((key.clone(), TileAction::Snapshot));
                    ui.close_menu();
                }
                if ui
                    .button(if recording { "Stop recording" } else { "Start recording" })
                    .clicked()
                {
                    action = Some((key.clone(), TileAction::Record(!recording)));
                    ui.close_menu();
                }
                if dual_lens {
                    ui.separator();
                    if ui
                        .button(if on_tele { "Wide lens" } else { "Telephoto lens" })
                        .clicked()
                    {
                        action = Some((key.clone(), TileAction::Lens(!on_tele)));
                        ui.close_menu();
                    }
                }
                ui.separator();
                if ui.button("Reconnect").clicked() {
                    action = Some((key.clone(), TileAction::Reconnect));
                    ui.close_menu();
                }
            });
        }

        if let Some(slot) = radar_slot {
            let (row, column) = (slot / columns, slot % columns);
            let rect = Rect::from_min_size(
                area.min
                    + Vec2::new(
                        column as f32 * (cell.x + gap),
                        row as f32 * (cell.y + gap),
                    ),
                cell,
            );

            let home = self.radar_home_for(rect.height());
            let mut view = self.radar_view.unwrap_or(home);
            let default = crate::weather::poller::RadarInfo::default();
            let asked = self.radar_tile_view.show(
                ui,
                rect,
                radar_info.as_deref().unwrap_or(&default),
                &self.store.preferences.radar_basemap,
                self.store.preferences.clock_is_24_hour(),
                self.store.preferences.weather_metric,
                &mut view,
                home,
                // A cell on the wall is the same map without the furniture
                // there is no room for.
                true,
            );
            self.radar_view = Some(view);
            self.radar_on_the_wall = true;

            // Double-clicking a camera fills the pane with it; double-clicking
            // the radar opens the one that has the scrubber, the legend and the
            // warnings on it — the same promise, kept where it can be.
            if asked == super::radar::Asked::OpenFull {
                self.tab = Tab::Weather;
                self.weather.showing = super::weather::PaneTab::Radar;
            }
        }

        // Whatever cells are left after the cameras and the radar take the
        // front of the forecast, one period per cell, in the order the service
        // sends them — so the spare cell on a five-camera wall is tonight, and
        // a seven-camera wall gets tonight and tomorrow.
        //
        // On `Spare` nothing is displaced to make room and nothing is added to
        // make a period fit: a wall that tiles exactly shows none of these, and
        // what is coming next is in the strip at the top on every wall anyway.
        // On `Always` the cells were counted before the shape was chosen, so
        // there are at least as many as were asked for — and any the cameras
        // happened to leave over on top of that are used too, because an empty
        // cell beside a forecast is not an improvement on another period.
        //
        // Gated on the weather being on rather than on the strip being up. In
        // the Roku channel the two come together; here the strip has a height
        // of its own and turning it off is a decision about *pixels over the
        // cameras* — which says nothing about a cell no camera wanted.
        let mut open_period: Option<usize> = None;
        if wants_forecast && self.weather_showing() {
            let taken = cameras + usize::from(radar_slot.is_some());
            let free = spare_cells(columns, rows, cameras, radar_slot.is_some());
            let periods = self.weather.periods();
            // Every cell that was promised one is drawn even before a reading
            // lands, so the wall keeps its shape and an empty cell in a row of
            // cameras does not read as a camera that failed.
            let cells = free.min(periods.len().max(reserved));
            for index in 0..cells {
                let slot = taken + index;
                let (row, column) = (slot / columns, slot % columns);
                let rect = Rect::from_min_size(
                    area.min
                        + Vec2::new(
                            column as f32 * (cell.x + gap),
                            row as f32 * (cell.y + gap),
                        ),
                    cell,
                );
                match periods.get(index) {
                    Some(period) => {
                        if super::weather::forecast_tile(ui, rect, period).clicked() {
                            open_period = Some(index);
                        }
                    }
                    None => super::weather::forecast_waiting(
                        ui,
                        rect,
                        self.weather.waiting_note(),
                    ),
                }
            }
        }
        if let Some(index) = open_period {
            // The cell has room for a line of it; the pane has the wording.
            self.tab = Tab::Weather;
            self.weather.show_period(index);
        }

        if accepted_framing {
            self.accept_framing();
        }
        if let Some(key) = clicked {
            self.selected = Some(key);
        }
        if let Some((key, what)) = action {
            self.selected = Some(key.clone());
            self.tile_action(key, what);
        }
        if let Some(key) = activated {
            // Double-click fills the pane; again returns to the grid.
            self.maximized = if self.maximized.is_some() {
                None
            } else {
                self.spotlight.clear();
                Some(key)
            };
            self.rebuild();
        }
    }
}

impl eframe::App for KestrelApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The channel list appears asynchronously as devices connect.
        let revision = self.manager.sources().len();
        if revision != self.sources_revision {
            self.sources_revision = revision;
            self.rebuild();
        }

        self.pointer_present = ctx.input(|i| i.pointer.has_pointer());

        // Any pointer movement brings the camera names back.
        if ctx.input(|i| {
            i.pointer.is_moving()
                || i.events.iter().any(|e| {
                    matches!(
                        e,
                        egui::Event::PointerMoved(_)
                            | egui::Event::PointerButton { .. }
                            | egui::Event::MouseWheel { .. }
                    )
                })
        }) {
            self.last_pointer_move = std::time::Instant::now();
        }

        self.apply_upgrades();
        self.reconcile_audio();
        self.streams.expire_parked();
        self.top_up_warm();
        self.pump_events();

        self.reconcile_weather();

        // Fullscreen is the case a screen blanker is built to catch: a live
        // picture nobody is touching. Cheap to call every frame — the request
        // only crosses to the bus when it changes.
        self.inhibitor.set(
            self.fullscreen && self.store.preferences.keep_awake_fullscreen,
            "Showing live cameras fullscreen",
        );

        self.handle_keys(ctx);
        // The top bar goes the same way the camera names do, off the same idle
        // timer, so a wall settling down does it in one movement rather than
        // two. Drawn or not drawn rather than faded: it is a panel and the grid
        // takes the height it leaves, and a toolbar dissolving in place while
        // the video grows underneath it reads as a glitch.
        if self.header_showing() {
            self.header(ctx);
        }

        // The strip stays up in fullscreen, unlike the sidebar and the rail: it
        // is part of the wall rather than something for managing it, and a
        // fullscreen camera wall with the weather on it is the whole point of
        // having one.
        if self.tab != Tab::Weather && self.store.preferences.weather_bar && self.weather_showing() {
            let height = self.store.preferences.weather_bar_height.clamp(72.0, 260.0);
            egui::TopBottomPanel::top("weather")
                .exact_height(height)
                .frame(egui::Frame::none().fill(theme::PANEL))
                .show(ctx, |ui| {
                    let rect = ui.max_rect();
                    self.weather.bar(ui, rect, &self.store.preferences);
                });
        }

        // Fullscreen is for watching, not managing: give the video the screen.
        if !self.fullscreen && self.sidebar_open {
            self.sidebar(ctx);
            // The rail drives the *live* camera; it has nothing to say about a
            // recording, so Playback gets the width instead.
            if self.tab == Tab::Live {
                self.control_rail(ctx);
            }
        }

        match self.dialog.show(ctx) {
            Outcome::Save(device) => self.save_device(device),
            Outcome::Remove(id) => self.remove_device(&id),
            Outcome::None => {}
        }
        if let PrefsOutcome::Save(updated) = self.prefs.show(ctx) {
            self.apply_preferences(*updated);
        }
        if let Some(on) = self.pending_fullscreen.take() {
            self.set_fullscreen(ctx, on);
        }
        self.about_window(ctx);
        self.virtual_window(ctx);

        // The sweep is held for the length of the frame that draws it: the
        // radar is megabytes of pixels and the pane only borrows them.
        let radar_info = self.radar_poller.as_ref().map(|poller| poller.latest());
        let mut radar_in_the_tab = false;
        // Cleared before the pane is drawn, so only a grid that actually put
        // the radar on screen this frame counts as showing it.
        self.radar_on_the_wall = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::INK))
            .show(ctx, |ui| match self.tab {
                Tab::Live => self.live_grid(ui),
                Tab::Playback => self.playback.show(ui, &self.manager),
                Tab::Weather => {
                    // The grid is deliberately flush to the edges — video
                    // should use every pixel it is given — but a page of text
                    // set hard against the window is unreadable, so the
                    // weather gets the margin the camera wall does not want.
                    let home = self.radar_home_for(ui.available_height());
                    let mut view = self.radar_view.unwrap_or(home);
                    let mut showing = false;
                    egui::Frame::none()
                        .inner_margin(egui::Margin::symmetric(22, 16))
                        .show(ui, |ui| {
                            showing = self.weather.pane(
                                ui,
                                &self.store.preferences,
                                radar_info.as_deref(),
                                &mut view,
                                home,
                            );
                        });
                    radar_in_the_tab = showing;
                    self.radar_view = Some(view);
                    // The radar asked to be left. The shell owns the tabs, so
                    // it is the one that can honour it.
                    if self.weather.take_leaving() {
                        self.tab = Tab::Live;
                        radar_in_the_tab = false;
                    }
                }
            });
        drop(radar_info);
        self.reconcile_radar(radar_in_the_tab || self.radar_on_the_wall);

        if let Some((message, shown_at)) = &self.status {
            if shown_at.elapsed() < std::time::Duration::from_secs(4) {
                let text = message.clone();
                egui::Area::new("toast".into())
                    .anchor(egui::Align2::CENTER_BOTTOM, Vec2::new(0.0, -24.0))
                    .show(ctx, |ui| {
                        egui::Frame::none()
                            .fill(theme::PANEL_ALT)
                            .stroke(egui::Stroke::new(1.0, theme::BORDER))
                            .corner_radius(9)
                            .inner_margin(egui::Margin::symmetric(16, 9))
                            .show(ui, |ui| {
                                // A toast is one line unless it genuinely will
                                // not fit. An Area sizes itself to its content,
                                // so there is no width to wrap against and egui
                                // wraps to whatever it guessed first — which
                                // turned "Motion: Driveway" into three lines in
                                // a chip narrower than the words in it.
                                //
                                // Long ones do exist, and they are the ones
                                // worth reading whole: a snapshot or a
                                // recording names the file it wrote. So the
                                // line is measured, and wrapping happens only
                                // past a share of the window.
                                let font = egui::TextStyle::Body.resolve(ui.style());
                                let one_line = ui
                                    .painter()
                                    .layout_no_wrap(text.clone(), font, theme::TEXT)
                                    .size()
                                    .x;
                                let room = toast_room(ui.ctx().screen_rect().width());
                                if one_line > room {
                                    ui.set_max_width(room);
                                }
                                ui.add(egui::Label::new(text).wrap_mode(if one_line > room {
                                    egui::TextWrapMode::Wrap
                                } else {
                                    egui::TextWrapMode::Extend
                                }));
                            });
                    });
            } else {
                self.status = None;
            }
        }

        self.service_pointer(ctx);
        self.service_screenshot(ctx);

        // Live video means continuous repaints; without this egui would only
        // redraw on input and the picture would appear frozen.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.playback.release();
        // Give the screen back before anything slow happens, so a shutdown that
        // takes a moment does not hold the machine awake through it.
        self.inhibitor.set(false, "");
        self.weather_poller = None;
        self.radar_poller = None;
        self.tiles.clear();
        // Release the device sessions before the process goes away.
        self.manager.shutdown();
        // Dropping a running decode thread aborts; give them a moment to unwind.
        self.streams.shutdown();
        // Only write if the user actually changed something: the PyQt client
        // shares this file and may have been the one to touch it.
        let _ = self.store.save_if_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(device: &str, channel: u32, view: Option<&str>) -> Source {
        Source {
            id: match view {
                Some(view) => SourceId::virtual_camera(device, channel, view),
                None => SourceId::camera(device, channel),
            },
            channel: crate::api::Channel::new(channel),
            view: view.map(|id| VirtualCamera {
                id: id.into(),
                channel,
                name: id.into(),
                zoom: 2.5,
                centre: (0.5, 0.5),
                hidden: false,
            }),
        }
    }

    /// A toast is a few words and wants one line. It used to wrap against a
    /// width nothing had chosen — an Area sizes itself to its content, so there
    /// was no width to wrap against and egui used whatever it guessed first,
    /// which put "Motion: Back Garden" on three lines in a chip narrower than
    /// the words in it.
    #[test]
    fn a_toast_has_room_for_what_toasts_actually_say() {
        // The widest thing said routinely is a camera name after a word or two.
        // At a generous eight points a character even the narrowest toast
        // allowed has room for it several times over.
        const GENEROUS: f32 = 8.0;
        for message in [
            "Motion: Back Garden",
            "Motion on 4 cameras",
            "Recording stopped",
            "Showing Front gate",
        ] {
            let needs = message.len() as f32 * GENEROUS;
            assert!(
                needs < toast_room(640.0),
                "{message:?} wants {needs} and the narrowest toast offers {}",
                toast_room(640.0)
            );
        }
    }

    /// The bound is the window rather than a fixed width, so the wall it runs
    /// on decides — and a small window still gets a usable chip rather than a
    /// column of single words.
    #[test]
    fn a_toast_is_bounded_by_the_window_but_never_squeezed() {
        // Never below the floor, however small the window claims to be.
        assert_eq!(toast_room(0.0), TOAST_LEAST);
        assert_eq!(toast_room(100.0), TOAST_LEAST);

        // Wider windows give more room, and a 4K wall gives plenty — a written
        // file's whole path is what the long toasts are for.
        assert!(toast_room(1920.0) > toast_room(1280.0));
        assert!(toast_room(3840.0) > toast_room(1920.0));
        assert!(
            toast_room(3840.0) > 2000.0,
            "a 4K wall should fit a path without wrapping"
        );

        // And never the whole window: a toast that reached both edges would
        // stop reading as a chip over the video.
        for window in [1280.0, 1920.0, 3840.0_f32] {
            assert!(toast_room(window) < window * 0.8);
        }
    }

    /// A crop exists to magnify, so it pulls the main stream — and its parent
    /// comes with it, because there is only one connection under them both.
    #[test]
    fn a_crop_on_the_wall_puts_its_camera_on_the_main_stream() {
        let wall = vec![
            source("nvr", 0, None),
            source("nvr", 0, Some("gate")),
            source("nvr", 1, None),
        ];
        let chosen = wanted_streams(&wall, StreamType::Sub, true);

        assert_eq!(chosen.len(), 2, "one connection per camera, not per tile");
        assert_eq!(chosen[&("nvr".to_string(), 0)], StreamType::Main);
        assert_eq!(
            chosen[&("nvr".to_string(), 1)],
            StreamType::Sub,
            "a camera with no crop on the wall is unaffected"
        );
    }

    /// Main wins wherever the two disagree, whichever order the wall is in.
    /// Handing the crop the sub stream to spare its parent the bandwidth makes
    /// the crop useless, which is the one thing the parent was fine without.
    #[test]
    fn the_sharper_stream_wins_however_the_wall_is_ordered() {
        for wall in [
            vec![source("nvr", 0, None), source("nvr", 0, Some("gate"))],
            vec![source("nvr", 0, Some("gate")), source("nvr", 0, None)],
        ] {
            let chosen = wanted_streams(&wall, StreamType::Sub, true);
            assert_eq!(chosen[&("nvr".to_string(), 0)], StreamType::Main);
        }
    }

    /// With promotion switched off a crop takes whatever the wall is on, which
    /// is the point of the setting: a large wall stays light.
    #[test]
    fn promotion_can_be_declined() {
        let wall = vec![source("nvr", 0, None), source("nvr", 0, Some("gate"))];
        let chosen = wanted_streams(&wall, StreamType::Sub, false);
        assert_eq!(chosen[&("nvr".to_string(), 0)], StreamType::Sub);

        // And an expanded camera is still on main whatever the setting says,
        // because that decision was already made before this one.
        let chosen = wanted_streams(&wall, StreamType::Main, false);
        assert_eq!(chosen[&("nvr".to_string(), 0)], StreamType::Main);
    }

    /// A crop is a camera as far as the wall is concerned: it takes a cell and
    /// changes the shape of the grid like any other.
    #[test]
    fn crops_are_counted_as_cameras_when_the_wall_is_laid_out() {
        use crate::config::RadarTile;
        // Three cameras and one crop of one of them is four cells, not three.
        let (columns, rows, _) = wall_layout(4, RadarTile::Never, 0);
        assert_eq!((columns, rows), (2, 2));
        let (columns, rows, _) = wall_layout(3, RadarTile::Never, 0);
        assert_eq!((columns, rows), (2, 2), "and three still fits in four cells");
        // Six of them keep the widescreen shape the rule was written for.
        let (columns, rows, _) = wall_layout(6, RadarTile::Never, 0);
        assert_eq!((columns, rows), (3, 2));
    }

    use crate::config::RadarTile;

    /// The rule the Roku channel settled on: a spare cell costs the cameras
    /// nothing, and taking one always costs them all a little size.
    #[test]
    fn a_spare_cell_is_only_taken_when_the_cameras_left_one() {
        // Four cameras tile exactly as 2x2, so there is nothing spare.
        assert_eq!(wall_layout(4, RadarTile::Spare, 0), (2, 2, None));
        assert_eq!(wall_layout(9, RadarTile::Spare, 0), (3, 3, None));
        assert_eq!(wall_layout(16, RadarTile::Spare, 0), (4, 4, None));

        // Three do not: the fourth cell is going spare.
        assert_eq!(wall_layout(3, RadarTile::Spare, 0), (2, 2, Some(3)));
        // Nor do five, seven or eight.
        assert_eq!(wall_layout(5, RadarTile::Spare, 0).2, Some(5));
        assert_eq!(wall_layout(8, RadarTile::Spare, 0).2, Some(8));
    }

    /// Folding is remembered and revealing is not, and the difference is the
    /// point: a 36-channel box you folded once you want folded tomorrow, while
    /// revealing is how you undo hiding rather than a way to live.
    #[test]
    fn folding_is_kept_in_order_and_reversible() {
        let mut folded: Vec<String> = Vec::new();

        assert!(fold_device(&mut folded, "nvr-b", true));
        assert!(!fold_device(&mut folded, "nvr-b", true), "folding twice is not a change");
        assert!(fold_device(&mut folded, "nvr-a", true));
        assert_eq!(folded, vec!["nvr-a", "nvr-b"], "the file reads in order");

        assert!(fold_device(&mut folded, "nvr-a", false));
        assert_eq!(folded, vec!["nvr-b"], "and only that one opened");
        assert!(!fold_device(&mut folded, "nvr-a", false));

        // The preference it lives in starts empty, so nothing is folded until
        // somebody says so.
        assert!(crate::config::Preferences::default().collapsed_devices.is_empty());
    }

    /// The top bar goes off the same idle rule the camera names do, so a wall
    /// settling down clears itself in one movement. Off, it never goes at all.
    #[test]
    fn the_top_bar_follows_the_same_idle_rule_as_the_names() {
        // Asked to hide: present while the pointer is busy, gone once it is not.
        assert_eq!(title_visibility(true, 0.0, 2.0, true).1, 1.0, "just moved");
        assert!(title_visibility(true, 10.0, 2.0, true).1 <= 0.0, "long idle");
        // A pointer that has left the window is not about to do anything.
        assert_eq!(title_visibility(true, 0.0, 2.0, false).1, 0.0);
        // Not asked to hide: nothing fades, whatever the timer says.
        assert_eq!(title_visibility(false, 600.0, 2.0, false), (false, 1.0));
    }

    /// A mode that is switched on and cannot fire looks exactly like a quiet
    /// night, so it has to say so. Both ways of getting there are silent: no
    /// detection types chosen, and every camera left out one right-click at a
    /// time.
    #[test]
    fn following_says_when_it_could_never_fire() {
        // The ordinary case says nothing at all.
        assert_eq!(nothing_to_follow(2, 8, 0), None);
        assert_eq!(nothing_to_follow(2, 8, 7), None, "one camera left is enough");

        // No detection types outranks the rest: it is the one set where it is
        // set, so naming it sends somebody to the right place.
        assert!(nothing_to_follow(0, 8, 0).unwrap().contains("detection types"));
        assert!(nothing_to_follow(0, 0, 0).is_some(), "even with no cameras");

        // Every camera left out, said as the whole rather than as a count to
        // be compared against one nothing shows.
        let all = nothing_to_follow(2, 8, 8).unwrap();
        assert!(all.contains("all 8 cameras"), "{all}");
        let one = nothing_to_follow(2, 1, 1).unwrap();
        assert!(one.contains("the only camera"), "{one}");

        // An empty wall is not this feature's problem to report.
        assert_eq!(nothing_to_follow(2, 0, 0), None);
    }

    /// The forecast takes whatever the cameras and the radar left, and never
    /// takes a cell from either.
    #[test]
    fn the_forecast_fills_the_cells_nothing_else_wanted() {
        // The Roku channel's own examples, which is what this replicates.
        let (columns, rows, radar) = wall_layout(5, RadarTile::Never, 0);
        assert_eq!(spare_cells(columns, rows, 5, radar.is_some()), 1, "3x2 with five");
        let (columns, rows, radar) = wall_layout(7, RadarTile::Never, 0);
        assert_eq!(spare_cells(columns, rows, 7, radar.is_some()), 2, "3x3 with seven");

        // A wall that tiles exactly shows none.
        for cameras in [1usize, 2, 4, 9, 16] {
            let (columns, rows, radar) = wall_layout(cameras, RadarTile::Never, 0);
            assert_eq!(
                spare_cells(columns, rows, cameras, radar.is_some()),
                0,
                "{cameras} cameras tile exactly"
            );
        }

        // The radar gets the first spare cell, and the forecast what is left.
        let (columns, rows, radar) = wall_layout(7, RadarTile::Spare, 0);
        assert_eq!(radar, Some(7), "the radar follows the last camera");
        assert_eq!(spare_cells(columns, rows, 7, true), 1, "and the forecast the rest");

        // Never more cells than there are, whatever the mode.
        for mode in [RadarTile::Never, RadarTile::Spare, RadarTile::Always] {
            for cameras in 0..20 {
                let (columns, rows, radar) = wall_layout(cameras, mode, 0);
                let free = spare_cells(columns, rows, cameras, radar.is_some());
                assert!(
                    cameras + usize::from(radar.is_some()) + free == columns * rows,
                    "{cameras} cameras in {mode:?}: {free} spare does not close the grid"
                );
            }
        }
    }

    /// Forced on, the forecast gets the cells it asked for whatever the camera
    /// count — including on the walls that tile exactly and would otherwise
    /// leave nothing over.
    #[test]
    fn a_forced_forecast_always_gets_its_cells() {
        for reserved in 1..=4usize {
            for cameras in 0..20 {
                for mode in [RadarTile::Never, RadarTile::Spare, RadarTile::Always] {
                    let (columns, rows, radar) = wall_layout(cameras, mode, reserved);
                    let free = spare_cells(columns, rows, cameras, radar.is_some());
                    assert!(
                        free >= reserved,
                        "{cameras} cameras, {mode:?}, {reserved} promised: only {free} over"
                    );
                    // And no camera lost its cell to make room.
                    assert!(columns * rows >= cameras, "a camera was dropped");
                }
            }
        }

        // Four cameras tile exactly as 2x2 and leave nothing; asking for two
        // periods grows the wall to 3x2 rather than dropping a camera.
        let (columns, rows, radar) = wall_layout(4, RadarTile::Never, 2);
        assert_eq!((columns, rows), (3, 2));
        assert_eq!(spare_cells(columns, rows, 4, radar.is_some()), 2);
    }

    /// A cell that exists only because the forecast asked for it is not a cell
    /// the cameras left over, so the radar's "spare" must not take it — its
    /// whole promise is that no camera is ever shrunk for the radar.
    #[test]
    fn the_radar_does_not_take_a_cell_the_forecast_created() {
        // Four cameras tile exactly. With two periods promised the wall becomes
        // a 3x2, and both new cells belong to the forecast.
        let (columns, rows, radar) = wall_layout(4, RadarTile::Spare, 2);
        assert_eq!((columns, rows), (3, 2));
        assert_eq!(radar, None, "the cameras left nothing over");
        assert_eq!(spare_cells(columns, rows, 4, false), 2);

        // Five cameras genuinely do leave one over, and the radar still gets it
        // — the forecast's promise is met by the cells past that.
        let (columns, rows, radar) = wall_layout(5, RadarTile::Spare, 2);
        assert_eq!(radar, Some(5), "a cell the cameras left over is still the radar's");
        assert!(spare_cells(columns, rows, 5, true) >= 2, "and the forecast keeps its two");
    }

    /// Four cameras and the radar lay out as six cells rather than four, so no
    /// camera is dropped to make room.
    #[test]
    fn taking_a_cell_always_grows_the_grid_instead_of_dropping_a_camera() {
        let (columns, rows, slot) = wall_layout(4, RadarTile::Always, 0);
        assert_eq!((columns, rows), (3, 2), "four cameras and the radar is a 3x2");
        assert_eq!(slot, Some(4), "the radar follows the last camera");
        assert!(columns * rows >= 5, "every camera still has a cell");

        // Whatever the count, the cameras all keep a cell.
        for cameras in 0..20 {
            let (columns, rows, slot) = wall_layout(cameras, RadarTile::Always, 0);
            assert!(columns * rows > cameras, "{cameras} cameras had no room for the radar");
            assert_eq!(slot, Some(cameras));
        }
    }

    #[test]
    fn the_wall_is_unchanged_when_the_radar_is_not_on_it() {
        for cameras in 1..20 {
            let (columns, rows, slot) = wall_layout(cameras, RadarTile::Never, 0);
            assert_eq!(slot, None);
            // The same shape the grid had before the radar existed.
            let expected = if cameras == 6 { 3 } else { (cameras as f32).sqrt().ceil() as usize };
            assert_eq!(columns, expected.max(1));
            assert!(columns * rows >= cameras);
        }
    }

    /// A wall with no cameras on it at all still shows the radar, rather than
    /// telling somebody to add a camera over the top of the weather.
    #[test]
    fn the_radar_can_be_the_only_thing_on_the_wall() {
        assert_eq!(wall_layout(0, RadarTile::Always, 0), (1, 1, Some(0)));
        assert_eq!(wall_layout(0, RadarTile::Spare, 0), (1, 1, Some(0)));
    }

    #[test]
    fn names_stay_put_when_auto_hide_is_off() {
        // However long the mouse sits still, and whatever the delay says.
        for idle in [0.0, 2.0, 600.0] {
            assert_eq!(title_visibility(false, idle, 2.0, true), (false, 1.0));
        }
    }

    #[test]
    fn names_fade_out_after_the_configured_delay() {
        // Fully visible right up to the delay.
        assert_eq!(title_visibility(true, 0.0, 2.0, true), (true, 1.0));
        assert_eq!(title_visibility(true, 1.9, 2.0, true), (true, 1.0));
        assert_eq!(title_visibility(true, 2.0, 2.0, true), (true, 1.0));

        // Then fading, and gone once the fade completes.
        let (_, mid) = title_visibility(true, 2.0 + TITLE_FADE / 2.0, 2.0, true);
        assert!((0.1..0.9).contains(&mid), "should be mid-fade, got {mid}");
        assert_eq!(title_visibility(true, 2.0 + TITLE_FADE, 2.0, true).1, 0.0);
        assert_eq!(title_visibility(true, 60.0, 2.0, true).1, 0.0);
    }

    /// The pointer leaving the window means nobody is pointing at anything.
    #[test]
    fn names_hide_at_once_when_the_pointer_leaves_the_window() {
        assert_eq!(title_visibility(true, 0.0, 2.0, false), (true, 0.0));
        // Even mid-delay, when they would otherwise be fully visible.
        assert_eq!(title_visibility(true, 0.5, 10.0, false).1, 0.0);
        // But an always-on configuration still keeps them.
        assert_eq!(title_visibility(false, 0.0, 2.0, false), (false, 1.0));
    }

    #[test]
    fn the_delay_is_honoured_rather_than_fixed() {
        // A long delay keeps them up well past the default two seconds.
        assert_eq!(title_visibility(true, 5.0, 10.0, true).1, 1.0);
        // A zero delay still fades rather than cutting.
        assert_eq!(title_visibility(true, 0.0, 0.0, true).1, 1.0);
        assert_eq!(title_visibility(true, TITLE_FADE, 0.0, true).1, 0.0);
    }
}
