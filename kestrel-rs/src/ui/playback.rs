//! The Playback tab: browse what the device has recorded, and play it back.

use std::sync::{Arc, Mutex};

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime};
use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, Vec2};
use log::warn;

use super::theme;
use crate::api::{Recording, StreamType};
use crate::manager::DeviceManager;
use crate::video::PlaybackWorker;

type Key = (String, u32);

#[derive(Default)]
struct Search {
    clips: Arc<Mutex<Vec<Recording>>>,
    days: Arc<Mutex<Vec<u32>>>,
    busy: Arc<Mutex<bool>>,
    error: Arc<Mutex<Option<String>>>,
}

pub struct PlaybackView {
    channel: Option<Key>,
    month: NaiveDate,
    day: NaiveDate,
    search: Search,
    /// What the last search was for, so it only re-runs when something changes.
    searched: Option<(Key, NaiveDate)>,
    months_loaded: Option<(Key, u32, u32)>,

    player: Option<PlaybackWorker>,
    playing: Option<Recording>,
    texture: Option<TextureHandle>,
    last_sequence: u64,
    speed: f32,
    scrub: Option<f64>,
}

impl Default for PlaybackView {
    fn default() -> Self {
        let today = Local::now().date_naive();
        PlaybackView {
            channel: None,
            month: today,
            day: today,
            search: Search::default(),
            searched: None,
            months_loaded: None,
            player: None,
            playing: None,
            texture: None,
            last_sequence: 0,
            speed: 1.0,
            scrub: None,
        }
    }
}

impl PlaybackView {
    /// Stop any playback, e.g. when leaving the tab.
    pub fn release(&mut self) {
        self.player = None;
        self.playing = None;
        self.texture = None;
    }

    fn start_search(&mut self, manager: &DeviceManager, key: Key, day: NaiveDate) {
        let Some(client) = manager.client(&key.0) else { return };
        self.searched = Some((key.clone(), day));
        self.search.clips.lock().unwrap().clear();
        *self.search.error.lock().unwrap() = None;
        *self.search.busy.lock().unwrap() = true;

        let (clips, busy, error) = (
            Arc::clone(&self.search.clips),
            Arc::clone(&self.search.busy),
            Arc::clone(&self.search.error),
        );
        let channel = key.1;
        std::thread::spawn(move || {
            let start = day.and_hms_opt(0, 0, 0).unwrap();
            let end = day.and_hms_opt(23, 59, 59).unwrap();
            match client.search_recordings(channel, start, end, StreamType::Main) {
                Ok(found) => *clips.lock().unwrap() = found,
                Err(err) => *error.lock().unwrap() = Some(err.to_string()),
            }
            *busy.lock().unwrap() = false;
        });
    }

    /// Ask which days in the shown month hold footage, to mark the calendar.
    fn load_month(&mut self, manager: &DeviceManager, key: Key, month: NaiveDate) {
        let Some(client) = manager.client(&key.0) else { return };
        self.months_loaded = Some((key.clone(), month.year() as u32, month.month()));
        self.search.days.lock().unwrap().clear();

        let days = Arc::clone(&self.search.days);
        let channel = key.1;
        std::thread::spawn(move || match client.recorded_days(channel, month, StreamType::Main) {
            Ok(found) => *days.lock().unwrap() = found,
            Err(err) => warn!("could not list recorded days: {err}"),
        });
    }

