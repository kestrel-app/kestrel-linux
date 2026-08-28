//! Add / edit / remove a camera or NVR.
//!
//! Without this the app can only show devices someone put in the config file by
//! hand, so it is the difference between a viewer and something a new user can
//! actually set up.

use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};

use super::theme;
use crate::api::vendor::Vendor;
use crate::config::DeviceConfig;

/// What the dialog wants the app to do once it closes.
pub enum Outcome {
    None,
    Save(DeviceConfig),
    Remove(String),
}

type TestResult = Result<String, String>;

#[derive(Default)]
pub struct DeviceDialog {
    open: bool,
    /// Set when editing an existing device rather than adding one.
    editing: Option<String>,
    form: DeviceConfig,
    test: Arc<Mutex<Option<TestResult>>>,
    testing: bool,
    /// What an address said it was running, once asked.
    identified: Arc<Mutex<Option<Option<crate::api::vendor::Detected>>>>,
    identifying: bool,
    confirm_remove: bool,
}

impl DeviceDialog {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn add(&mut self) {
        self.reset();
        self.open = true;
        self.editing = None;
        self.form = DeviceConfig::default();
    }

    pub fn edit(&mut self, device: &DeviceConfig) {
        self.reset();
        self.open = true;
        self.editing = Some(device.id.clone());
        self.form = device.clone();
    }

    fn reset(&mut self) {
        *self.test.lock().unwrap() = None;
        self.testing = false;
        self.identifying = false;
        *self.identified.lock().unwrap() = None;
        self.confirm_remove = false;
    }

    /// Try the entered details against the real device before saving them.
    ///
    /// Reports the model and channel list, so a typo in the address or password
    /// is caught here rather than showing up later as an empty grid.
    /// Ask the address what it is, and set the form from the answer.
    ///
    /// Runs off the UI thread: five probes with short timeouts is still up to a
    /// few seconds against a host that is simply not there.
    fn start_identify(&mut self) {
        let host = self.form.host.trim().to_string();
        if host.is_empty() {
            return;
        }
        self.identifying = true;
        *self.identified.lock().unwrap() = None;

        let slot = Arc::clone(&self.identified);
        let (port, https) = (self.form.port, self.form.https);
        let relaxed = self.form.allow_self_signed;
        std::thread::spawn(move || {
            let found = crate::api::vendor::detect(&host, port, https, relaxed);
            *slot.lock().unwrap() = Some(found);
        });
    }

    /// Fold a finished identification into the form.
    fn apply_identification(&mut self) {
        let found = self.identified.lock().unwrap().take();
        let Some(found) = found else { return };
        self.identifying = false;

        match found {
            Some(detected) => {
                self.form.vendor = detected.vendor.to_string();
                self.form.port = detected.port;
                self.form.https = detected.https;
                self.form.apply_vendor_defaults();
                *self.test.lock().unwrap() =
                    Some(Ok(format!("Looks like {}.", detected.detail)));
            }
            None => {
                // Not an error: the address may be right and the system simply
                // quiet until it has credentials.
                *self.test.lock().unwrap() = Some(Err(
                    "Could not tell what this is — pick the system yourself.".into(),
                ));
            }
        }
    }

