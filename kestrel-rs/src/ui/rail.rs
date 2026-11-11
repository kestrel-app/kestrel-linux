//! The right-hand control rail: PTZ, presets, snapshot and recording.
//!
//! Every device call blocks on the network, so nothing here runs inline — each
//! action is handed to a thread and its outcome comes back through a shared
//! slot the UI reads on the next frame.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText, Vec2};
use log::warn;

use super::theme;
use crate::api::vendor::Vendor;
use crate::api::{Block, Channel};

/// How often a held direction is re-sent. Short enough that movement looks
/// continuous, long enough that a held button is a few requests a second rather
/// than one per rendered frame.
const MOVE_REPEAT: std::time::Duration = std::time::Duration::from_millis(350);

/// Reolink PTZ is hold-to-move: a direction command runs until an explicit
/// Stop, so the buttons drive on press and stop on release.
const DIRECTIONS: [(&str, &str); 8] = [
    ("leftup", "↖"),
    ("up", "↑"),
    ("rightup", "↗"),
    ("left", "←"),
    ("right", "→"),
    ("leftdown", "↙"),
    ("down", "↓"),
    ("rightdown", "↘"),
];

#[derive(Default)]
pub struct RailState {
    /// Which direction is being held, so a release can send Stop exactly once.
    active_direction: Option<&'static str>,
    /// When the current direction was last sent, for the repeat below.
    last_move_sent: Option<std::time::Instant>,
    speed: i64,
    /// Presets for the selected channel, loaded in the background.
    presets: Arc<Mutex<Vec<(i64, String)>>>,
    presets_for: Option<(String, u32)>,
    selected_preset: Option<i64>,
    pub status: Arc<Mutex<Option<String>>>,
    confirm_calibrate: bool,

    /// The camera's floodlight configuration, read in the background.
    floodlight: Arc<Mutex<Option<Block>>>,
    floodlight_for: Option<(String, u32)>,
    /// Brightness while the slider is being dragged, written on release so a
    /// drag does not fire a device write per frame.
    floodlight_bright: Option<i64>,
    /// A floodlight write is in flight. The controls are disabled until the
    /// device has confirmed it, so what is shown is always what the device
    /// reported and never a guess.
    floodlight_pending: Arc<AtomicUsize>,
}

impl RailState {
    pub fn new() -> Self {
        RailState {
            speed: 32,
            ..Default::default()
        }
    }

    fn report(&self, message: impl Into<String>) {
        *self.status.lock().unwrap() = Some(message.into());
    }

