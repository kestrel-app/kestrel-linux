//! Application preferences.
//!
//! Everything here already lived in `config.json`; this is the surface for
//! changing it without hand-editing a file. Device *settings* — encoder, image,
//! OSD and the rest that live on the camera — are a separate concern and
//! deliberately not mixed in.

use eframe::egui::{self, RichText};

use super::theme;
use crate::config::Preferences;

/// Controls line up in a single column, so a label change never shifts them.
const LABEL_WIDTH: f32 = 190.0;
const CONTROL_WIDTH: f32 = 250.0;

pub struct PreferencesDialog {
    open: bool,
    /// Edited copy: nothing is applied until Save, so Cancel is a real cancel.
    draft: Preferences,
    /// What the ZIP code resolved to, or why it did not. Held here rather than
    /// in preferences because it is about the last thing typed, not about the
    /// configuration.
    zip_note: String,
    zip_found: bool,
}

impl Default for PreferencesDialog {
    fn default() -> Self {
        PreferencesDialog {
            open: false,
            draft: Preferences::default(),
            zip_note: String::new(),
            zip_found: false,
        }
    }
}

pub enum Outcome {
    None,
    Save(Box<Preferences>),
}

impl PreferencesDialog {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, current: &Preferences) {
        self.draft = current.clone();
        // A code that was resolved before this dialog opened is still resolved;
        // saying so beats an empty line under a filled-in field.
        self.zip_found = !self.draft.weather_lat.is_empty();
        self.zip_note = if self.zip_found {
            format!(
                "{}, {}",
                self.draft.weather_lat, self.draft.weather_lon
            )
        } else {
            String::new()
        };
        self.open = true;
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        if !self.open {
            return Outcome::None;
        }
        let mut outcome = Outcome::None;
        let mut keep_open = true;