    fn start_test(&mut self) {
        if self.form.host.trim().is_empty() {
            *self.test.lock().unwrap() = Some(Err("Enter an address first.".into()));
            return;
        }
        self.testing = true;
        *self.test.lock().unwrap() = None;

        let form = self.form.clone();
        let slot = Arc::clone(&self.test);
        std::thread::spawn(move || {
            // Test with the same module that will actually run the device.
            let mut client = crate::api::vendor::build(&form);
            let outcome = match client.connect() {
                Ok(info) => {
                    let online = client.channels().iter().filter(|c| c.online).count();
                    let names: Vec<String> = client
                        .channels()
                        .iter()
                        .filter(|c| c.online)
                        .take(4)
                        .map(|c| c.display_name())
                        .collect();
                    let more = if online > names.len() { ", …" } else { "" };
                    Ok(format!(
                        "Connected: {} ({:?}), firmware {}\n{online} channel(s): {}{more}",
                        info.model,
                        info.kind(),
                        info.firmware,
                        names.join(", ")
                    ))
                }
                Err(err) => Err(err.to_string()),
            };
            client.logout();
            *slot.lock().unwrap() = Some(outcome);
        });
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        if !self.open {
            return Outcome::None;
        }
        let mut outcome = Outcome::None;
        let mut keep_open = true;
        // A finished probe arrives on another thread; fold it in before drawing
        // so the picker and ports show what it found.
        self.apply_identification();
        if self.identifying {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }

        let title = if self.editing.is_some() {
            "Edit device"
        } else {
            "Add camera or NVR"
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .default_width(430.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("device-form")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        // First, because it decides what the rest means: the
                        // ports, and whether a username is wanted at all.
                        ui.label("System");
                        egui::ComboBox::from_id_salt("device-vendor")
                            .selected_text(crate::api::vendor::label_for(&self.form.vendor))
                            .width(280.0)
                            .show_ui(ui, |ui| {
                                for vendor in crate::api::vendor::VENDORS {
                                    if ui
                                        .selectable_label(
                                            self.form.vendor == vendor.id,
                                            format!("{}  —  {}", vendor.label, vendor.detail),
                                        )
                                        .clicked()
                                    {
                                        self.form.vendor = vendor.id.to_string();
                                        self.form.apply_vendor_defaults();
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label("Address");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.host)
                                .hint_text("192.0.2.50 or hostname")
                                .desired_width(280.0),
                        );
                        ui.end_row();

                        ui.label("Label");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.label)
                                .hint_text("optional — defaults to the device's own name")
                                .desired_width(280.0),
                        );
                        ui.end_row();

                        ui.label("Username");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.username)
                                .desired_width(280.0),
                        );
                        ui.end_row();

                        ui.label("Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.password)
                                .password(true)
                                .desired_width(280.0),
                        );
                        ui.end_row();

                        ui.label("Ports");
                        ui.horizontal(|ui| {
                            ui.label("HTTP");
                            ui.add(egui::DragValue::new(&mut self.form.port).range(1..=65535));
                            ui.label("RTSP");
                            ui.add(
                                egui::DragValue::new(&mut self.form.rtsp_port).range(1..=65535),
                            );
                            // Reolink's own web UI moves to 443 when HTTPS is on.
                            if ui.checkbox(&mut self.form.https, "HTTPS").changed() {
                                self.form.port = match (self.form.https, self.form.port) {
                                    (true, 80) => 443,
                                    (false, 443) => 80,
                                    (_, port) => port,
                                };
                            }
                        });
                        ui.end_row();

                        // Only meaningful over HTTPS, and off unless asked for.
                        if self.form.https {
                            ui.label("Certificate");
                            ui.vertical(|ui| {
                                ui.checkbox(
                                    &mut self.form.allow_self_signed,
                                    "Trust this device's own certificate",
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(
                                            "Cameras and NVRs ship self-signed certificates, which \
                                             cannot be verified. Trusting one encrypts the \
                                             connection but does not prove what is at the other \
                                             end — still better than HTTP, which sends the \
                                             password in the clear.",
                                        )
                                        .size(11.0)
                                        .color(theme::PLACEHOLDER),
                                    )
                                    .wrap(),
                                );
                            });
                            ui.end_row();
                        }
                    });

                ui.add_space(6.0);
                match self.test.lock().unwrap().clone() {
                    Some(Ok(message)) => {
                        self.testing = false;
                        ui.label(RichText::new(message).size(11.0).color(theme::OK));
                    }
                    Some(Err(message)) => {
                        self.testing = false;
                        ui.label(
                            RichText::new(format!("Failed: {message}"))
                                .size(11.0)
                                .color(theme::ERROR),
                        );
                    }
                    None if self.identifying => {
                        ui.label(
                            RichText::new("Asking the address what it is…")
                                .size(11.0)
                                .color(theme::TEXT_DIM),
                        );
                    }
                    None if self.testing => {
                        ui.label(RichText::new("Connecting…").size(11.0).color(theme::TEXT_DIM));
                    }
                    None => {}
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Test connection").clicked() {
                        self.start_test();
                    }
                    // Asking the address what it is beats making someone read
                    // the picker and guess.
                    let can_identify = !self.form.host.trim().is_empty() && !self.identifying;
                    if ui
                        .add_enabled(can_identify, egui::Button::new("Identify this system"))
                        .on_hover_text("Ask the address what it is running")
                        .clicked()
                    {
                        self.start_identify();
                    }

                    let can_save = !self.form.host.trim().is_empty();
                    if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                        let mut device = self.form.clone();
                        device.host = device.host.trim().to_string();
                        if device.username.trim().is_empty() {
                            device.username = "admin".into();
                        }
                        outcome = Outcome::Save(device);
                        keep_open = false;
                    }

                    if ui.button("Cancel").clicked() {
                        keep_open = false;
                    }

                    if let Some(id) = self.editing.clone() {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.confirm_remove {
                                if ui
                                    .button(RichText::new("Really remove").color(theme::ERROR))
                                    .clicked()
                                {
                                    outcome = Outcome::Remove(id);
                                    keep_open = false;
                                }
                            } else if ui.button("Remove").clicked() {
                                self.confirm_remove = true;
                            }
                        });
                    }
                });

                if self.confirm_remove {
                    ui.label(
                        RichText::new(
                            "Recordings and snapshots already saved to disk are kept.",
                        )
                        .size(11.0)
                        .color(theme::TEXT_DIM),
                    );
                }
            });

        if !keep_open {
            self.open = false;
            self.reset();
        }
        outcome
    }
}
