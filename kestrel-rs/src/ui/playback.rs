//! The Playback tab: browse what the device has recorded, and play it back.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime};
use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, Vec2};
use log::warn;

use super::theme;
use crate::api::{EventKind, Recording, StreamType};
use crate::manager::DeviceManager;
use crate::video::PlaybackWorker;

type Key = (String, u32);

/// A recording being fetched over the network — to play, or to keep.
struct Fetch {
    clip: Recording,
    /// Where it is going. A kept file goes to Downloads; one only to play goes to
    /// a cache, so playing it again does not fetch it again.
    to: PathBuf,
    keep: bool,
    progress: Arc<Mutex<f32>>,
    done: Arc<Mutex<Option<Result<(), String>>>>,
}

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

    fetch: Option<Fetch>,
    /// A line about the last download: where it went, or why it did not.
    note: Option<String>,
    /// Which triggers to list; empty lists everything.
    filter: Vec<EventKind>,

    /// The timeline's visible window: its centre and width, in seconds of the day.
    view_centre: f64,
    view_span: f64,
    /// A span chosen on the timeline to download, in seconds of the day.
    range: Option<(f64, f64)>,
    /// What the pointer is dragging on the timeline.
    dragging: Option<Drag>,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    RangeStart,
    RangeEnd,
    /// Panning the view, from where the drag began.
    Pan { from_centre: f64, from_x: f32 },
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
            fetch: None,
            note: None,
            filter: Vec::new(),
            view_centre: DAY_SECONDS as f64 / 2.0,
            view_span: DAY_SECONDS as f64,
            range: None,
            dragging: None,
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
        self.player = None;
        self.texture = None;
        self.last_sequence = 0;
        self.playing = Some(clip.clone());

        match client.download_url(&clip) {
            Ok(Some(url)) => self.player = Some(PlaybackWorker::start(url)),
            // No URL: a recording listed by time only. Where the device replays
            // by time, play it as it arrives — the picture starts in a second or
            // two, however long the recording. (Fetching it whole first took as
            // long as the transfer.)
            Ok(None) => match client.recording_stream(&clip) {
                Some(spec) => self.player = Some(PlaybackWorker::start_recording(spec)),
                None => {
                    // Fetched already for a download: play that copy.
                    let cached = cache_path(&key, &clip);
                    if cached.exists() {
                        self.player = Some(PlaybackWorker::start(cached.to_string_lossy().into_owned()));
                    }
                }
            },
            Err(err) => warn!("could not build a playback URL: {err}"),
        }
    }

    /// Save a recording into Downloads as an MP4.
    fn download(&mut self, manager: &DeviceManager, clip: Recording, downloads: &Path) {
        let Some(key) = self.channel.clone() else { return };
        let camera = manager
            .sources()
            .into_iter()
            .find(|s| s.stream_key() == key)
            .map(|s| manager.source_label(&s))
            .unwrap_or_else(|| format!("Channel {}", key.1 + 1));
        let _ = std::fs::create_dir_all(downloads);
        let to = downloads.join(download_name(&camera, &clip));
        // Already fetched to play: keep that copy rather than fetching again.
        let cached = cache_path(&key, &clip);
        if cached.exists() && std::fs::copy(&cached, &to).is_ok() {
            self.note = Some(format!("Saved {}", to.display()));
            return;
        }
        self.start_fetch(manager, clip, to, true);
    }

    fn start_fetch(&mut self, manager: &DeviceManager, clip: Recording, to: PathBuf, keep: bool) {
        let Some(key) = self.channel.clone() else { return };
        prune_cache();
        let Some(client) = manager.client(&key.0) else { return };
        let progress = Arc::new(Mutex::new(0.0));
        let done = Arc::new(Mutex::new(None));
        let (thread_progress, thread_done, thread_clip, thread_to) =
            (Arc::clone(&progress), Arc::clone(&done), clip.clone(), to.clone());
        std::thread::spawn(move || {
            if let Some(dir) = thread_to.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // Written beside the destination and moved into place, so a fetch
            // that fails halfway never leaves a file that looks finished.
            let partial = thread_to.with_extension("part.mp4");
            let result = client
                .fetch_recording(&thread_clip, &partial, &mut |p| *thread_progress.lock().unwrap() = p)
                .map_err(|e| e.to_string())
                .and_then(|()| std::fs::rename(&partial, &thread_to).map_err(|e| e.to_string()));
            if result.is_err() {
                let _ = std::fs::remove_file(&partial);
            }
            *thread_done.lock().unwrap() = Some(result);
        });
        self.note = None;
        self.fetch = Some(Fetch { clip, to, keep, progress, done });
    }

    /// Pick up a finished fetch: play it, or say where it was saved.
    fn poll_fetch(&mut self, ctx: &egui::Context) {
        let Some(fetch) = &self.fetch else { return };
        let Some(result) = fetch.done.lock().unwrap().take() else {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
            return;
        };
        let fetch = self.fetch.take().unwrap();
        match result {
            Ok(()) if fetch.keep => self.note = Some(format!("Saved {}", fetch.to.display())),
            Ok(()) => {
                if self.playing.as_ref().is_some_and(|p| p.start == fetch.clip.start) {
                    self.player = Some(PlaybackWorker::start(fetch.to.to_string_lossy().into_owned()));
                }
            }
            Err(err) => {
                warn!("could not fetch the recording: {err}");
                self.note = Some(format!(
                    "Could not {} the recording: {err}",
                    if fetch.keep { "download" } else { "fetch" }
                ));
            }
        }
    }

    /// `download_sub` is the remembered download quality, changed here when the
    /// choice beside Download is.
    pub fn show(&mut self, ui: &mut egui::Ui, manager: &DeviceManager, downloads: &Path, download_sub: &mut bool) {
        self.poll_fetch(ui.ctx());
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

        self.player_pane(ui, manager, downloads, download_sub);
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

        let all = self.search.clips.lock().unwrap().clone();
        // A device that replays by time can fetch every clip, named or not.
        let by_time = self
            .channel
            .as_ref()
            .and_then(|key| manager.client(&key.0))
            .map(|client| client.fetches_by_time())
            .unwrap_or(false);
        let fetchable = |clip: &Recording| by_time || clip.is_fetchable();

        // Filter chips, each with how many of the day's clips it matches. The
        // common kinds are always offered (dimmed when the day has none), the
        // rest only when they turn up. A device that does not say what set a
        // recording off — the HTTP search — gets none.
        let count = |kind: EventKind| all.iter().filter(|c| c.triggers.contains(&kind)).count();
        let any_triggers = all.iter().any(|c| !c.triggers.is_empty());
        let kinds: Vec<(EventKind, usize)> = EventKind::RECORDED
            .iter()
            .map(|&k| (k, count(k)))
            .filter(|&(k, n)| n > 0 || EventKind::ALL.contains(&k))
            .collect();
        self.filter.retain(|k| kinds.iter().any(|(kind, n)| kind == k && *n > 0));
        if any_triggers {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if ui.selectable_label(self.filter.is_empty(), format!("All {}", all.len())).clicked() {
                    self.filter.clear();
                }
                for &(kind, n) in &kinds {
                    let on = self.filter.contains(&kind);
                    let colour = if on {
                        theme::TEXT
                    } else if n == 0 {
                        theme::PLACEHOLDER
                    } else {
                        theme::event_color(kind)
                    };
                    let chip = egui::SelectableLabel::new(on, RichText::new(format!("{} {n}", kind.label())).color(colour));
                    if ui.add_enabled(n > 0, chip).clicked() {
                        if on {
                            self.filter.retain(|k| *k != kind);
                        } else {
                            self.filter.push(kind);
                        }
                    }
                }
            });
            ui.add_space(4.0);
        }
        let clips: Vec<Recording> = all.iter().filter(|c| shown(c, &self.filter)).cloned().collect();

        let unusable = clips.iter().filter(|c| !fetchable(c)).count();
        let total: i64 = clips.iter().map(|c| c.size).sum();
        ui.label(
            RichText::new(if all.is_empty() {
                "No recordings on this date".to_string()
            } else if clips.len() < all.len() {
                format!("{} of {} clip(s)", clips.len(), all.len())
            } else if total > 0 {
                format!("{} clip(s) · {:.2} GB", clips.len(), total as f64 / 1e9)
            } else {
                format!("{} clip(s)", clips.len())
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
                let colour = if fetchable(clip) {
                    theme::TEXT
                } else {
                    theme::PLACEHOLDER
                };
                let mut label = clip.label();
                if let Some(kind) = clip.triggers.first() {
                    label = format!("{label} · {}", kind.label());
                }
                let response = ui.selectable_label(selected, RichText::new(label).color(colour));
                let response = if fetchable(clip) {
                    response.on_hover_text(format!(
                        "{} – {}{}",
                        clip.start.format("%Y-%m-%d %H:%M:%S"),
                        clip.end.format("%H:%M:%S"),
                        if clip.size > 0 { format!("\n{:.1} MB", clip.size as f64 / 1e6) } else { String::new() }
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

        // Seven columns sized to the panel. The grid's default minimum column
        // width is wider than a seventh of it, which pushed Sunday off the edge.
        let gap = 2.0;
        let column = ((ui.available_width() - gap * 6.0) / 7.0).floor();
        egui::Grid::new("calendar")
            .spacing(Vec2::new(gap, gap))
            .min_col_width(column)
            .max_col_width(column)
            .show(ui, |ui| {
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

    fn player_pane(&mut self, ui: &mut egui::Ui, manager: &DeviceManager, downloads: &Path, download_sub: &mut bool) {
        let area = ui.available_rect_before_wrap();
        let transport_height = 34.0;
        // The transport row and the timeline below the picture, with the spacing
        // the layout puts between them.
        let below = transport_height + TOOLBAR_HEIGHT + TIMELINE_HEIGHT + ui.spacing().item_spacing.y * 4.0;
        let video = egui::Rect::from_min_size(
            area.min,
            Vec2::new(area.width(), (area.height() - below).max(0.0)),
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
                let fetching = self.fetch.as_ref().filter(|f| !f.keep);
                let message = match (&self.playing, &self.player) {
                    (Some(_), None) if fetching.is_some() => {
                        let p = *fetching.unwrap().progress.lock().unwrap();
                        format!("Fetching the recording… {:.0}%", p * 100.0)
                    }
                    (Some(clip), None) if !clip.is_fetchable() => {
                        "This recording has no file handle\n\nThe device listed it but gave no \
                         name, so it cannot be fetched over the HTTP API."
                            .to_string()
                    }
                    // Listed with a handle, but the device gives no URL to fetch it
                    // from: a device reached over Baichuan, whose recordings can be
                    // found but not yet streamed.
                    (Some(_), None) => {
                        "Playing this recording is not supported yet\n\nThis device's \
                         recordings can be listed, but not yet played or downloaded."
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
        let clip = self.playing.clone();
        ui.horizontal(|ui| {
            ui.set_min_height(transport_height - 4.0);
            let player = self.player.as_ref();
            let duration = player
                .map(|p| p.duration())
                .unwrap_or(0.0)
                .max(clip.as_ref().map(|c| c.duration_seconds() as f64).unwrap_or(0.0));
            let position = self.scrub.unwrap_or_else(|| player.map(|p| p.position()).unwrap_or(0.0));

            let paused = player.map(|p| p.is_paused()).unwrap_or(true);
            if ui.add_enabled(player.is_some(), egui::Button::new(if paused { "▶" } else { "⏸" })).clicked() {
                if let Some(player) = player {
                    player.toggle_pause();
                }
            }
            ui.label(RichText::new(clock(position)).size(11.0).color(theme::TEXT_DIM));

            let mut value = position;
            let slider = ui.add_enabled(
                player.is_some(),
                egui::Slider::new(&mut value, 0.0..=duration.max(1.0))
                    .show_value(false)
                    .handle_shape(egui::style::HandleShape::Circle),
            );
            if slider.dragged() {
                // Track the handle locally while dragging; seeking on every
                // frame would thrash the device.
                self.scrub = Some(value);
            } else if slider.drag_stopped() {
                if let (Some(target), Some(player)) = (self.scrub.take(), player) {
                    player.seek(target);
                }
            }

            ui.label(RichText::new(clock(duration)).size(11.0).color(theme::TEXT_DIM));

            let mut speed = self.speed;
            ui.add_enabled_ui(player.is_some(), |ui| {
                egui::ComboBox::from_id_salt("speed")
                    .selected_text(format!("{speed}×"))
                    .width(64.0)
                    .show_ui(ui, |ui| {
                        for option in [0.5f32, 1.0, 2.0, 4.0, 8.0] {
                            if ui.selectable_value(&mut speed, option, format!("{option}×")).clicked() {
                                if let Some(player) = player {
                                    player.set_speed(option);
                                }
                            }
                        }
                    });
            });
            self.speed = speed;

            // Download quality. Only a system that fetches by time can fetch the
            // sub stream; one that only serves its files over HTTP serves main.
            let either = self
                .channel
                .as_ref()
                .and_then(|key| manager.client(&key.0))
                .map(|client| client.fetches_by_time())
                .unwrap_or(false);
            let quality = if *download_sub && either { "Sub" } else { "Main" };
            ui.add_enabled_ui(either, |ui| {
                egui::ComboBox::from_id_salt("download-quality")
                    .selected_text(quality)
                    .width(64.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(download_sub, false, "Main")
                            .on_hover_text("The full-quality recording");
                        ui.selectable_value(download_sub, true, "Sub")
                            .on_hover_text("Many times smaller (often 20-30x), so far quicker to download");
                    })
                    .response
                    .on_hover_text("Which stream Download saves")
                    .on_disabled_hover_text("This system only serves the main recording for download");
            });

            // Download: whatever this recording is, as an MP4 in Downloads.
            let downloading = self.fetch.as_ref().filter(|f| f.keep).map(|f| *f.progress.lock().unwrap());
            let can = clip.is_some() && self.fetch.is_none();
            let label = match downloading {
                Some(p) => format!("Downloading… {:.0}%", p * 100.0),
                None => "Download".to_string(),
            };
            if ui
                .add_enabled(can, egui::Button::new(label))
                .on_hover_text(format!("Save as an MP4 in {}", downloads.display()))
                .clicked()
            {
                if let Some(mut clip) = clip.clone() {
                    if *download_sub && either {
                        clip.stream_type = StreamType::Sub;
                    }
                    self.download(manager, clip, downloads);
                }
            }
            if let Some(note) = &self.note {
                ui.label(RichText::new(note).size(11.0).color(theme::TEXT_DIM));
            }
        });

        // --- timeline --------------------------------------------------------
        let sub = *download_sub && self.can_fetch(manager);
        self.timeline_toolbar(ui, manager, downloads, sub);
        if let Some(clip) = self.timeline(ui) {
            self.play(manager, clip);
        }
    }

    /// Whether the selected device can fetch any span by time — what a range
    /// download needs.
    fn can_fetch(&self, manager: &DeviceManager) -> bool {
        self.channel
            .as_ref()
            .and_then(|key| manager.client(&key.0))
            .map(|client| client.fetches_by_time())
            .unwrap_or(false)
    }

    /// The playing point, in seconds of the day, when something is playing.
    fn playhead(&self) -> Option<f64> {
        let clip = self.playing.as_ref()?;
        let day_start = self.day.and_hms_opt(0, 0, 0)?;
        let position = self.player.as_ref().map(|p| p.position()).unwrap_or(0.0);
        Some((clip.start - day_start).num_milliseconds() as f64 / 1000.0 + position)
    }

    /// Zoom, and the range to download.
    fn timeline_toolbar(&mut self, ui: &mut egui::Ui, manager: &DeviceManager, downloads: &Path, sub: bool) {
        ui.horizontal(|ui| {
            ui.set_min_height(TOOLBAR_HEIGHT);
            ui.spacing_mut().item_spacing.x = 4.0;
            for (label, span) in ZOOMS {
                let on = (self.view_span - span).abs() < 1.0;
                if ui.selectable_label(on, label).clicked() {
                    self.zoom_to(span, self.playhead().unwrap_or(self.view_centre));
                }
            }
            ui.separator();

            let by_time = self.can_fetch(manager);
            match self.range {
                None => {
                    if ui
                        .add_enabled(by_time, egui::Button::new("Select range"))
                        .on_hover_text("Drag the two handles on the timeline to the span to save")
                        .on_disabled_hover_text("This system only downloads whole recordings")
                        .clicked()
                    {
                        // A minute around the playing point, or the middle of the view;
                        // zoomed in far enough to place the handles by the second.
                        let centre = self.playhead().unwrap_or(self.view_centre);
                        if self.view_span > 3600.0 {
                            self.zoom_to(600.0, centre);
                        }
                        self.range = Some(((centre - 30.0).max(0.0), (centre + 30.0).min(DAY_SECONDS as f64)));
                    }
                }
                Some((from, to)) => {
                    let label = format!(
                        "Download {} – {} ({}){}",
                        clock_of_day_seconds(from),
                        clock_of_day_seconds(to),
                        clock(to - from),
                        if sub { " · sub" } else { "" }
                    );
                    let busy = self.fetch.is_some();
                    if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                        if let Some(key) = self.channel.clone() {
                            let day_start = self.day.and_hms_opt(0, 0, 0).unwrap();
                            let at = |s: f64| day_start + chrono::Duration::milliseconds((s * 1000.0) as i64);
                            let span = Recording {
                                channel: key.1,
                                start: at(from),
                                end: at(to),
                                name: String::new(),
                                size: 0,
                                stream_type: if sub { StreamType::Sub } else { StreamType::Main },
                                width: 0,
                                height: 0,
                                frame_rate: 0,
                                playback_time: None,
                                triggers: Vec::new(),
                            };
                            self.download(manager, span, downloads);
                        }
                    }
                    if ui.small_button("✕").on_hover_text("Clear the range").clicked() {
                        self.range = None;
                    }
                }
            }
        });
    }

    /// Show `span` seconds of the day around `centre`, kept inside the day.
    fn zoom_to(&mut self, span: f64, centre: f64) {
        self.view_span = span.clamp(60.0, DAY_SECONDS as f64);
        let half = self.view_span / 2.0;
        self.view_centre = centre.clamp(half, DAY_SECONDS as f64 - half);
    }

    /// The day as a strip, each clip a mark coloured by what set it off, with the
    /// playing point and any range chosen to download. The wheel zooms around the
    /// pointer and a drag pans; the range's handles drag. Clicking plays the clip
    /// under the pointer, or the nearest one.
    fn timeline(&mut self, ui: &mut egui::Ui) -> Option<Recording> {
        let clips: Vec<Recording> = self
            .search
            .clips
            .lock()
            .unwrap()
            .iter()
            .filter(|c| shown(c, &self.filter))
            .cloned()
            .collect();
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), TIMELINE_HEIGHT),
            egui::Sense::click_and_drag(),
        );
        let painter = ui.painter_at(rect);
        let bar = egui::Rect::from_min_max(
            rect.min + Vec2::new(8.0, 4.0),
            egui::pos2(rect.max.x - 8.0, rect.max.y - 16.0),
        );
        painter.rect_filled(bar, egui::CornerRadius::same(2), theme::PANEL_ALT);

        // This frame's view, copied so the pointer can change the real one below.
        let view_span = self.view_span;
        let view_start = self.view_centre - view_span / 2.0;
        let x_of = |secs: f64| bar.left() + bar.width() * ((secs - view_start) / view_span) as f32;
        let secs_at = |x: f32| (view_start + ((x - bar.left()) / bar.width()) as f64 * view_span).clamp(0.0, DAY_SECONDS as f64);
        let day_start = self.day.and_hms_opt(0, 0, 0).unwrap();
        let of_day = |t: NaiveDateTime| (t - day_start).num_milliseconds() as f64 / 1000.0;

        // Ticks at a step that suits the zoom, labelled inside the bar's ends.
        let step = tick_step(view_span);
        let mut tick = (view_start / step).ceil() * step;
        while tick <= view_start + view_span + 0.5 {
            let x = x_of(tick);
            painter.line_segment(
                [egui::pos2(x, bar.bottom()), egui::pos2(x, bar.bottom() + 3.0)],
                egui::Stroke::new(1.0_f32, theme::BORDER),
            );
            let align = if x - bar.left() < 20.0 {
                egui::Align2::LEFT_TOP
            } else if bar.right() - x < 20.0 {
                egui::Align2::RIGHT_TOP
            } else {
                egui::Align2::CENTER_TOP
            };
            painter.text(
                egui::pos2(x, bar.bottom() + 4.0),
                align,
                if step < 60.0 { clock_of_day_seconds(tick) } else { clock_of_day(tick) },
                egui::FontId::proportional(9.0),
                theme::PLACEHOLDER,
            );
            tick += step;
        }

        for clip in &clips {
            let (left, right) = (x_of(of_day(clip.start)), x_of(of_day(clip.end)));
            if right < bar.left() || left > bar.right() {
                continue;
            }
            let mark = egui::Rect::from_min_max(
                egui::pos2(left.max(bar.left()), bar.top()),
                egui::pos2(right.max(left + 2.0).min(bar.right()), bar.bottom()),
            );
            let colour = clip.triggers.first().map(|k| theme::event_color(*k)).unwrap_or(theme::ACCENT_DEEP);
            painter.rect_filled(mark, egui::CornerRadius::ZERO, colour);
        }

        // The range to download: shaded, with a handle at each end.
        if let Some((from, to)) = self.range {
            let (left, right) = (x_of(from), x_of(to));
            let shade = egui::Rect::from_min_max(egui::pos2(left, bar.top()), egui::pos2(right, bar.bottom()));
            painter.rect_filled(shade.intersect(bar), egui::CornerRadius::ZERO, theme::TEXT.gamma_multiply(0.18));
            for x in [left, right] {
                if (bar.left()..=bar.right()).contains(&x) {
                    painter.rect_filled(
                        egui::Rect::from_center_size(egui::pos2(x, bar.center().y), Vec2::new(4.0, bar.height() + 6.0)),
                        egui::CornerRadius::same(1),
                        theme::TEXT,
                    );
                }
            }
        }
        if let Some(at) = self.playhead() {
            let x = x_of(at);
            if (bar.left()..=bar.right()).contains(&x) {
                painter.line_segment(
                    [egui::pos2(x, bar.top() - 2.0), egui::pos2(x, bar.bottom() + 2.0)],
                    egui::Stroke::new(2.0_f32, theme::ACCENT_BRIGHT),
                );
            }
        }

        // --- the pointer ------------------------------------------------------
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                if let Some(pos) = response.hover_pos() {
                    let anchor = secs_at(pos.x);
                    let span = (self.view_span * (1.0 - scroll as f64 / 200.0).clamp(0.5, 2.0)).clamp(60.0, DAY_SECONDS as f64);
                    // Keep the time under the pointer where it is.
                    let fraction = ((pos.x - bar.left()) / bar.width()) as f64;
                    let start = (anchor - fraction * span).clamp(0.0, DAY_SECONDS as f64 - span);
                    self.view_span = span;
                    self.view_centre = start + span / 2.0;
                }
            }
        }
        if response.drag_started() {
            if let Some(pos) = response.interact_pointer_pos() {
                let near = |secs: f64| (x_of(secs) - pos.x).abs() <= 6.0;
                self.dragging = match self.range {
                    Some((from, _)) if near(from) => Some(Drag::RangeStart),
                    Some((_, to)) if near(to) => Some(Drag::RangeEnd),
                    _ => Some(Drag::Pan { from_centre: self.view_centre, from_x: pos.x }),
                };
            }
        }
        if response.dragged() {
            if let (Some(drag), Some(pos)) = (self.dragging, response.interact_pointer_pos()) {
                match (drag, self.range.as_mut()) {
                    (Drag::RangeStart, Some(range)) => range.0 = secs_at(pos.x).min(range.1 - 1.0),
                    (Drag::RangeEnd, Some(range)) => range.1 = secs_at(pos.x).max(range.0 + 1.0),
                    (Drag::Pan { from_centre, from_x }, _) => {
                        let moved = ((from_x - pos.x) / bar.width()) as f64 * self.view_span;
                        let half = self.view_span / 2.0;
                        self.view_centre = (from_centre + moved).clamp(half, DAY_SECONDS as f64 - half);
                    }
                    _ => {}
                }
            }
        }
        if response.drag_stopped() {
            self.dragging = None;
        }
        if let Some(pos) = response.hover_pos() {
            let when = day_start + chrono::Duration::milliseconds((secs_at(pos.x) * 1000.0) as i64);
            let near = clip_near(&clips, when, bar.width() * (DAY_SECONDS as f64 / self.view_span) as f32);
            let text = match near {
                Some(c) => format!(
                    "{} · {}{}",
                    when.format("%H:%M:%S"),
                    c.label(),
                    c.triggers.first().map(|k| format!(" · {}", k.label())).unwrap_or_default()
                ),
                None => when.format("%H:%M:%S").to_string(),
            };
            response.clone().on_hover_text_at_pointer(text);
        }
        if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let when = day_start + chrono::Duration::milliseconds((secs_at(pos.x) * 1000.0) as i64);
                return clip_near(&clips, when, bar.width() * (DAY_SECONDS as f64 / self.view_span) as f32).cloned();
            }
        }
        None
    }
}

/// The zoom levels offered, as (label, seconds shown).
const ZOOMS: [(&str, f64); 5] = [("24h", 86400.0), ("6h", 21600.0), ("1h", 3600.0), ("10m", 600.0), ("2m", 120.0)];

const TOOLBAR_HEIGHT: f32 = 24.0;

/// A tick spacing that puts a handful of labels across `span` seconds.
fn tick_step(span: f64) -> f64 {
    [10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 3.0 * 3600.0, 6.0 * 3600.0]
        .into_iter()
        .find(|step| span / step <= 10.0)
        .unwrap_or(6.0 * 3600.0)
}

/// Seconds of the day as HH:MM.
fn clock_of_day(secs: f64) -> String {
    let s = secs.round().max(0.0) as i64;
    format!("{:02}:{:02}", s / 3600, s / 60 % 60)
}

/// Seconds of the day as HH:MM:SS, for a zoom fine enough to want them.
fn clock_of_day_seconds(secs: f64) -> String {
    let s = secs.round().max(0.0) as i64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

const TIMELINE_HEIGHT: f32 = 40.0;
const DAY_SECONDS: i64 = 24 * 3600;

/// The clip at `when`, or else the one starting nearest it — within a few pixels'
/// worth of the day, so a click on empty time does not jump across hours.
fn clip_near(clips: &[Recording], when: NaiveDateTime, bar_width: f32) -> Option<&Recording> {
    if let Some(inside) = clips.iter().find(|c| c.start <= when && when <= c.end) {
        return Some(inside);
    }
    let reach = (DAY_SECONDS as f32 / bar_width.max(1.0) * 6.0) as i64;
    clips
        .iter()
        .map(|c| (c, (c.start - when).num_seconds().abs()))
        .filter(|(_, gap)| *gap <= reach)
        .min_by_key(|(_, gap)| *gap)
        .map(|(c, _)| c)
}

/// Whether a clip passes the trigger filter. No filter shows everything; with one,
/// a clip that carries no trigger at all is hidden.
fn shown(clip: &Recording, filter: &[EventKind]) -> bool {
    filter.is_empty() || clip.triggers.iter().any(|k| filter.contains(k))
}

/// Where a recording fetched only to play is kept, so playing it again is
/// instant. Named by everything that makes it that recording.
fn cache_path(key: &Key, clip: &Recording) -> PathBuf {
    let device: String = key.0.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    std::env::temp_dir().join("kestrel-playback").join(format!(
        "{device}-{}-{}-{}-{}.mp4",
        key.1,
        clip.start.format("%Y%m%d%H%M%S"),
        clip.end.format("%H%M%S"),
        match clip.stream_type {
            StreamType::Main => "main",
            StreamType::Sub => "sub",
        }
    ))
}

/// How long a recording fetched only to play is kept for playing again.
const CACHE_KEEP: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Clear out recordings fetched to play more than a day ago, so the cache does not
/// grow without end. Downloads live elsewhere and are never touched.
fn prune_cache() {
    let Some(dir) = cache_dir_entries() else { return };
    for entry in dir.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > CACHE_KEEP);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn cache_dir_entries() -> Option<std::fs::ReadDir> {
    std::fs::read_dir(std::env::temp_dir().join("kestrel-playback")).ok()
}

/// A download's file name: the camera and when, readable in a file manager.
fn download_name(camera: &str, clip: &Recording) -> String {
    let camera: String = camera
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' })
        .collect();
    // A sub-stream copy says so, so it is not mistaken for the full recording.
    let sub = if clip.stream_type == StreamType::Sub { " (sub)" } else { "" };
    // Start and end, so two spans from the same moment do not overwrite each other.
    format!(
        "{} {}-{}{sub}.mp4",
        camera.trim(),
        clip.start.format("%Y-%m-%d %H.%M.%S"),
        clip.end.format("%H.%M.%S")
    )
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

    fn clip(h: u32, m: u32, secs: i64, kinds: &[EventKind]) -> Recording {
        let start = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(h, m, 0).unwrap();
        Recording {
            channel: 0,
            start,
            end: start + chrono::Duration::seconds(secs),
            name: String::new(),
            size: 0,
            stream_type: StreamType::Main,
            width: 0,
            height: 0,
            frame_rate: 0,
            playback_time: None,
            triggers: kinds.to_vec(),
        }
    }

    #[test]
    fn the_filter_shows_matching_clips_and_none_hides_nothing() {
        let person = clip(1, 0, 10, &[EventKind::Person]);
        let motion = clip(2, 0, 10, &[EventKind::Motion]);
        let bare = clip(3, 0, 10, &[]);
        assert!([&person, &motion, &bare].iter().all(|c| shown(c, &[])));
        assert!(shown(&person, &[EventKind::Person]));
        assert!(!shown(&motion, &[EventKind::Person]));
        assert!(!shown(&bare, &[EventKind::Person]), "no trigger, so not a person");
    }

    #[test]
    fn a_click_finds_the_clip_under_it_or_the_one_just_by() {
        let clips = vec![clip(1, 0, 60, &[]), clip(5, 0, 60, &[])];
        let at = |h, m, s| NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(h, m, s).unwrap();
        // 1000 px for the day: ~86 s a pixel, so "near" is ~8.6 minutes.
        assert_eq!(clip_near(&clips, at(1, 0, 30), 1000.0).unwrap().start, clips[0].start);
        assert_eq!(clip_near(&clips, at(4, 55, 0), 1000.0).unwrap().start, clips[1].start);
        assert!(clip_near(&clips, at(3, 0, 0), 1000.0).is_none(), "empty time is not a jump");
    }

    #[test]
    fn a_download_is_named_by_camera_and_time() {
        let c = clip(20, 6, 11, &[]);
        assert_eq!(download_name("Front Door", &c), "Front Door 2026-10-05 20.06.00-20.06.11.mp4");
        assert_eq!(download_name("a/b", &c), "a_b 2026-10-05 20.06.00-20.06.11.mp4");
        let sub = Recording { stream_type: StreamType::Sub, ..c };
        assert_eq!(download_name("Front Door", &sub), "Front Door 2026-10-05 20.06.00-20.06.11 (sub).mp4");
    }

    #[test]
    fn ticks_suit_the_zoom() {
        assert_eq!(tick_step(86400.0), 3.0 * 3600.0);
        assert_eq!(tick_step(3600.0), 600.0);
        assert_eq!(tick_step(120.0), 15.0);
        assert_eq!(clock_of_day(3600.0 * 13.5), "13:30");
        assert_eq!(clock_of_day_seconds(61.0), "00:01:01");
    }

    #[test]
    fn zooming_stays_inside_the_day() {
        let mut view = PlaybackView::default();
        view.zoom_to(600.0, 10.0);
        assert_eq!(view.view_span, 600.0);
        assert_eq!(view.view_centre, 300.0, "pushed in from the start of the day");
        view.zoom_to(10.0, 50_000.0);
        assert_eq!(view.view_span, 60.0, "never narrower than a minute");
    }
}