    /// Run a device call off the UI thread, reporting failures into the rail.
    fn spawn<F>(&self, what: &'static str, client: Arc<dyn Vendor>, action: F)
    where
        F: FnOnce(&dyn Vendor) -> crate::api::Result<()> + Send + 'static,
    {
        let status = Arc::clone(&self.status);
        std::thread::spawn(move || {
            if let Err(err) = action(client.as_ref()) {
                warn!("{what} failed: {err}");
                *status.lock().unwrap() = Some(format!("{what} failed: {err}"));
            }
        });
    }

    /// PTZ speed, shared with anything else that drives the camera.
    pub fn speed(&self) -> i64 {
        self.speed
    }

    /// Read the floodlight state when the selected camera changes.
    fn sync_floodlight(&mut self, client: &Arc<dyn Vendor>, key: (String, u32)) {
        if self.floodlight_for.as_ref() == Some(&key) {
            return;
        }
        self.floodlight_for = Some(key.clone());
        *self.floodlight.lock().unwrap() = None;
        self.floodlight_bright = None;

        let slot = Arc::clone(&self.floodlight);
        let client = Arc::clone(client);
        let channel = key.1;
        std::thread::spawn(move || match client.white_led(channel) {
            Ok(block) => *slot.lock().unwrap() = Some(block),
            Err(err) => warn!("could not read the floodlight for channel {channel}: {err}"),
        });
    }

    /// Change one floodlight field.
    ///
    /// Nothing is shown until the device has confirmed it. The camera applies a
    /// floodlight write asynchronously and keeps reporting the old state for up
    /// to ~2s, so the controls are disabled until the read-back agrees rather
    /// than predicting an outcome that might not arrive.
    fn set_floodlight(&self, client: &Arc<dyn Vendor>, field: &'static str, value: i64) {
        let Some(original) = self.floodlight.lock().unwrap().clone() else { return };
        *self.status.lock().unwrap() = None;
        self.floodlight_pending.fetch_add(1, Ordering::Relaxed);

        let slot = Arc::clone(&self.floodlight);
        let status = Arc::clone(&self.status);
        let pending = Arc::clone(&self.floodlight_pending);
        let client = Arc::clone(client);
        std::thread::spawn(move || {
            match client.set_floodlight(&original, field, value) {
                // What the device stored, not what we asked for.
                Ok(updated) => *slot.lock().unwrap() = Some(updated),
                Err(err) => {
                    warn!("floodlight {field} failed: {err}");
                    *slot.lock().unwrap() = Some(original);
                    *status.lock().unwrap() = Some(format!("Floodlight: {err}"));
                }
            }
            pending.fetch_sub(1, Ordering::Relaxed);
        });
    }

    /// Re-read presets whenever the selected camera changes.
    ///
    /// The camera's answer decides whether the controls are usable — the
    /// advertised `ptzPreset` ability is under-reported often enough that
    /// trusting it hides presets that are really there.
    fn sync_presets(&mut self, client: &Arc<dyn Vendor>, key: (String, u32)) {
        if self.presets_for.as_ref() == Some(&key) {
            return;
        }
        self.presets_for = Some(key.clone());
        self.selected_preset = None;
        self.presets.lock().unwrap().clear();

        let presets = Arc::clone(&self.presets);
        let client = Arc::clone(client);
        let channel = key.1;
        std::thread::spawn(move || match client.ptz_presets(channel) {
            Ok(found) => *presets.lock().unwrap() = found,
            Err(err) => warn!("could not load presets for channel {channel}: {err}"),
        });
    }

    /// Whether this camera has any controls at all.
    ///
    /// Snapshot and recording moved to the toolbar, so a fixed camera with no
    /// floodlight has nothing left to put here and the pane is hidden entirely
    /// rather than shown empty.
    pub fn has_controls(channel: Option<&Channel>) -> bool {
        channel
            .map(|c| c.ptz_supported || c.ptz_presets_supported || c.is_dual_lens())
            .unwrap_or(false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        client: Option<Arc<dyn Vendor>>,
        device_id: &str,
        channel: Option<&Channel>,
        label: &str,
        on_lens: &mut dyn FnMut(bool),
    ) {
        ui.label(
            RichText::new("CAMERA CONTROL")
                .size(11.0)
                .strong()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(4.0);

        let (Some(client), Some(channel)) = (client, channel) else {
            ui.label(RichText::new("No camera selected").color(theme::TEXT_DIM));
            return;
        };
        ui.label(RichText::new(label).color(theme::TEXT));

        let dual_lens = channel.is_dual_lens();
        let has_ptz = channel.ptz_supported;
        if dual_lens {
            ui.label(RichText::new("zoom switches lens").size(11.0).color(theme::TEXT_DIM));
        }
        ui.add_space(8.0);

        // A camera that cannot move gets no movement controls: a greyed-out pad
        // is just clutter, and with nothing else to show the whole pane goes.
        if has_ptz || dual_lens || channel.ptz_presets_supported {
            // Ask the camera itself rather than trusting the advertised ability:
            // firmware under-reports ptzPreset often enough to hide presets that
            // exist. Channels with no PTZ at all are skipped, since querying them
            // just returns "get config failed".
            if has_ptz || channel.ptz_presets_supported {
                self.sync_presets(&client, (device_id.to_string(), channel.index));
            }
            if has_ptz {
                self.pad(ui, &client, channel, true);
                ui.add_space(8.0);
            }
            self.zoom(ui, &client, channel, has_ptz || dual_lens, on_lens);
            ui.add_space(8.0);
            self.presets_section(ui, &client, channel, has_ptz);
        }

        if let Some(message) = self.status.lock().unwrap().clone() {
            ui.add_space(6.0);
            ui.label(RichText::new(message).size(11.0).color(theme::WARN));
        }
    }

    fn pad(&mut self, ui: &mut egui::Ui, client: &Arc<dyn Vendor>, channel: &Channel, enabled: bool) {
        ui.label(RichText::new("Pan / Tilt").size(11.0).color(theme::TEXT_DIM));
        ui.add_space(2.0);

        // Which direction, if any, the pointer is currently holding down.
        let mut held: Option<&'static str> = None;
        let button = |ui: &mut egui::Ui, glyph: &str| {
            ui.add_enabled(enabled, egui::Button::new(glyph).min_size(Vec2::new(38.0, 30.0)))
        };

        egui::Grid::new("ptz-pad").spacing(Vec2::splat(4.0)).show(ui, |ui| {
            for (index, (direction, glyph)) in DIRECTIONS.iter().enumerate() {
                if index == 4 {
                    // Centre cell: home. Stop is redundant here — releasing a
                    // direction already sends it — so the middle of the pad is
                    // better spent on the one place you always want to get back
                    // to.
                    if button(ui, "⌂")
                        .on_hover_text("Return to the home position")
                        .clicked()
                    {
                        let ch = channel.index;
                        self.report("Returning to home position");
                        self.spawn("Home", Arc::clone(client), move |c| c.ptz_go_home(ch));
                    }
                }
                if button(ui, glyph).is_pointer_button_down_on() {
                    held = Some(direction);
                }
                if index == 2 || index == 4 || index == 7 {
                    ui.end_row();
                }
            }
        });

        let ch = channel.index;
        if held != self.active_direction {
            // Releasing must send Stop, or the camera keeps travelling.
            if self.active_direction.is_some() {
                self.spawn("PTZ stop", Arc::clone(client), move |c| c.ptz_stop(ch));
            }
            if let Some(direction) = held {
                let speed = self.speed;
                self.spawn("PTZ move", Arc::clone(client), move |c| {
                    c.ptz_move(ch, direction, speed)
                });
                self.last_move_sent = Some(std::time::Instant::now());
            } else {
                self.last_move_sent = None;
            }
            self.active_direction = held;
        } else if let Some(direction) = held {
            // Still held. A single move command does not run until Stop: the
            // firmware applies its own safety timeout and halts on its own,
            // which is why holding a button produced one short nudge. Re-issue
            // it while the button is down so the movement continues, spaced far
            // enough apart not to flood the device.
            let due = self
                .last_move_sent
                .map(|at| at.elapsed() >= MOVE_REPEAT)
                .unwrap_or(true);
            if due {
                let speed = self.speed;
                self.spawn("PTZ move", Arc::clone(client), move |c| {
                    c.ptz_move(ch, direction, speed)
                });
                self.last_move_sent = Some(std::time::Instant::now());
            }
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Speed").size(11.0).color(theme::TEXT_DIM));
            ui.add_enabled(enabled, egui::Slider::new(&mut self.speed, 1..=64).show_value(true));
        });
    }

    fn zoom(
        &mut self,
        ui: &mut egui::Ui,
        client: &Arc<dyn Vendor>,
        channel: &Channel,
        enabled: bool,
        on_lens: &mut dyn FnMut(bool),
    ) {
        // A dual-lens camera has no optical zoom to drive: the two lenses *are*
        // the zoom steps, so the zoom controls swap between them instead.
        if channel.is_dual_lens() {
            ui.label(RichText::new("Lens").size(11.0).color(theme::TEXT_DIM));
            ui.add_space(2.0);
            let half = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new("− Wide").min_size(Vec2::new(half, 28.0)))
                    .on_hover_text("Switch to the wide-angle lens")
                    .clicked()
                {
                    on_lens(false);
                }
                if ui
                    .add(egui::Button::new("+ Telephoto").min_size(Vec2::new(half, 28.0)))
                    .on_hover_text("Switch to the telephoto lens")
                    .clicked()
                {
                    on_lens(true);
                }
            });
            return;
        }