        egui::Window::new("Preferences")
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(theme::PANEL)
                    .inner_margin(16),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(560.0)
                    .show(ui, |ui| {
                        self.live_view(ui);
                        self.readiness(ui);
                        self.motion(ui);
                        self.weather(ui);
                        self.radar(ui);
                        self.files(ui);
                    });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new("Reset to defaults").frame(false))
                        .on_hover_text(
                            "Does not touch your device list, media folder, or where the \
                             weather is read from.",
                        )
                        .clicked()
                    {
                        // An address and a ZIP code are closer to a device than
                        // to a preference — somebody typed them once and would
                        // have to find them again. Only the *switch* resets.
                        self.draft = Preferences {
                            media_dir: self.draft.media_dir.clone(),
                            weather_source: self.draft.weather_source.clone(),
                            weewx_url: self.draft.weewx_url.clone(),
                            weather_zip: self.draft.weather_zip.clone(),
                            weather_lat: self.draft.weather_lat.clone(),
                            weather_lon: self.draft.weather_lon.clone(),
                            ..Preferences::default()
                        };
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new("Save").min_size(egui::vec2(88.0, 28.0)))
                            .clicked()
                        {
                            outcome = Outcome::Save(Box::new(self.draft.clone()));
                            keep_open = false;
                        }
                        if ui
                            .add(egui::Button::new("Cancel").min_size(egui::vec2(88.0, 28.0)))
                            .clicked()
                        {
                            keep_open = false;
                        }
                    });
                });
            });

        if !keep_open {
            self.open = false;
        }
        outcome
    }

    // ---------------------------------------------------------------- pieces

    /// A titled card, so the panel reads as grouped settings rather than a list.
    fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
        ui.add_space(10.0);
        ui.label(
            RichText::new(title)
                .size(11.0)
                .strong()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(4.0);
        egui::Frame::NONE
            .fill(theme::INK)
            .stroke(egui::Stroke::new(1.0_f32, theme::BORDER_SOFT))
            .corner_radius(10)
            .inner_margin(12)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                body(ui)
            });
    }

    /// One labelled row with the control in the shared column.
    fn row(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui)) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [LABEL_WIDTH, 20.0],
                egui::Label::new(RichText::new(label).color(theme::TEXT)).halign(egui::Align::LEFT),
            );
            body(ui);
        });
    }

    fn note(ui: &mut egui::Ui, text: &str) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH);
            ui.add(
                egui::Label::new(RichText::new(text).size(11.0).color(theme::PLACEHOLDER))
                    .wrap(),
            );
        });
    }

    // ---------------------------------------------------------------- sections

    fn live_view(&mut self, ui: &mut egui::Ui) {
        let draft = &mut self.draft;
        Self::section(ui, "LIVE VIEW", |ui| {
            Self::row(ui, "Grid stream", |ui| {
                let mut sub = draft.live_substream;
                egui::ComboBox::from_id_salt("pref-grid-stream")
                    .selected_text(if sub { "Sub stream (lighter)" } else { "Main stream (sharper)" })
                    .width(CONTROL_WIDTH)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut sub, true, "Sub stream (lighter)");
                        ui.selectable_value(&mut sub, false, "Main stream (sharper)");
                    });
                draft.live_substream = sub;
            });

            Self::row(ui, "Expanded camera", |ui| {
                let mut best = draft.expanded_stream != "sub";
                egui::ComboBox::from_id_salt("pref-expanded")
                    .selected_text(if best { "Best quality" } else { "Lower bandwidth" })
                    .width(CONTROL_WIDTH)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut best, true, "Best quality");
                        ui.selectable_value(&mut best, false, "Lower bandwidth");
                    });
                draft.expanded_stream = if best { "main".into() } else { "sub".into() };
            });

            Self::row(ui, "Virtual cameras", |ui| {
                ui.checkbox(
                    &mut draft.virtual_cameras_use_main_stream,
                    "Use the main stream",
                );
            });
            Self::note(
                ui,
                if draft.virtual_cameras_use_main_stream {
                    "A virtual camera exists to magnify, and there is nothing in a sub stream \
                     to magnify: a 4x crop of 640x360 is 160x90 filling a cell. A camera with \
                     one of its crops on the wall therefore pulls its main stream, and the \
                     camera and every crop of it share that one decode."
                } else {
                    "Virtual cameras take whatever stream the wall is on, which keeps a large \
                     wall light at the price of a soft picture wherever one is zoomed in far."
                },
            );

            Self::row(ui, "Picture in each cell", |ui| {
                use crate::config::PictureFill;
                let current = draft.picture_fill();
                egui::ComboBox::from_id_salt("pref-picture-fill")
                    .selected_text(current.label())
                    .width(CONTROL_WIDTH)
                    .show_ui(ui, |ui| {
                        for mode in PictureFill::ALL {
                            if ui.selectable_label(current == mode, mode.label()).clicked() {
                                draft.picture_fill = mode.key().to_string();
                            }
                        }
                    });
            });
            Self::note(
                ui,
                match draft.picture_fill() {
                    crate::config::PictureFill::Stretch => {
                        "A wall is tiled square-ish and cameras are not: three columns and two \
                         rows of a widescreen display give each camera a cell narrower than its \
                         picture. Stretching keeps the whole frame and uses every pixel of the \
                         cell, at the price of the shape."
                    }
                    crate::config::PictureFill::Fit => {
                        "The picture keeps its shape, so what is on screen is the shape the \
                         camera sees — with black bars above and below, or either side, \
                         wherever the cell does not match."
                    }
                    crate::config::PictureFill::Fill => {
                        "The shape is kept and the picture is scaled up until it covers the \
                         cell, so the edges of the frame are cropped away. Worth knowing on a \
                         camera aimed along a driveway, where the edges are the point."
                    }
                },
            );

            Self::row(ui, "Offline channels", |ui| {
                ui.checkbox(&mut draft.show_offline_channels, "Show them in the grid");
            });
            Self::note(
                ui,
                "Unpopulated NVR slots report as offline channels, so they are hidden \
                 by default rather than taking a tile.",
            );

            Self::row(ui, "Camera list", |ui| {
                ui.checkbox(&mut draft.sidebar_open, "Show it at startup");
            });
            Self::note(
                ui,
                "Only where it starts. The list folds away and comes back from the \u{25e7} button \
                 in the top bar, or with Ctrl+B, and that is a mode rather than a setting \u{2014} \
                 the same distinction follow motion draws.",
            );

            Self::row(ui, "Top bar", |ui| {
                ui.checkbox(&mut draft.auto_hide_header, "Hide it until the mouse moves");
            });
            Self::note(
                ui,
                "Off by default, because a toolbar that vanishes is startling if you did not ask \
                 for it. On, it goes the same way the camera names do and off the same timer, so \
                 a wall settling down clears itself in one movement rather than two. Ctrl+B still \
                 reaches the camera list while it is away.",
            );

            Self::row(ui, "Camera names", |ui| {
                ui.checkbox(&mut draft.auto_hide_titles, "Hide until the mouse moves");
            });
            if draft.auto_hide_titles {
                Self::note(
                    ui,
                    "While hidden the picture uses the whole tile; names fade back in on any \
                     mouse movement, and hide at once when the pointer leaves the window. \
                     Detection badges stay visible either way.",
                );
                Self::row(ui, "Hide after", |ui| {
                    ui.add_sized(
                        [CONTROL_WIDTH, 20.0],
                        egui::Slider::new(&mut draft.title_hide_seconds, 0.5..=10.0).suffix(" s"),
                    );
                });
                Self::row(ui, "Mouse pointer", |ui| {
                    ui.checkbox(&mut draft.hide_pointer_when_idle, "Hide it as well");
                });
                Self::note(
                    ui,
                    "Only while it rests over the pictures — over the toolbar or the camera \
                     list it stays put, where a vanishing pointer would just be lost.",
                );
            } else {
                Self::note(
                    ui,
                    "The name keeps a strip above each picture, so it never covers the feed.",
                );
            }

            Self::row(ui, "Fullscreen", |ui| {
                ui.checkbox(&mut draft.keep_awake_fullscreen, "Keep the screen awake");
            });
            Self::note(
                ui,
                "A wall is something you watch without touching, which is exactly what a \
                 screen blanker treats as an idle machine. Only while fullscreen — leaving \
                 it lets the screen sleep again straight away.",
            );

            Self::row(ui, "Stream statistics", |ui| {
                ui.checkbox(&mut draft.show_stream_stats, "Show frame rate and bitrate");
            });
            Self::note(
                ui,
                "Printed beside each camera's name. Useful when a picture looks wrong; \
                 clutter the rest of the time.",
            );
        });
    }

    fn readiness(&mut self, ui: &mut egui::Ui) {
        let draft = &mut self.draft;
        Self::section(ui, "KEEPING CAMERAS READY", |ui| {
            Self::row(ui, "Off-screen cameras", |ui| {
                ui.checkbox(&mut draft.warm_streams, "Stay connected");
            });
            Self::note(
                ui,
                "They keep receiving but stop decoding, so showing one takes 0.07s \
                 instead of about 6s, for roughly 0.8% of a CPU core each.",
            );

            let enabled = draft.warm_streams;
            ui.add_enabled_ui(enabled, |ui| {
                Self::row(ui, "Maximum kept ready", |ui| {
                    ui.add_sized(
                        [CONTROL_WIDTH, 20.0],
                        egui::Slider::new(&mut draft.max_warm_streams, 0..=32),
                    );
                });
            });
        });
    }

    fn motion(&mut self, ui: &mut egui::Ui) {
        let draft = &mut self.draft;
        Self::section(ui, "MOTION", |ui| {
            // Following is switched on from the header or the ⋯ menu; these are
            // how it behaves when you do, editable whether or not it is running.
            {
                Self::row(ui, "Follow motion reacts to", |ui| {
                    for kind in crate::api::EventKind::ALL {
                        let key = kind.device_key().to_string();
                        let mut on = draft.follow_kinds.contains(&key);
                        if ui.checkbox(&mut on, kind.label()).changed() {
                            if on {
                                draft.follow_kinds.push(key);
                            } else {
                                draft.follow_kinds.retain(|k| k != &key);
                            }
                        }
                    }
                });
                if draft.follow_kinds.is_empty() {
                    Self::note(ui, "Nothing selected — the view will never follow.");
                } else {
                    Self::note(
                        ui,
                        "Everything still reaches the event feed and notifications; this \
                         only decides what steers the live view.",
                    );
                }

                Self::row(ui, "Hold each camera for", |ui| {
                    ui.add_sized(
                        [CONTROL_WIDTH, 20.0],
                        egui::Slider::new(&mut draft.follow_dwell_seconds, 2.0..=120.0)
                            .suffix(" s"),
                    );
                });
                // The exclusion is per camera, so it is not set here — this is
                // where somebody looks for it, though, and a camera left out of
                // following is invisible until you know it can be done.
                Self::note(
                    ui,
                    "Individual cameras can be left out — right-click one on the wall or in \
                     the sidebar. A drive that catches every car on the road is worth having \
                     up and not worth being pulled to.",
                );
            }

            Self::row(ui, "Check for detections", |ui| {
                ui.add_sized(
                    [CONTROL_WIDTH, 20.0],
                    egui::Slider::new(&mut draft.event_poll_seconds, 0.5..=10.0).suffix(" s"),
                );
            });
            Self::note(
                ui,
                "Detections are polled — the API has no push channel — so anything shorter \
                 than this can be missed. Applies on restart.",
            );

            Self::row(ui, "Notifications", |ui| {
                ui.checkbox(&mut draft.desktop_notifications, "Show desktop alerts");
            });

            let notifying = draft.desktop_notifications;
            ui.add_enabled_ui(notifying, |ui| {
                Self::row(ui, "Alert me about", |ui| {
                    for kind in crate::api::EventKind::ALL {
                        let key = kind.device_key().to_string();
                        let mut on = draft.notify_kinds.contains(&key);
                        if ui.checkbox(&mut on, kind.label()).changed() {
                            if on {
                                draft.notify_kinds.push(key);
                            } else {
                                draft.notify_kinds.retain(|k| k != &key);
                            }
                        }
                    }
                });
            });
            if notifying && draft.notify_kinds.is_empty() {
                Self::note(ui, "Nothing selected — no alerts will be sent.");
            } else {
                Self::note(
                    ui,
                    "Everything still reaches the event feed either way; this only decides \
                     what is worth interrupting you for.",
                );
            }
        });
    }

    // ---------------------------------------------------------------- weather

    fn weather(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "WEATHER", |ui| {
            ui.checkbox(
                &mut self.draft.weather_enabled,
                "Show the weather alongside the cameras",
            );
            Self::note(
                ui,
                "A band above the grid and a Weather tab, from your own station or from \
                 the National Weather Service. This is the only part of Kestrel that \
                 reaches outside your network.",
            );

            let on = self.draft.weather_enabled;
            ui.add_enabled_ui(on, |ui| {
                ui.add_space(6.0);
                Self::row(ui, "Read it from", |ui| {
                    let mut weewx = self.draft.weather_source == "weewx";
                    egui::ComboBox::from_id_salt("pref-weather-source")
                        .selected_text(if weewx {
                            "A weewx server on your network"
                        } else {
                            "The National Weather Service"
                        })
                        .width(CONTROL_WIDTH)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut weewx, false, "The National Weather Service");
                            ui.selectable_value(&mut weewx, true, "A weewx server on your network");
                        });
                    // Through the source's own key rather than a literal, so
                    // the spelling that gets saved is the one that gets read.
                    self.draft.weather_source = if weewx {
                        crate::weather::poller::Source::Weewx
                    } else {
                        crate::weather::poller::Source::Nws
                    }
                    .key()
                    .to_string();
                });

                if self.draft.weather_source() == crate::weather::poller::Source::Weewx {
                    self.weewx_rows(ui);
                }

                // The location is asked for whatever the source is.
                //
                // It belongs to *where you are* rather than to the source: the
                // service is addressed by it, and so is the radar — which is a
                // different service on a different host, and somebody reading
                // their own station is as likely to want to see where the rain
                // is. Tucking this inside the weather.gov branch meant picking
                // weewx removed the only way to set it, so the radar could be
                // switched on and then never had anywhere to point.
                self.location_rows(ui);

                // Asked whatever the source is. It used to sit inside the
                // weather.gov branch, on the reasoning that a weewx server
                // reports in whatever units it is set to and there is nothing
                // to choose — true of the *readings*, and not of everything
                // else. The radar is a second service with distances of its
                // own, and somebody reading their own station still has to be
                // able to say whether those are miles.
                self.units_row(ui);

                Self::row(ui, "Check every", |ui| {
                    let mut minutes = self.draft.weather_poll_seconds / 60.0;
                    if ui
                        .add_sized(
                            [CONTROL_WIDTH, 20.0],
                            egui::Slider::new(&mut minutes, 1.0..=30.0)
                                .step_by(1.0)
                                .suffix(" min"),
                        )
                        .changed()
                    {
                        self.draft.weather_poll_seconds = minutes * 60.0;
                    }
                });
                Self::note(
                    ui,
                    "A weewx archive record is minutes apart and a weather.gov observation \
                     is hourly, so there is nothing to gain from asking more often. A \
                     failed check is retried sooner than this.",
                );

                Self::row(ui, "Clock", |ui| {
                    let mut choice = self.draft.clock_24_hour;
                    egui::ComboBox::from_id_salt("pref-clock")
                        .selected_text(match choice {
                            None => "Follow the system",
                            Some(false) => "12-hour",
                            Some(true) => "24-hour",
                        })
                        .width(CONTROL_WIDTH)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut choice, None, "Follow the system");
                            ui.selectable_value(&mut choice, Some(false), "12-hour");
                            ui.selectable_value(&mut choice, Some(true), "24-hour");
                        });
                    self.draft.clock_24_hour = choice;
                });

                Self::row(ui, "Forecast on the wall", |ui| {
                    use crate::config::ForecastTiles;
                    let current = self.draft.forecast_tiles.clone();
                    let label = match current.as_str() {
                        "always" => "Always take cells",
                        "never" => "Not on the wall",
                        _ => "Only spare cells",
                    };
                    egui::ComboBox::from_id_salt("pref-forecast-tiles")
                        .selected_text(label)
                        .width(CONTROL_WIDTH)
                        .show_ui(ui, |ui| {
                            for (mode, name) in [
                                (ForecastTiles::Never, "Not on the wall"),
                                (ForecastTiles::Spare, "Only spare cells"),
                                (ForecastTiles::Always, "Always take cells"),
                            ] {
                                if ui.selectable_label(current == mode.key(), name).clicked() {
                                    self.draft.forecast_tiles = mode.key().to_string();
                                }
                            }
                        });
                });
                if self.draft.forecast_tiles == "always" {
                    Self::row(ui, "How many periods", |ui| {
                        let mut periods = self.draft.forecast_periods as f32;
                        if ui
                            .add_sized(
                                [CONTROL_WIDTH, 20.0],
                                egui::Slider::new(&mut periods, 1.0..=6.0).step_by(1.0),
                            )
                            .changed()
                        {
                            self.draft.forecast_periods = periods.round() as u32;
                        }
                    });
                }
                Self::note(
                    ui,
                    match self.draft.forecast_tiles.as_str() {
                        "always" => {
                            "Cells of its own, counted before the wall picks its shape — so the \
                             forecast is there whatever the camera count. No camera is dropped, \
                             but they are all a little smaller. Any cells the cameras leave \
                             over on top of these get a period too."
                        }
                        "never" => {
                            "The strip above the grid and the Weather tab, and nothing on the \
                             wall itself."
                        }
                        _ => {
                            "Only the cells the cameras and the radar left over, so no camera \
                             ever changes size for it — five cameras tile as a 3x2 and the \
                             sixth cell is tonight. A wall of 4, 9 or 16 cameras tiles exactly \
                             and shows none."
                        }
                    },
                );

                Self::row(ui, "Strip above the grid", |ui| {
                    ui.checkbox(&mut self.draft.weather_bar, "Show it");
                });
                if self.draft.weather_bar {
                    Self::row(ui, "Strip height", |ui| {
                        ui.add_sized(
                            [CONTROL_WIDTH, 20.0],
                            egui::Slider::new(&mut self.draft.weather_bar_height, 80.0..=220.0)
                                .suffix(" px"),
                        );
                    });
                    Self::note(
                        ui,
                        "It takes this from the cameras. The Weather tab has the full \
                         reading either way.",
                    );
                }
            });
        });
    }

    /// Where you are, which both the service and the radar are addressed by.
    fn location_rows(&mut self, ui: &mut egui::Ui) {
        let for_radar_only = self.draft.weather_source() == crate::weather::poller::Source::Weewx;

        Self::row(ui, "ZIP code", |ui| {
            let response = ui.add_sized(
                [CONTROL_WIDTH, 20.0],
                egui::TextEdit::singleline(&mut self.draft.weather_zip).hint_text("01001"),
            );
            if response.changed() {
                self.resolve_zip();
            }
        });

        // Looked up as it is typed rather than behind a button: the table is
        // carried in the binary, so this is a binary search over memory and
        // there is nothing to wait for.
        if !self.zip_note.is_empty() {
            let colour = if self.zip_found {
                theme::TEXT_DIM
            } else {
                theme::WARN
            };
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(LABEL_WIDTH);
                ui.add(egui::Label::new(RichText::new(&self.zip_note).size(11.0).color(colour)).wrap());
            });
        }

        Self::note(
            ui,
            if for_radar_only {
                "Your station provides the readings, so this is only needed for the radar — \
                 which is a different service, and has to be told where to point. Leave it \
                 empty if you do not want the radar."
            } else {
                "weather.gov is addressed by coordinate, so the code is looked up here and the \
                 coordinate is what gets saved. The table ships with Kestrel — nothing is asked \
                 of anyone to do it. The radar uses the same location."
            },
        );
    }

    /// Degrees, speeds and distances, in one answer.
    ///
    /// US customary by default, which is where the National Weather Service,
    /// the radar and nearly everyone this is pointed at already are.
    fn units_row(&mut self, ui: &mut egui::Ui) {
        const US: &str = "US customary (°F, mph, miles)";
        const SI: &str = "Metric (°C, km/h, kilometres)";

        Self::row(ui, "Units", |ui| {
            let mut metric = self.draft.weather_metric;
            egui::ComboBox::from_id_salt("pref-weather-units")
                .selected_text(if metric { SI } else { US })
                .width(CONTROL_WIDTH)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut metric, false, US);
                    ui.selectable_value(&mut metric, true, SI);
                });
            self.draft.weather_metric = metric;
        });
        Self::note(
            ui,
            if self.draft.weather_source() == crate::weather::poller::Source::Weewx {
                "Your station reports in whatever units it is configured for and those are \
                 shown as they come, so this sets the radar's distances."
            } else {
                "The readings, the forecast and how far the radar reaches."
            },
        );
    }

    fn weewx_rows(&mut self, ui: &mut egui::Ui) {
        Self::row(ui, "Address", |ui| {
            ui.add_sized(
                [CONTROL_WIDTH, 20.0],
                egui::TextEdit::singleline(&mut self.draft.weewx_url)
                    .hint_text("http://weewx.local/weather.json"),
            );
        });
        Self::note(
            ui,
            "The JSON document weewx publishes for Home Assistant. Whatever units the \
             station is configured for are the ones shown, so there is nothing to choose.",
        );

        Self::row(ui, "Certificate", |ui| {
            ui.checkbox(
                &mut self.draft.weewx_allow_self_signed,
                "Trust it even if unverified",
            );
        });
        Self::note(
            ui,
            "The same allowance a device gets, and for the same reason: a server on a home \
             network is regularly behind a private CA. It buys encryption without \
             authentication.",
        );
    }

    /// Turn what has been typed into a coordinate, or say why not.
    fn resolve_zip(&mut self) {
        let typed = self.draft.weather_zip.trim().to_string();
        if typed.is_empty() {
            self.draft.weather_lat.clear();
            self.draft.weather_lon.clear();
            self.zip_note.clear();
            self.zip_found = false;
            return;
        }

        match crate::weather::zip::lookup(&typed) {
            Ok(found) => {
                self.draft.weather_zip = found.zip;
                self.draft.weather_lat = found.lat.clone();
                self.draft.weather_lon = found.lon.clone();
                self.zip_note = format!("{}, {}", found.lat, found.lon);
                self.zip_found = true;
            }
            Err(why) => {
                // Cleared, not left pointing at the last code that worked — a
                // half-typed code must not silently keep polling the old one.
                self.draft.weather_lat.clear();
                self.draft.weather_lon.clear();
                self.zip_note = why;
                self.zip_found = false;
            }
        }
    }

    fn radar(&mut self, ui: &mut egui::Ui) {
        let draft = &mut self.draft;
        let locatable = !draft.weather_lat.is_empty();

        Self::section(ui, "RADAR", |ui| {
            ui.add_enabled_ui(draft.weather_enabled, |ui| {
                ui.checkbox(&mut draft.radar_enabled, "Show the National Weather Service radar");
            });
            Self::note(
                ui,
                "The enhanced radar — the seamless national mosaic, over a map — inside the \
                 Weather tab. It is only fetched while you are looking at it.",
            );

            if draft.radar_enabled && !locatable {
                Self::note(
                    ui,
                    "Set a ZIP code above first. The radar is addressed by coordinate even \
                     when the readings come from your own station, which has thermometers \
                     and no idea where it is.",
                );
            }

            ui.add_enabled_ui(draft.radar_enabled && draft.weather_enabled, |ui| {
                Self::row(ui, "On the camera wall", |ui| {
                    use crate::config::RadarTile;
                    let current = draft.radar_tile.clone();
                    let label = match current.as_str() {
                        "always" => "Always take a cell",
                        "spare" => "Only a spare cell",
                        _ => "Not on the wall",
                    };
                    egui::ComboBox::from_id_salt("pref-radar-tile")
                        .selected_text(label)
                        .width(CONTROL_WIDTH)
                        .show_ui(ui, |ui| {
                            for (mode, name) in [
                                (RadarTile::Never, "Not on the wall"),
                                (RadarTile::Spare, "Only a spare cell"),
                                (RadarTile::Always, "Always take a cell"),
                            ] {
                                if ui.selectable_label(current == mode.key(), name).clicked() {
                                    draft.radar_tile = mode.key().to_string();
                                }
                            }
                        });
                });
                Self::note(
                    ui,
                    match draft.radar_tile.as_str() {
                        "always" => {
                            "An item on the wall in its own right. Four cameras and the radar lay \
                             out as six cells rather than four: no camera is dropped, but they \
                             are all a little smaller."
                        }
                        "spare" => {
                            "Only a cell the cameras left over, and nothing when they tile \
                             exactly — so no camera ever changes size for it. A wall of 4, 9 or \
                             16 cameras will show none."
                        }
                        _ => "The Weather tab only.",
                    },
                );

                Self::row(ui, "View covers", |ui| {
                    // Kept in kilometres whichever unit is shown — the grid the
                    // map is built on is metric, so this converts the dial
                    // rather than the setting, and changing units re-labels
                    // what is stored instead of altering it.
                    let metric = draft.weather_metric;
                    const MILE: f32 = crate::weather::KM_PER_MILE as f32;
                    // The range the map can actually show, rather than a
                    // narrower one: it stopped at 80km while the wheel would
                    // take you to forty, so the setting could not be asked for
                    // a view plainly reachable by hand.
                    let (mut span, range, step, suffix) = if metric {
                        (draft.radar_span_km as f32, 30.0..=1200.0, 10.0, " km")
                    } else {
                        (
                            (draft.radar_span_km as f32 / MILE).round(),
                            20.0..=750.0,
                            5.0,
                            " mi",
                        )
                    };
                    if ui
                        .add_sized(
                            [CONTROL_WIDTH, 20.0],
                            egui::Slider::new(&mut span, range)
                                .step_by(step)
                                .logarithmic(true)
                                .suffix(suffix),
                        )
                        .changed()
                    {
                        draft.radar_span_km = if metric {
                            span.round() as u32
                        } else {
                            (span * MILE).round() as u32
                        };
                    }
                });
                Self::note(
                    ui,
                    "Top to bottom, and where the view starts — drag the radar itself to look \
                     somewhere else, scroll to zoom, and Reset view brings it back here.",
                );

                Self::row(ui, "Map underneath", |ui| {
                    let current = draft.radar_basemap.clone();
                    let label = match current.as_str() {
                        "topo" => "Terrain",
                        "dark" => "Dark canvas",
                        _ => "Streets",
                    };
                    egui::ComboBox::from_id_salt("pref-radar-map")
                        .selected_text(label)
                        .width(CONTROL_WIDTH)
                        .show_ui(ui, |ui| {
                            for (key, name) in
                                [("street", "Streets"), ("topo", "Terrain"), ("dark", "Dark canvas")]
                            {
                                if ui.selectable_label(current == key, name).clicked() {
                                    draft.radar_basemap = key.to_string();
                                }
                            }
                        });
                });
                Self::note(
                    ui,
                    "The dark canvas is the one that keeps its place names readable through \
                     heavy rain — its lettering is drawn over the weather rather than under it.",
                );
            });
        });
    }

    fn files(&mut self, ui: &mut egui::Ui) {
        let draft = &mut self.draft;
        Self::section(ui, "FILES", |ui| {
            Self::row(ui, "Media folder", |ui| {
                ui.add_sized(
                    [CONTROL_WIDTH, 20.0],
                    egui::TextEdit::singleline(&mut draft.media_dir),
                );
            });
            Self::note(ui, "Snapshots, recordings and downloads are written here.");
        });
    }
}