    fn play(&mut self, manager: &DeviceManager, clip: Recording) {
        let Some(key) = self.channel.clone() else { return };
        let Some(client) = manager.client(&key.0) else { return };

        match client.download_url(&clip) {
            Ok(Some(url)) => {
                self.player = Some(PlaybackWorker::start(url));
                self.playing = Some(clip);
                self.texture = None;
                self.last_sequence = 0;
            }
            Ok(None) => {
                // Firmware that indexes recordings by time gives no handle to
                // fetch, so say so rather than opening a request that 404s.
                self.player = None;
                self.playing = Some(clip);
            }
            Err(err) => warn!("could not build a playback URL: {err}"),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, manager: &DeviceManager) {
        // Recordings live on the device, so this lists cameras rather than
        // views of them: a crop has no footage of its own to search.
        let sources: Vec<crate::manager::Source> = manager
            .sources()
            .into_iter()
            .filter(|source| !source.is_virtual())
            .collect();
        if sources.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("No cameras").color(theme::PLACEHOLDER));
            });
            return;
        }
        if self.channel.is_none() {
            self.channel = sources.first().map(|s| s.stream_key());
        }

        // Only Reolink serves recordings over its API today. Saying so beats an
        // empty calendar that looks like a camera with nothing recorded.
        if let Some((device, _)) = self.channel.clone() {
            let serves = manager
                .client(&device)
                .map(|client| client.supports_playback())
                .unwrap_or(true);
            if !serves {
                let system = manager
                    .configs()
                    .into_iter()
                    .find(|c| c.id == device)
                    .map(|c| crate::api::vendor::label_for(&c.vendor))
                    .unwrap_or("This system");
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{system} does not serve recordings through its API.\n\
                             Live view works; use its own interface for playback."
                        ))
                        .color(theme::PLACEHOLDER),
                    );
                });
                return;
            }
        }

        // Kick off whatever the current selection needs.
        if let Some(key) = self.channel.clone() {
            if self.months_loaded.as_ref()
                != Some(&(key.clone(), self.month.year() as u32, self.month.month()))
            {
                self.load_month(manager, key.clone(), self.month);
            }
            if self.searched.as_ref() != Some(&(key.clone(), self.day)) {
                self.start_search(manager, key, self.day);
            }
        }

        egui::SidePanel::left("playback-browser")
            .exact_width(280.0)
            .frame(egui::Frame::NONE.fill(theme::PANEL).inner_margin(10))
            .show_inside(ui, |ui| self.browser(ui, manager, &sources));

        self.player_pane(ui);
    }

    fn browser(
        &mut self,
        ui: &mut egui::Ui,
        manager: &DeviceManager,
        sources: &[crate::manager::Source],
    ) {
        ui.label(
            RichText::new("RECORDINGS")
                .size(11.0)
                .strong()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(6.0);

        let current = self
            .channel
            .as_ref()
            .and_then(|key| {
                sources
                    .iter()
                    .find(|s| s.stream_key() == *key)
                    .map(|s| manager.source_label(s))
            })
            .unwrap_or_default();

        egui::ComboBox::from_id_salt("playback-channel")
            .selected_text(current)
            .width(250.0)
            .show_ui(ui, |ui| {
                for source in sources {
                    let label = manager.source_label(source);
                    ui.selectable_value(&mut self.channel, Some(source.stream_key()), label);
                }
            });

        ui.add_space(8.0);
        self.calendar(ui);
        ui.add_space(8.0);

        if *self.search.busy.lock().unwrap() {
            ui.label(RichText::new("Searching…").color(theme::TEXT_DIM));
            return;
        }
        if let Some(error) = self.search.error.lock().unwrap().clone() {
            ui.label(RichText::new(error).size(11.0).color(theme::ERROR));
            return;
        }

        let clips = self.search.clips.lock().unwrap().clone();
        let unusable = clips.iter().filter(|c| !c.is_fetchable()).count();
        let total: i64 = clips.iter().map(|c| c.size).sum();
        ui.label(
            RichText::new(if clips.is_empty() {
                "No recordings on this date".to_string()
            } else {
                format!("{} clip(s) · {:.2} GB", clips.len(), total as f64 / 1e9)
            })
            .size(11.0)
            .color(theme::TEXT_DIM),
        );
        if unusable > 0 {
            ui.label(
                RichText::new(format!("{unusable} not fetchable over the API"))
                    .size(11.0)
                    .color(theme::WARN),
            );
        }

        ui.add_space(4.0);
        let mut chosen: Option<Recording> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for clip in &clips {
                let selected = self
                    .playing
                    .as_ref()
                    .map(|p| p.start == clip.start)
                    .unwrap_or(false);
                let colour = if clip.is_fetchable() {
                    theme::TEXT
                } else {
                    theme::PLACEHOLDER
                };
                let response = ui.selectable_label(selected, RichText::new(clip.label()).color(colour));
                let response = if clip.is_fetchable() {
                    response.on_hover_text(format!(
                        "{} – {}\n{:.1} MB",
                        clip.start.format("%Y-%m-%d %H:%M:%S"),
                        clip.end.format("%H:%M:%S"),
                        clip.size as f64 / 1e6
                    ))
                } else {
                    response.on_hover_text(
                        "This firmware lists recordings without a file name, so they \
                         cannot be streamed or downloaded over the HTTP API.",
                    )
                };
                if response.clicked() {
                    chosen = Some(clip.clone());
                }
            }
        });
        if let Some(clip) = chosen {
            self.play(manager, clip);
        }
    }

    /// A compact month grid, with days holding footage picked out in copper.
    fn calendar(&mut self, ui: &mut egui::Ui) {
        let days = self.search.days.lock().unwrap().clone();

        ui.horizontal(|ui| {
            if ui.small_button("‹").clicked() {
                self.month = shift_month(self.month, -1);
            }
            ui.label(
                RichText::new(self.month.format("%B %Y").to_string())
                    .color(theme::TEXT)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Never offer a future month: there is nothing recorded there.
                let next = shift_month(self.month, 1);
                let allowed = next <= Local::now().date_naive().with_day(1).unwrap();
                if ui.add_enabled(allowed, egui::Button::new("›").small()).clicked() {
                    self.month = next;
                }
            });
        });
        ui.add_space(2.0);

        let first = self.month.with_day(1).unwrap();
        // Monday-first columns.
        let offset = first.weekday().num_days_from_monday() as usize;
        let length = days_in_month(first);
        let today = Local::now().date_naive();

        egui::Grid::new("calendar").spacing(Vec2::new(2.0, 2.0)).show(ui, |ui| {
            for label in ["M", "T", "W", "T", "F", "S", "S"] {
                ui.label(RichText::new(label).size(10.0).color(theme::PLACEHOLDER));
            }
            ui.end_row();

            let mut column = 0;
            for _ in 0..offset {
                ui.label(" ");
                column += 1;
            }
            for day in 1..=length {
                let date = first.with_day(day).unwrap();
                let has_footage = days.contains(&day);
                let selected = date == self.day;
                let future = date > today;

                let mut text = RichText::new(format!("{day:>2}")).size(11.0);
                text = if future {
                    text.color(theme::PLACEHOLDER)
                } else if has_footage {
                    text.color(theme::ACCENT_BRIGHT).strong()
                } else {
                    text.color(theme::TEXT_DIM)
                };

                if ui
                    .add_enabled(!future, egui::SelectableLabel::new(selected, text))
                    .clicked()
                {
                    self.day = date;
                }

                column += 1;
                if column % 7 == 0 {
                    ui.end_row();
                }
            }
        });
    }

    fn player_pane(&mut self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        let transport_height = 34.0;
        let video = egui::Rect::from_min_size(
            area.min,
            Vec2::new(area.width(), (area.height() - transport_height).max(0.0)),
        );

        // --- picture ---------------------------------------------------------
        if let Some(player) = &self.player {
            if let Some(frame) = player.latest_frame() {
                if frame.sequence != self.last_sequence {
                    self.last_sequence = frame.sequence;
                    let image = ColorImage::from_rgba_unmultiplied(
                        [frame.width as usize, frame.height as usize],
                        &frame.rgba,
                    );
                    match &mut self.texture {
                        Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                        None => {
                            self.texture = Some(ui.ctx().load_texture(
                                "playback",
                                image,
                                egui::TextureOptions::LINEAR,
                            ))
                        }
                    }
                }
            }
        }

        let painter = ui.painter_at(video);
        painter.rect_filled(video, egui::CornerRadius::ZERO, theme::INK);
        match &self.texture {
            Some(texture) => {
                let size = texture.size_vec2();
                let scale = (video.width() / size.x).min(video.height() / size.y);
                let target = egui::Rect::from_center_size(video.center(), size * scale);
                painter.image(
                    texture.id(),
                    target,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                let message = match (&self.playing, &self.player) {
                    (Some(clip), None) if !clip.is_fetchable() => {
                        "This recording has no file handle\n\nThe device listed it but gave no \
                         name, so it cannot be fetched over the HTTP API."
                            .to_string()
                    }
                    (Some(_), Some(player)) => player
                        .error()
                        .map(|err| format!("Playback failed\n{err}"))
                        .unwrap_or_else(|| "Buffering…".to_string()),
                    _ => "Select a recording to play".to_string(),
                };
                painter.text(
                    video.center(),
                    egui::Align2::CENTER_CENTER,
                    message,
                    egui::FontId::proportional(13.0),
                    theme::PLACEHOLDER,
                );
            }
        }
        ui.advance_cursor_after_rect(video);

        // --- transport -------------------------------------------------------
        let Some(player) = &self.player else { return };
        let duration = player
            .duration()
            .max(self.playing.as_ref().map(|c| c.duration_seconds() as f64).unwrap_or(0.0));
        let position = self.scrub.unwrap_or_else(|| player.position());

        ui.horizontal(|ui| {
            if ui.button(if player.is_paused() { "▶" } else { "⏸" }).clicked() {
                player.toggle_pause();
            }
            ui.label(RichText::new(clock(position)).size(11.0).color(theme::TEXT_DIM));

            let mut value = position;
            let slider = ui.add(
                egui::Slider::new(&mut value, 0.0..=duration.max(1.0))
                    .show_value(false)
                    .handle_shape(egui::style::HandleShape::Circle),
            );
            if slider.dragged() {
                // Track the handle locally while dragging; seeking on every
                // frame would thrash the device.
                self.scrub = Some(value);
            } else if slider.drag_stopped() {
                if let Some(target) = self.scrub.take() {
                    player.seek(target);
                }
            }

            ui.label(RichText::new(clock(duration)).size(11.0).color(theme::TEXT_DIM));

            egui::ComboBox::from_id_salt("speed")
                .selected_text(format!("{}×", self.speed))
                .width(64.0)
                .show_ui(ui, |ui| {
                    for option in [0.5f32, 1.0, 2.0, 4.0, 8.0] {
                        if ui
                            .selectable_value(&mut self.speed, option, format!("{option}×"))
                            .clicked()
                        {
                            player.set_speed(option);
                        }
                    }
                });
        });
    }
}

fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as i64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn days_in_month(date: NaiveDate) -> u32 {
    let (year, month) = (date.year(), date.month());
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    next.and_then(|d| d.pred_opt()).map(|d| d.day()).unwrap_or(28)
}

fn shift_month(date: NaiveDate, delta: i32) -> NaiveDate {
    let mut year = date.year();
    let mut month = date.month() as i32 + delta;
    while month < 1 {
        month += 12;
        year -= 1;
    }
    while month > 12 {
        month -= 12;
        year += 1;
    }
    NaiveDate::from_ymd_opt(year, month as u32, 1).unwrap_or(date)
}

#[allow(dead_code)]
fn unused(_: NaiveDateTime) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_arithmetic_wraps_years() {
        let january = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        assert_eq!(shift_month(january, -1), NaiveDate::from_ymd_opt(2025, 12, 1).unwrap());
        let december = NaiveDate::from_ymd_opt(2026, 12, 3).unwrap();
        assert_eq!(shift_month(december, 1), NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
    }

    #[test]
    fn month_lengths_including_leap_years() {
        let check = |y, m, expected| {
            assert_eq!(days_in_month(NaiveDate::from_ymd_opt(y, m, 1).unwrap()), expected)
        };
        check(2026, 1, 31);
        check(2026, 2, 28);
        check(2024, 2, 29); // leap
        check(2026, 4, 30);
    }

    #[test]
    fn clock_formats_minutes_and_seconds() {
        assert_eq!(clock(0.0), "00:00");
        assert_eq!(clock(61.0), "01:01");
        assert_eq!(clock(-5.0), "00:00");
        assert_eq!(clock(3599.0), "59:59");
    }
}