        ui.label(RichText::new("Zoom").size(11.0).color(theme::TEXT_DIM));
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let half = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            for (direction, glyph) in [("zoom_out", "−"), ("zoom_in", "+")] {
                let response =
                    ui.add_enabled(enabled, egui::Button::new(glyph).min_size(Vec2::new(half, 28.0)));
                if response.clicked() {
                    let ch = channel.index;
                    let speed = self.speed;
                    let client = Arc::clone(client);
                    let status = Arc::clone(&self.status);
                    // A tap is a nudge: move briefly, then stop.
                    std::thread::spawn(move || {
                        if let Err(err) = client.ptz_move(ch, direction, speed) {
                            *status.lock().unwrap() = Some(format!("PTZ failed: {err}"));
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        let _ = client.ptz_stop(ch);
                    });
                }
            }
        });
    }

    /// Floodlight controls for the toolbar.
    ///
    /// Compact on purpose: one button that lights up with the lamp, and the
    /// brightness behind a small menu, because brightness gets set once and
    /// on/off is what actually gets used.
    pub fn floodlight_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        client: &Arc<dyn Vendor>,
        device_id: &str,
        channel: &Channel,
    ) {
        self.sync_floodlight(client, (device_id.to_string(), channel.index));

        let Some(block) = self.floodlight.lock().unwrap().clone() else {
            ui.add_enabled(false, egui::Button::new("\u{2600}"))
                .on_hover_text("Reading the floodlight\u{2026}");
            return;
        };

        let on = crate::api::models::as_i64(block.raw.get("state")) != 0;
        let stored_bright = crate::api::models::as_i64(block.raw.get("bright")).clamp(0, 100);
        // Disabled while a write is outstanding: a second click would race the
        // first, and this firmware does not settle instantly.
        let waiting = self.floodlight_pending.load(Ordering::Relaxed) > 0;

        ui.add_enabled_ui(!waiting, |ui| {
            // This row is laid out right to left, so the brightness menu is
            // added first to end up on the right of the button it belongs to.
            ui.menu_button("\u{25BE}", |ui| {
                ui.set_min_width(190.0);
                ui.label(
                    RichText::new("FLOODLIGHT")
                        .size(11.0)
                        .strong()
                        .color(theme::TEXT_DIM),
                );
                let mut bright = self.floodlight_bright.unwrap_or(stored_bright);
                let response = ui.add(
                    egui::Slider::new(&mut bright, 0..=100)
                        .text("Brightness")
                        .suffix("%"),
                );
                if response.dragged() {
                    self.floodlight_bright = Some(bright);
                } else if response.drag_stopped() || (response.changed() && !response.dragged()) {
                    self.floodlight_bright = None;
                    if bright != stored_bright {
                        self.set_floodlight(client, "bright", bright);
                    }
                }
                if let Some(message) = self.status.lock().unwrap().clone() {
                    ui.add(
                        egui::Label::new(RichText::new(message).size(11.0).color(theme::WARN))
                            .wrap(),
                    );
                }
            })
            .response
            .on_hover_text("Brightness");

            // Named rather than left as a bare symbol: the neighbouring
            // controls are words, and a lone sun beside them is a guess.
            if ui
                .selectable_label(on, "\u{2600} Light")
                .on_hover_text(if waiting {
                    "Waiting for the camera\u{2026}".to_string()
                } else if on {
                    format!("Floodlight on at {stored_bright}% \u{2014} click to turn it off")
                } else {
                    "Turn the floodlight on".to_string()
                })
                .clicked()
            {
                self.set_floodlight(client, "state", if on { 0 } else { 1 });
            }
        });

        // A write in flight has to keep the UI ticking, or the control stays
        // greyed until something else causes a repaint.
        if waiting {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(120));
        }
    }

    fn presets_section(
        &mut self,
        ui: &mut egui::Ui,
        client: &Arc<dyn Vendor>,
        channel: &Channel,
        enabled: bool,
    ) {
        ui.label(RichText::new("Presets").size(11.0).color(theme::TEXT_DIM));
        ui.add_space(2.0);
        let presets = self.presets.lock().unwrap().clone();

        // One column width for everything in this section. The Go to / Home
        // pair defines it: two halves plus the gap between them. The preset
        // dropdown and Calibrate then span exactly that, so the section reads as
        // one column rather than three different widths.
        let half = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
        let full = half * 2.0 + ui.spacing().item_spacing.x;

        if presets.is_empty() {
            ui.label(RichText::new("No presets saved").size(11.0).color(theme::PLACEHOLDER));
        } else {
            if self.selected_preset.is_none() {
                self.selected_preset = presets.first().map(|(id, _)| *id);
            }
            let current = self
                .selected_preset
                .and_then(|id| presets.iter().find(|(pid, _)| *pid == id))
                .map(|(_, name)| name.clone())
                .unwrap_or_default();

            egui::ComboBox::from_id_salt("presets")
                .selected_text(current)
                .width(full)
                .show_ui(ui, |ui| {
                    for (id, name) in &presets {
                        ui.selectable_value(&mut self.selected_preset, Some(*id), name);
                    }
                });
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let can_goto = !presets.is_empty() && self.selected_preset.is_some();
            if ui
                .add_enabled(
                    can_goto,
                    egui::Button::new("Go to").min_size(Vec2::new(half, 26.0)),
                )
                .clicked()
            {
                if let Some(id) = self.selected_preset {
                    let (ch, speed) = (channel.index, self.speed);
                    self.spawn("Preset", Arc::clone(client), move |c| {
                        c.ptz_goto_preset(ch, id, speed)
                    });
                }
            }
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new("Home").min_size(Vec2::new(half, 26.0)),
                )
                .on_hover_text("Return to the camera's guard position")
                .clicked()
            {
                let ch = channel.index;
                self.report("Returning to home position");
                self.spawn("Home", Arc::clone(client), move |c| c.ptz_go_home(ch));
            }
        });
        ui.add_space(4.0);

        // Calibration sweeps the full range and locks out other commands, so it
        // asks first rather than firing on a stray click.
        if self.confirm_calibrate {
            ui.label(
                RichText::new("Calibrate? The camera will sweep its full range.")
                    .size(11.0)
                    .color(theme::WARN),
            );
            ui.horizontal(|ui| {
                if ui.button("Yes, calibrate").clicked() {
                    self.confirm_calibrate = false;
                    let ch = channel.index;
                    self.report("Calibrating — this takes a moment");
                    self.spawn("Calibration", Arc::clone(client), move |c| c.ptz_calibrate(ch));
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_calibrate = false;
                }
            });
        } else if ui
            .add_enabled(
                enabled,
                egui::Button::new("Calibrate").min_size(Vec2::new(full, 26.0)),
            )
            .on_hover_text("Re-reference the pan/tilt position")
            .clicked()
        {
            self.confirm_calibrate = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Lens;

    fn plain() -> Channel {
        Channel::new(5)
    }

    #[test]
    fn a_fixed_camera_has_no_controls_so_the_pane_is_hidden() {
        assert!(!RailState::has_controls(Some(&plain())));
        assert!(!RailState::has_controls(None), "nothing selected shows nothing");
    }

    /// The floodlight lives in the toolbar, so having one is not on its own a
    /// reason to take 260px of the grid for a control pane.
    #[test]
    fn a_floodlight_alone_does_not_open_the_pane() {
        let mut light = plain();
        light.floodlight_supported = true;
        assert!(!RailState::has_controls(Some(&light)));
    }

    #[test]
    fn anything_the_pane_can_drive_keeps_it_open() {
        let mut ptz = plain();
        ptz.ptz_supported = true;
        assert!(RailState::has_controls(Some(&ptz)));

        // Presets without the movement ability still belong here: firmware
        // under-reports one and not the other.
        let mut presets = plain();
        presets.ptz_presets_supported = true;
        assert!(RailState::has_controls(Some(&presets)));

        // A dual-lens camera has the lens switch even with no PTZ at all.
        let mut dual = plain();
        dual.lens = Lens::Wide;
        dual.lens_partner = Some(6);
        assert!(RailState::has_controls(Some(&dual)));
    }
}
