//! Device list and preferences.
//!
//! The on-disk format is deliberately identical to the Python client's:
//! `~/.config/kestrel/config.json`, mode 0600, with passwords in the system
//! keyring under service `kestrel` and account `{id}:{user}@{host}`. That means
//! an existing installation carries straight over — same devices, same saved
//! credentials, no re-entry.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use log::{debug, warn};
use serde::{Deserialize, Serialize};

const APP_NAME: &str = "kestrel";
const KEYRING_SERVICE: &str = "kestrel";

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

fn default_media_dir() -> PathBuf {
    dirs::video_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Videos"))
        .join("Kestrel")
}

fn new_id() -> String {
    // Short random hex, matching the Python client's uuid4()[:12].
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mixed = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:012x}", mixed & 0xFFFF_FFFF_FFFF)
}

/// A fresh identifier for a virtual camera.
pub fn new_view_id() -> String {
    new_id()
}

/// A saved crop of one camera, shown on the wall as a camera of its own.
///
/// A camera pointed along a driveway sees the gate, the porch and the road at
/// once, and the wall can only ever show you all three or none of them. A
/// virtual camera is the framing you would have set by hand — scroll to zoom,
/// drag to pan — written down and given a name, so it survives the tile being
/// parked, the page being turned and the app being restarted.
///
/// It is not a second connection. The crop is a different rectangle out of the
/// picture its parent is already decoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VirtualCamera {
    /// Stable across renames and reordering, so a tile keeps its identity and
    /// its place in the page order when the list is edited.
    #[serde(default = "new_id")]
    pub id: String,
    /// The channel it crops into.
    pub channel: u32,
    pub name: String,
    /// Magnification into the parent's picture; 1.0 would be the whole thing.
    pub zoom: f32,
    /// Centre of the crop in the parent's picture, 0..1 on each axis.
    pub centre: (f32, f32),
    /// Kept off the wall until it is shown again, exactly as a channel can be.
    #[serde(default)]
    pub hidden: bool,
}

impl VirtualCamera {
    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            format!("{:.1}x view", self.zoom)
        } else {
            self.name.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// Which system this is. Absent in configs written before Kestrel spoke to
    /// anything but Reolink, which is exactly what those devices were.
    #[serde(default = "default_vendor")]
    pub vendor: String,
    pub host: String,
    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_rtsp_port")]
    pub rtsp_port: u16,
    #[serde(default)]
    pub https: bool,
    /// Channels the user has hidden, by index.
    ///
    /// Stored per device rather than as a preference because it describes the
    /// hardware — an NVR input with nothing plugged into it, or a camera
    /// pointed at something nobody wants on the wall — and should survive the
    /// device being edited.
    #[serde(default)]
    pub hidden_channels: Vec<u32>,
    /// Channels follow-motion leaves alone, by index.
    ///
    /// Beside the hidden ones and for the same reason: it describes what the
    /// camera is *pointed at* rather than how the app is set up. A drive that
    /// catches every car on the road, or a doorbell looking at the pavement, is
    /// worth having on the wall and not worth being yanked to every time
    /// something moves — and that stays true however following is configured.
    ///
    /// Kept as the exceptions rather than as the members, so a camera nobody
    /// has ever mentioned behaves the way it always did and a device that grows
    /// a channel gets the ordinary treatment for it rather than silence.
    #[serde(default)]
    pub unfollowed_channels: Vec<u32>,
    /// Crops of this device's channels, shown as cameras in their own right.
    ///
    /// Beside the two exception lists and for the same reason: a virtual
    /// camera describes what a lens is pointed at, so it belongs to the box
    /// that owns the lens and should outlive any edit to how the app reaches
    /// it.
    #[serde(default)]
    pub virtual_cameras: Vec<VirtualCamera>,
    /// Virtual cameras follow-motion leaves alone, by id.
    ///
    /// Kept apart from `unfollowed_channels` rather than sharing it: those are
    /// channel indices and these are ids, and one list holding both kinds of
    /// name is a list nothing can read without asking which it got.
    #[serde(default)]
    pub unfollowed_views: Vec<String>,
    /// Trust this device's certificate even though nothing vouches for it.
    ///
    /// Off unless asked for, and per device rather than global: it buys
    /// encryption without authentication, which is right for an appliance on a
    /// local network that ships a self-signed certificate, and wrong as a
    /// blanket default.
    #[serde(default)]
    pub allow_self_signed: bool,
    #[serde(default)]
    pub label: String,
    #[serde(default = "new_id")]
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub cached_name: String,
    #[serde(default)]
    pub cached_model: String,
    #[serde(default = "default_channels")]
    pub cached_channels: u32,

    /// Only serialised when no keyring is available.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
}

fn default_vendor() -> String {
    crate::api::vendor::DEFAULT_VENDOR.to_string()
}

fn default_username() -> String { "admin".into() }
fn default_port() -> u16 { 80 }
fn default_rtsp_port() -> u16 { 554 }
fn default_true() -> bool { true }
fn default_channels() -> u32 { 1 }

impl DeviceConfig {
    /// Move the ports to where the chosen system actually listens.
    ///
    /// Only touched when they are still at another system's defaults, so a
    /// deliberate choice is never overwritten.
    pub fn is_hidden(&self, channel: u32) -> bool {
        self.hidden_channels.contains(&channel)
    }

    /// Hide or show one channel. Returns whether anything changed.
    pub fn set_hidden(&mut self, channel: u32, hidden: bool) -> bool {
        let was = self.is_hidden(channel);
        if hidden == was {
            return false;
        }
        if hidden {
            self.hidden_channels.push(channel);
            // Sorted so the config file reads sensibly and two runs that hide
            // the same set produce the same file.
            self.hidden_channels.sort_unstable();
        } else {
            self.hidden_channels.retain(|c| *c != channel);
        }
        true
    }

    /// Whether follow-motion may bring this channel up.
    pub fn follows_motion(&self, channel: u32) -> bool {
        !self.unfollowed_channels.contains(&channel)
    }

    /// Let follow-motion have this channel, or leave it out. Returns whether
    /// anything changed.
    pub fn set_follows_motion(&mut self, channel: u32, follows: bool) -> bool {
        if follows == self.follows_motion(channel) {
            return false;
        }
        if follows {
            self.unfollowed_channels.retain(|c| *c != channel);
        } else {
            self.unfollowed_channels.push(channel);
            // Sorted, so the file reads sensibly and two runs that exclude the
            // same cameras write the same document.
            self.unfollowed_channels.sort_unstable();
        }
        true
    }

    // ------------------------------------------------- virtual cameras

    pub fn virtual_camera(&self, id: &str) -> Option<&VirtualCamera> {
        self.virtual_cameras.iter().find(|v| v.id == id)
    }

    /// The virtual cameras cropped out of one channel, in the order they were
    /// made — which is the order they take on the wall, straight after the
    /// camera they came from.
    pub fn views_of(&self, channel: u32) -> impl Iterator<Item = &VirtualCamera> {
        self.virtual_cameras.iter().filter(move |v| v.channel == channel)
    }

    pub fn add_virtual_camera(&mut self, view: VirtualCamera) {
        self.virtual_cameras.push(view);
    }

    /// Replace one by id. Returns whether it was there to replace.
    pub fn update_virtual_camera(&mut self, view: VirtualCamera) -> bool {
        match self.virtual_cameras.iter_mut().find(|v| v.id == view.id) {
            Some(slot) => {
                *slot = view;
                true
            }
            None => false,
        }
    }

    /// Forget one, and any exception recorded against it. Returns whether
    /// anything went.
    pub fn remove_virtual_camera(&mut self, id: &str) -> bool {
        let before = self.virtual_cameras.len();
        self.virtual_cameras.retain(|v| v.id != id);
        // Otherwise the exception outlives the camera and silently attaches
        // itself to the next one to be given the same id.
        self.unfollowed_views.retain(|v| v != id);
        self.virtual_cameras.len() != before
    }

    /// Hide or show one virtual camera. Returns whether anything changed.
    pub fn set_view_hidden(&mut self, id: &str, hidden: bool) -> bool {
        match self.virtual_cameras.iter_mut().find(|v| v.id == id) {
            Some(view) if view.hidden != hidden => {
                view.hidden = hidden;
                true
            }
            _ => false,
        }
    }

    /// Whether follow-motion may bring this virtual camera up.
    pub fn view_follows_motion(&self, id: &str) -> bool {
        !self.unfollowed_views.iter().any(|v| v == id)
    }

    /// Let follow-motion have this virtual camera, or leave it out. Returns
    /// whether anything changed.
    pub fn set_view_follows_motion(&mut self, id: &str, follows: bool) -> bool {
        if follows == self.view_follows_motion(id) {
            return false;
        }
        if follows {
            self.unfollowed_views.retain(|v| v != id);
        } else {
            self.unfollowed_views.push(id.to_string());
            // Sorted, for the reason the other two exception lists are.
            self.unfollowed_views.sort();
        }
        true
    }

    pub fn apply_vendor_defaults(&mut self) {
        let defaults: &[(&str, u16, bool, u16)] = &[
            // vendor, HTTP port, HTTPS, stream port
            ("reolink", 80, false, 554),
            ("frigate", 5000, false, 8554),
            ("zoneminder", 80, false, 554),
            ("qnap", 8080, false, 554),
            ("unifi", 443, true, 7441),
        ];
        let known: Vec<u16> = defaults.iter().map(|(_, port, _, _)| *port).collect();
        let known_stream: Vec<u16> = defaults.iter().map(|(_, _, _, port)| *port).collect();

        if let Some((_, port, https, stream)) =
            defaults.iter().find(|(id, _, _, _)| *id == self.vendor)
        {
            if known.contains(&self.port) {
                self.port = *port;
                self.https = *https;
            }
            if known_stream.contains(&self.rtsp_port) {
                self.rtsp_port = *stream;
            }
        }
    }
}

impl Default for DeviceConfig {
    fn default() -> Self {
        DeviceConfig {
            vendor: default_vendor(),
            host: String::new(),
            username: default_username(),
            port: default_port(),
            rtsp_port: default_rtsp_port(),
            https: false,
            hidden_channels: Vec::new(),
            unfollowed_channels: Vec::new(),
            virtual_cameras: Vec::new(),
            unfollowed_views: Vec::new(),
            allow_self_signed: false,
            label: String::new(),
            id: new_id(),
            enabled: true,
            cached_name: String::new(),
            cached_model: String::new(),
            cached_channels: 1,
            password: String::new(),
        }
    }
}

impl DeviceConfig {
    pub fn display_name(&self) -> &str {
        if !self.label.is_empty() {
            &self.label
        } else if !self.cached_name.is_empty() {
            &self.cached_name
        } else {
            &self.host
        }
    }

    fn keyring_account(&self) -> String {
        format!("{}:{}@{}", self.id, self.username, self.host)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub media_dir: String,
    pub grid_size: usize,
    /// How a picture sits in the cell it is given: "stretch", "fit" or "fill".
    /// See [`PictureFill`], which is where the trade-off is written down.
    pub picture_fill: String,
    pub live_substream: bool,
    /// Stream used when one camera fills the pane: best quality by default.
    pub expanded_stream: String,
    /// Give a camera with a virtual camera on screen its main stream.
    ///
    /// On by default, because a virtual camera exists to magnify and the sub
    /// stream has nothing to magnify: a 4x crop of 640x360 is 160x90 blown up
    /// to fill a cell. The cost is that the camera and every crop of it now
    /// share a main-stream decode rather than a sub-stream one, which on a
    /// wall of sixteen is worth being able to decline.
    pub virtual_cameras_use_main_stream: bool,
    pub event_poll_seconds: f32,
    pub desktop_notifications: bool,
    /// Which detection types raise a desktop alert, by device key.
    ///
    /// Everything by default, matching how notifications behaved before this was
    /// configurable. Plain motion is the noisy one — a tree in wind will trigger
    /// it all afternoon — so it is usually the first to turn off.
    pub notify_kinds: Vec<String>,
    /// Whether the selected camera's sound is played. On by default; muting is
    /// remembered, because a client that un-mutes itself every launch is a
    /// client that eventually surprises someone.
    pub audio_enabled: bool,
    pub show_offline_channels: bool,
    /// Hide camera names until the mouse moves.
    ///
    /// On by default. When hiding, the name floats over the picture the way a
    /// video player's controls do, so the video keeps the whole tile; with it
    /// off the name takes a permanent strip and never covers the feed.
    pub auto_hide_titles: bool,
    /// How long the mouse must be still before the names fade out.
    pub title_hide_seconds: f32,
    /// Hide the mouse pointer too, while it rests over the camera pictures.
    pub hide_pointer_when_idle: bool,
    /// Show each tile's frame rate and bitrate in its name strip.
    ///
    /// Off by default: it is diagnostic detail, useful when a camera looks wrong
    /// and clutter the rest of the time.
    pub show_stream_stats: bool,
    /// Whether the camera list starts shown.
    ///
    /// Only what it does at launch: the list is collapsed and opened from the
    /// header at any time, and that is a mode rather than a setting - the same
    /// distinction follow-motion draws. Somebody who wants the wall wide by
    /// default says so here; somebody who wants it wide for a minute presses
    /// the button.
    pub sidebar_open: bool,
    /// Whether the top bar takes itself away when the mouse goes still, the way
    /// the camera names do.
    ///
    /// Off by default, because a toolbar that vanishes is startling if you did
    /// not ask for it. On, it uses the same delay the names use: one idle
    /// timer, one answer, so the wall does not clear itself in two stages.
    pub auto_hide_header: bool,
    /// Note: whether following is *active* is not stored. It is a mode you
    /// enter, like the Live/Playback switch, and starting in it after a restart
    /// would be surprising. Only how it behaves is remembered.
    pub follow_dwell_seconds: f32,
    /// Which detection types follow-motion reacts to, by the device's own
    /// names: motion, people, vehicle, dog_cat, face. Everything still appears
    /// in the event feed and notifications; this only steers the live view.
    pub follow_kinds: Vec<String>,
    pub warm_streams: bool,
    pub max_warm_streams: usize,
    pub collapsed_devices: Vec<String>,

    // ---------------------------------------------------------------- weather
    //
    // Nothing here is a device: there is no session, no credentials and no
    // vendor dispatch, so a weather station is a handful of settings rather
    // than an entry in the device list. See `crate::weather`.
    pub weather_enabled: bool,
    /// Where it comes from: "nws" is the National Weather Service, read over
    /// the internet from a ZIP code; "weewx" is a weewx server on your own
    /// network. The service is the default because far more people have a ZIP
    /// code than have a weather station, and it is the whole of the setup.
    pub weather_source: String,
    /// weewx: the address of the document to read.
    pub weewx_url: String,
    /// The same allowance the NVRs get, and for the same reason — a weewx
    /// server on a home network is regularly behind a private CA. Per source
    /// rather than global, and not used by the weather.gov source, which talks
    /// to one public host with an ordinary certificate.
    pub weewx_allow_self_signed: bool,
    /// The ZIP code as it was typed, and what it resolved to.
    ///
    /// The service is addressed by coordinate and knows nothing about ZIP
    /// codes, so the lookup happens once when it is entered — see
    /// `crate::weather::zip` — and what is polled with is the coordinate. The
    /// code is kept so preferences can show what was typed.
    pub weather_zip: String,
    pub weather_lat: String,
    pub weather_lon: String,
    /// Degrees, speeds and distances: false is US customary, which is the
    /// default and where the service, the radar and nearly every user of this
    /// already are.
    ///
    /// It governs the readings only when they come from weather.gov — a weewx
    /// server reports in whatever units it is configured for and those are
    /// shown as they come — but it governs the radar's distances either way,
    /// which is why it is asked for whatever the source is.
    pub weather_metric: bool,
    /// A weewx archive record is minutes apart, and a service observation is
    /// hourly, so there is nothing to gain from asking more often than this.
    pub weather_poll_seconds: f32,
    /// Whether the strip appears over the camera grid. The Weather tab is
    /// always there once the weather is on; this is the glance-while-watching
    /// half, and it costs the grid a band of height.
    pub weather_bar: bool,
    pub weather_bar_height: f32,
    /// 22:47 or 10:47 PM. `None` follows the machine's locale, which is what
    /// decides it for every other program — see
    /// `crate::weather::locale_prefers_24_hour`.
    pub clock_24_hour: Option<bool>,

    /// The National Weather Service radar. Independent of which source the
    /// readings come from: a weewx server has thermometers and no idea where
    /// the rain is. What it does need is the location, so the ZIP code has to
    /// be set even for somebody reading their own station.
    pub radar_enabled: bool,
    /// Whether the radar also takes a cell on the camera wall, and on what
    /// terms: "never", "spare" or "always". See [`RadarTile`].
    pub radar_tile: String,
    /// How much ground the radar view covers, top to bottom, in kilometres.
    pub radar_span_km: u32,
    /// Which map goes under the radar: "street", "topo" or "dark". Taste, and
    /// asked rather than assumed — see `crate::weather::radar::base_url`.
    pub radar_basemap: String,

    /// Whether the forecast takes cells on the camera wall, and on what terms:
    /// "never", "spare" or "always". See [`ForecastTiles`].
    pub forecast_tiles: String,
    /// How many cells the forecast is given when it is set to always take them.
    /// Ignored otherwise — a spare cell is a spare cell however many there are.
    pub forecast_periods: u32,

    /// Keep the screen awake while Kestrel is fullscreen.
    ///
    /// On by default. A camera wall is something you look at without touching,
    /// which is exactly the case a screen blanker is built to catch.
    pub keep_awake_fullscreen: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            media_dir: default_media_dir().to_string_lossy().into_owned(),
            grid_size: 4,
            picture_fill: PictureFill::default().key().into(),
            live_substream: true,
            virtual_cameras_use_main_stream: true,
            expanded_stream: "main".into(),
            event_poll_seconds: 2.0,
            desktop_notifications: true,
            // People, faces, pets and parcels: the things worth interrupting
            // someone for. Plain motion and vehicles are left off because they
            // fire constantly — a tree in wind, or a car going past on the road.
            notify_kinds: vec![
                "people".to_string(),
                "face".to_string(),
                "dog_cat".to_string(),
                "package".to_string(),
            ],
            audio_enabled: true,
            show_offline_channels: false,
            auto_hide_titles: true,
            title_hide_seconds: 2.0,
            hide_pointer_when_idle: true,
            show_stream_stats: false,
            sidebar_open: true,
            auto_hide_header: false,
            follow_dwell_seconds: 12.0,
            // People and vehicles by default: plain motion fires on wind and
            // shadows, and pets are rarely what you want the view to jump to.
            // Both remain available in preferences.
            follow_kinds: vec![
                crate::api::EventKind::Person.device_key().to_string(),
                crate::api::EventKind::Vehicle.device_key().to_string(),
            ],
            warm_streams: true,
            max_warm_streams: 16,
            collapsed_devices: Vec::new(),

            // Off until asked for. It reaches out to the internet, which is
            // more than a camera client is assumed to want to do.
            weather_enabled: false,
            weather_source: "nws".into(),
            weewx_url: String::new(),
            weewx_allow_self_signed: true,
            weather_zip: String::new(),
            weather_lat: String::new(),
            weather_lon: String::new(),
            weather_metric: false,
            weather_poll_seconds: 300.0,
            weather_bar: true,
            weather_bar_height: 132.0,
            clock_24_hour: None,

            radar_enabled: false,
            radar_tile: "never".into(),
            radar_span_km: 200,
            radar_basemap: "street".into(),

            // A use for cells that were empty, rather than a claim on the wall.
            forecast_tiles: ForecastTiles::Spare.key().into(),
            forecast_periods: 2,

            keep_awake_fullscreen: true,
        }
    }
}

impl Preferences {
    // ---------------------------------------------------------------- weather

    pub fn weather_source(&self) -> crate::weather::poller::Source {
        crate::weather::poller::Source::from_key(&self.weather_source)
    }

    /// Everything a poll needs, lifted out of here so the loop never reads
    /// preferences.
    pub fn weather_settings(&self) -> crate::weather::poller::Settings {
        crate::weather::poller::Settings {
            source: self.weather_source(),
            url: self.weewx_url.trim().to_string(),
            allow_self_signed: self.weewx_allow_self_signed,
            lat: self.weather_lat.trim().to_string(),
            lon: self.weather_lon.trim().to_string(),
            metric: self.weather_metric,
            interval: std::time::Duration::from_secs_f32(self.weather_poll_seconds.max(60.0)),
        }
    }

    /// On, and with somewhere to point it. Switched on with no address would
    /// poll nothing forever, and the strip would sit on "waiting" all night.
    pub fn weather_usable(&self) -> bool {
        self.weather_enabled && self.weather_settings().usable()
    }

    /// The radar is addressed by coordinate like the forecast, so it needs the
    /// ZIP code resolved even when the readings are coming from a weewx server
    /// that has no idea where it is.
    ///
    /// Under the weather switch as well as its own: the radar lives inside the
    /// Weather tab, and radar on with the weather off would be a picture with
    /// no way to reach it.
    /// Where the radar starts, and where "Reset view" returns it to.
    ///
    /// `None` when there is nowhere to point it, which is the same condition
    /// [`Self::radar_usable`] reports — so a caller that has a `Look` knows the
    /// radar is worth running.
    pub fn radar_home(&self) -> Option<crate::weather::radar::Home> {
        if !self.radar_usable() {
            return None;
        }
        Some(crate::weather::radar::Home {
            lat: self.weather_lat.trim().parse().ok()?,
            lon: self.weather_lon.trim().parse().ok()?,
            span_km: self.radar_span_km as f64,
        })
    }

    pub fn picture_fill(&self) -> PictureFill {
        PictureFill::from_key(&self.picture_fill)
    }

    /// Whether the forecast goes on the wall, and on what terms.
    ///
    /// Gated on the weather being usable, the same way [`Self::radar_tile_mode`]
    /// is gated on the radar being: a wall with nothing configured should not
    /// grow cells for a forecast that will never arrive.
    pub fn forecast_tile_mode(&self) -> ForecastTiles {
        if !self.weather_usable() {
            return ForecastTiles::Never;
        }
        match self.forecast_tiles.as_str() {
            "always" => ForecastTiles::Always,
            "never" => ForecastTiles::Never,
            _ => ForecastTiles::Spare,
        }
    }

    /// How many cells the wall must keep for the forecast before it chooses its
    /// shape. Zero unless the forecast is set to always take them.
    pub fn forecast_reserved(&self) -> usize {
        if self.forecast_tile_mode() == ForecastTiles::Always {
            self.forecast_periods.clamp(1, 6) as usize
        } else {
            0
        }
    }

    pub fn radar_tile_mode(&self) -> RadarTile {
        if !self.radar_usable() {
            // One question for a wall to ask, rather than every caller having
            // to remember that the radar can be off, or unlocatable, or both.
            return RadarTile::Never;
        }
        match self.radar_tile.as_str() {
            "always" => RadarTile::Always,
            "spare" => RadarTile::Spare,
            _ => RadarTile::Never,
        }
    }

    pub fn radar_usable(&self) -> bool {
        self.radar_enabled
            && self.weather_enabled
            && !self.weather_lat.is_empty()
            && !self.weather_lon.is_empty()
    }

    /// Whether the clock is written 24-hour, falling back to the locale when
    /// the user has not said.
    pub fn clock_is_24_hour(&self) -> bool {
        self.clock_24_hour
            .unwrap_or_else(crate::weather::locale_prefers_24_hour)
    }

    /// What the weather section should say it is showing: the ZIP code, or the
    /// address, or why there is nothing yet.
    pub fn weather_description(&self) -> String {
        if self.weather_source() == crate::weather::poller::Source::Weewx {
            let url = self.weewx_url.trim();
            if url.is_empty() {
                return "A weewx server on your network".into();
            }
            return url.to_string();
        }

        let zip = self.weather_zip.trim();
        if zip.is_empty() {
            return "The National Weather Service, by ZIP code".into();
        }
        if self.weather_lat.is_empty() {
            return format!("{zip} — not looked up yet");
        }
        format!("weather.gov  ·  {zip}")
    }

    pub fn snapshots_dir(&self) -> PathBuf {
        Path::new(&self.media_dir).join("Snapshots")
    }
    pub fn recordings_dir(&self) -> PathBuf {
        Path::new(&self.media_dir).join("Recordings")
    }
    pub fn downloads_dir(&self) -> PathBuf {
        Path::new(&self.media_dir).join("Downloads")
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    devices: Vec<DeviceConfig>,
    #[serde(default)]
    preferences: Preferences,
}

pub struct ConfigStore {
    pub path: PathBuf,
    pub devices: Vec<DeviceConfig>,
    pub preferences: Preferences,
    keyring_ok: bool,
    /// What was on disk when we loaded, so a save that changes nothing can be
    /// skipped. Two clients share this file; rewriting it unconditionally on
    /// exit would overwrite whatever the other one changed in the meantime.
    loaded: Option<(Vec<String>, Preferences)>,
}

impl ConfigStore {
    pub fn load() -> Self {
        Self::load_from(config_dir().join("config.json"))
    }

    pub fn load_from(path: PathBuf) -> Self {
        let mut store = ConfigStore {
            path,
            devices: Vec::new(),
            preferences: Preferences::default(),
            keyring_ok: keyring_available(),
            loaded: None,
        };

        if let Ok(text) = fs::read_to_string(&store.path) {
            match serde_json::from_str::<OnDisk>(&text) {
                Ok(parsed) => {
                    store.devices = parsed.devices;
                    store.preferences = parsed.preferences;
                }
                Err(err) => warn!("could not read config {}: {err}", store.path.display()),
            }
        }

        // Fill in any password that lives in the keyring rather than the file.
        for device in &mut store.devices {
            if device.password.is_empty() {
                if let Some(secret) = read_secret(device) {
                    device.password = secret;
                }
            }
        }
        store.loaded = Some((store.device_fingerprint(), store.preferences.clone()));
        store
    }

    /// Identity of the device list, ignoring secrets.
    fn device_fingerprint(&self) -> Vec<String> {
        self.devices
            .iter()
            .map(|d| {
                // Virtual cameras belong here too: they are the only device
                // field a whole session can consist of editing, and a
                // fingerprint that ignores them lets `save_if_changed` decide
                // there was nothing to write.
                let views: String = d
                    .virtual_cameras
                    .iter()
                    .map(|v| {
                        format!(
                            "{}:{}:{}:{},{}:{};",
                            v.id, v.name, v.zoom, v.centre.0, v.centre.1, v.hidden
                        )
                    })
                    .collect();
                format!(
                    "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                    d.id,
                    d.host,
                    d.username,
                    d.port,
                    d.rtsp_port,
                    d.https,
                    d.label,
                    d.enabled,
                    views,
                    d.unfollowed_views.join(",")
                )
            })
            .collect()
    }

    /// True when nothing we would write differs from what we read.
    pub fn is_unchanged(&self) -> bool {
        match &self.loaded {
            Some((devices, preferences)) => {
                *devices == self.device_fingerprint() && *preferences == self.preferences
            }
            None => false,
        }
    }

    /// Save only if something actually changed.
    pub fn save_if_changed(&mut self) -> std::io::Result<()> {
        if self.is_unchanged() {
            debug!("config unchanged, not rewriting {}", self.path.display());
            return Ok(());
        }
        self.save()
    }

    pub fn keyring_available(&self) -> bool {
        self.keyring_ok
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut devices = self.devices.clone();
        for device in &mut devices {
            // Hand the secret to the keyring and keep it out of the file when
            // that works; otherwise it stays in the 0600 file, which is honest
            // rather than pretending obfuscation is encryption.
            if self.keyring_ok && !device.password.is_empty() && write_secret(device) {
                device.password.clear();
            }
        }

        let payload = OnDisk {
            version: 1,
            devices,
            preferences: self.preferences.clone(),
        };
        let json = serde_json::to_string_pretty(&payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // Write 0600 from the start, then rename: a crash mid-write must not
        // truncate the real config, and the secret must never briefly exist
        // under looser permissions.
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        self.loaded = Some((self.device_fingerprint(), self.preferences.clone()));
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&DeviceConfig> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn ensure_media_dirs(&self) {
        for dir in [
            self.preferences.snapshots_dir(),
            self.preferences.recordings_dir(),
            self.preferences.downloads_dir(),
        ] {
            let _ = fs::create_dir_all(dir);
        }
    }
}

// ---------------------------------------------------------------- keyring

fn entry(device: &DeviceConfig) -> Option<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &device.keyring_account()).ok()
}

fn read_secret(device: &DeviceConfig) -> Option<String> {
    let entry = entry(device)?;
    match entry.get_password() {
        Ok(secret) => Some(secret),
        Err(keyring::Error::NoEntry) => None,
        Err(err) => {
            debug!("keyring read failed: {err}");
            None
        }
    }
}

fn write_secret(device: &DeviceConfig) -> bool {
    let Some(entry) = entry(device) else { return false };
    match entry.set_password(&device.password) {
        Ok(()) => true,
        Err(err) => {
            warn!("keyring write failed, falling back to the config file: {err}");
            false
        }
    }
}

pub fn forget_secret(device: &DeviceConfig) {
    if let Some(entry) = entry(device) {
        let _ = entry.delete_credential();
    }
}

fn keyring_available() -> bool {
    // Probing costs one round trip to the Secret Service and tells us whether
    // to keep passwords out of the config file.
    match keyring::Entry::new(KEYRING_SERVICE, "__probe__") {
        Ok(entry) => !matches!(entry.get_password(), Err(keyring::Error::PlatformFailure(_))),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_python_clients_file_format() {
        // Exactly what the PyQt client writes.
        let json = r#"{
          "version": 1,
          "devices": [{
            "host": "192.0.2.242", "username": "admin", "port": 80, "rtsp_port": 554,
            "https": false, "label": "", "id": "a4a42c622b38", "enabled": true,
            "cached_name": "nvr0.example", "cached_model": "RLN36", "cached_channels": 36
          }],
          "preferences": {"grid_size": 16, "warm_streams": true, "max_warm_streams": 16}
        }"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert_eq!(parsed.devices.len(), 1);
        let device = &parsed.devices[0];
        assert_eq!(device.id, "a4a42c622b38");
        assert_eq!(device.display_name(), "nvr0.example");
        assert_eq!(device.cached_channels, 36);
        assert_eq!(parsed.preferences.grid_size, 16);
        assert_eq!(parsed.preferences.max_warm_streams, 16);
        // Unspecified preferences keep their defaults.
        assert!(parsed.preferences.live_substream);
    }

    /// People, pets and parcels alert by default; motion and vehicles do not,
    /// because they fire constantly and would train the user to ignore alerts.
    #[test]
    fn notifications_default_to_the_things_worth_interrupting_for() {
        use crate::api::EventKind;
        let prefs = Preferences::default();
        let on = |kind: EventKind| prefs.notify_kinds.iter().any(|k| k == kind.device_key());

        assert!(on(EventKind::Person));
        assert!(on(EventKind::Face));
        assert!(on(EventKind::Pet));
        assert!(on(EventKind::Package));

        assert!(!on(EventKind::Motion));
        assert!(!on(EventKind::Vehicle));
    }

    /// A config written before this setting existed picks up the defaults
    /// rather than an empty list, which would silence alerts on upgrade.
    #[test]
    fn an_older_config_still_notifies() {
        let json = r#"{"devices": [], "preferences": {"desktop_notifications": true}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert_eq!(parsed.preferences.notify_kinds, Preferences::default().notify_kinds);
        assert!(!parsed.preferences.notify_kinds.is_empty());
    }

    /// An empty list is a real choice — no alerts — and must survive a reload
    /// rather than being refilled with the default.
    #[test]
    fn choosing_no_notifications_is_respected() {
        let json = r#"{"devices": [], "preferences": {"notify_kinds": []}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert!(parsed.preferences.notify_kinds.is_empty());
    }

    /// A device saved before vendors existed was a Reolink, and must still be
    /// one after the upgrade rather than silently becoming something else.
    #[test]
    fn hiding_a_channel_is_remembered_and_reversible() {
        let mut device = DeviceConfig::default();
        assert!(!device.is_hidden(3));

        assert!(device.set_hidden(3, true), "hiding is a change");
        assert!(device.is_hidden(3));
        // Hiding it twice is not a change, and must not duplicate the entry.
        assert!(!device.set_hidden(3, true));
        assert_eq!(device.hidden_channels, vec![3]);

        assert!(device.set_hidden(3, false), "showing it again is a change");
        assert!(!device.is_hidden(3));
        assert!(device.hidden_channels.is_empty());
        assert!(!device.set_hidden(3, false));
    }

    /// The file should read the same however the user got there.
    #[test]
    fn hidden_channels_are_kept_in_order() {
        let mut device = DeviceConfig::default();
        for channel in [9, 2, 15, 0] {
            device.set_hidden(channel, true);
        }
        assert_eq!(device.hidden_channels, vec![0, 2, 9, 15]);
    }

    /// A config written before this existed hides nothing.
    #[test]
    fn an_older_config_hides_no_channels() {
        let json = r#"{"devices": [{"host": "nvr", "id": "abc"}], "preferences": {}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert!(parsed.devices[0].hidden_channels.is_empty());
        assert!(!parsed.devices[0].is_hidden(0));
    }

    /// Leaving a camera out of follow-motion is remembered per device, sorted
    /// like the hidden list, and reversible.
    #[test]
    fn a_camera_can_be_left_out_of_following() {
        let mut device = DeviceConfig::default();
        // Nothing said about it means it follows, which is what the app did
        // before there was anything to say.
        assert!(device.follows_motion(3));

        assert!(device.set_follows_motion(3, false), "leaving it out is a change");
        assert!(!device.follows_motion(3));
        assert!(!device.set_follows_motion(3, false), "and only once");
        assert_eq!(device.unfollowed_channels, vec![3]);

        for channel in [9, 0] {
            device.set_follows_motion(channel, false);
        }
        assert_eq!(device.unfollowed_channels, vec![0, 3, 9], "the file reads in order");

        assert!(device.set_follows_motion(3, true));
        assert!(device.follows_motion(3));
        assert_eq!(device.unfollowed_channels, vec![0, 9]);

        // And it is a separate decision from hiding: a camera left out of
        // following is still on the wall.
        assert!(!device.is_hidden(0));
    }

    /// The camera list shows unless somebody says otherwise, and the top bar
    /// stays put unless somebody asks for it to go. Both defaults are the
    /// unsurprising one: a wall that hides its own furniture on first run looks
    /// broken rather than clean.
    #[test]
    fn the_furniture_is_there_until_it_is_asked_to_go() {
        let prefs = Preferences::default();
        assert!(prefs.sidebar_open, "the camera list starts shown");
        assert!(!prefs.auto_hide_header, "and the top bar stays put");

        // A config written before either existed gets the same answers.
        let json = r#"{"devices": [], "preferences": {"grid_size": 4}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert!(parsed.preferences.sidebar_open);
        assert!(!parsed.preferences.auto_hide_header);
    }

    /// The wall stretches pictures to their cells unless told otherwise, and a
    /// config written before the setting existed gets that too — which is a
    /// deliberate change to what those walls looked like, and the reason the
    /// setting is there.
    #[test]
    fn a_picture_fills_its_cell_unless_told_otherwise() {
        assert_eq!(Preferences::default().picture_fill(), PictureFill::Stretch);

        let json = r#"{"devices": [], "preferences": {"grid_size": 4}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert_eq!(parsed.preferences.picture_fill(), PictureFill::Stretch);

        // Every spelling round-trips, and an unknown one is not a black screen.
        for mode in PictureFill::ALL {
            assert_eq!(PictureFill::from_key(mode.key()), mode);
        }
        assert_eq!(PictureFill::from_key("letterbox"), PictureFill::Stretch);
    }

    /// The forecast takes spare cells by default and can be forced to take
    /// cells of its own — but neither happens with no weather configured.
    #[test]
    fn the_forecast_takes_cells_only_when_there_is_weather() {
        let mut prefs = Preferences::default();
        // Nothing configured: no cells, whatever the setting says.
        prefs.forecast_tiles = ForecastTiles::Always.key().into();
        assert_eq!(prefs.forecast_tile_mode(), ForecastTiles::Never);
        assert_eq!(prefs.forecast_reserved(), 0);

        // Weather on and addressable.
        prefs.weather_enabled = true;
        prefs.weather_lat = "42.06".into();
        prefs.weather_lon = "-72.63".into();
        assert_eq!(prefs.forecast_tile_mode(), ForecastTiles::Always);
        assert_eq!(prefs.forecast_reserved(), 2, "the default count");

        // Spare and never reserve nothing — a spare cell is a spare cell.
        for mode in [ForecastTiles::Spare, ForecastTiles::Never] {
            prefs.forecast_tiles = mode.key().into();
            assert_eq!(prefs.forecast_tile_mode(), mode);
            assert_eq!(prefs.forecast_reserved(), 0, "{mode:?}");
        }

        // A count out of range cannot reserve the whole wall or none of it.
        prefs.forecast_tiles = ForecastTiles::Always.key().into();
        prefs.forecast_periods = 0;
        assert_eq!(prefs.forecast_reserved(), 1);
        prefs.forecast_periods = 99;
        assert_eq!(prefs.forecast_reserved(), 6);
    }

    /// Exempting one channel must leave every other channel on the same NVR
    /// alone. If this were ever wrong, one right-click would silently switch
    /// follow-motion off for the whole device — which is exactly what "it
    /// breaks the whole flow" would look like from the outside.
    #[test]
    fn exempting_one_channel_does_not_touch_its_neighbours() {
        let mut nvr = DeviceConfig::default();
        nvr.set_follows_motion(5, false);

        for channel in 0..16u32 {
            assert_eq!(
                nvr.follows_motion(channel),
                channel != 5,
                "channel {channel} after exempting 5"
            );
        }

        // And a second device is untouched by the first.
        let other = DeviceConfig::default();
        for channel in 0..16u32 {
            assert!(other.follows_motion(channel), "another device, channel {channel}");
        }
    }

    /// The two ways a camera can be kept out of follow-motion are independent,
    /// and following has to honour both. Hiding one used to be honoured only
    /// downstream, when the grid was rebuilt from the visible cameras — so a
    /// hidden camera was noted, ranked and selected, and then filtered out,
    /// leaving an empty selection and a wall that never moved.
    #[test]
    fn hiding_and_exempting_are_separate_ways_of_saying_no() {
        let mut device = DeviceConfig::default();
        device.set_hidden(2, true);
        device.set_follows_motion(7, false);

        // Neither did the other's job.
        assert!(device.follows_motion(2), "hiding is not an exemption");
        assert!(!device.is_hidden(7), "an exemption is not hiding");

        // The gate follow-motion applies is both together, and a camera has to
        // clear both to be brought up.
        let may_follow = |ch: u32| !device.is_hidden(ch) && device.follows_motion(ch);
        assert!(!may_follow(2), "a hidden camera must not pull the view to itself");
        assert!(!may_follow(7), "nor an exempted one");
        assert!(may_follow(0) && may_follow(15), "and everything else still does");
    }

    /// A config written before this existed follows everything.
    #[test]
    fn an_older_config_follows_every_camera() {
        let json = r#"{"devices": [{"host": "nvr", "id": "abc"}], "preferences": {}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert!(parsed.devices[0].unfollowed_channels.is_empty());
        assert!(parsed.devices[0].follows_motion(0));
    }

    #[test]
    fn a_device_without_a_vendor_is_reolink() {
        let json = r#"{"devices": [{"host": "192.0.2.242", "id": "abc"}], "preferences": {}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert_eq!(parsed.devices[0].vendor, "reolink");
        assert_eq!(DeviceConfig::default().vendor, "reolink");
    }

    #[test]
    fn a_saved_vendor_survives_a_round_trip() {
        let json = r#"{"devices": [{"host": "nvr", "id": "abc", "vendor": "frigate"}],
                       "preferences": {}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert_eq!(parsed.devices[0].vendor, "frigate");
    }

    /// Picking a system moves the ports to where it listens, but never
    /// overwrites one the user typed themselves.
    #[test]
    fn vendor_defaults_move_the_ports() {
        let mut device = DeviceConfig::default();
        assert_eq!((device.port, device.rtsp_port), (80, 554));

        device.vendor = "frigate".into();
        device.apply_vendor_defaults();
        assert_eq!(device.port, 5000);
        assert_eq!(device.rtsp_port, 8554);

        device.vendor = "unifi".into();
        device.apply_vendor_defaults();
        assert_eq!(device.port, 443);
        assert!(device.https, "Protect is HTTPS");

        // A deliberate port is left alone.
        let mut custom = DeviceConfig {
            port: 8123,
            rtsp_port: 1234,
            vendor: "frigate".into(),
            ..DeviceConfig::default()
        };
        custom.apply_vendor_defaults();
        assert_eq!((custom.port, custom.rtsp_port), (8123, 1234));
    }

    #[test]
    fn follow_defaults_to_people_and_vehicles() {
        let prefs = Preferences::default();
        assert_eq!(prefs.follow_kinds, vec!["people".to_string(), "vehicle".to_string()]);
    }

    /// A config written before this setting existed must pick up the default
    /// rather than ending up with an empty list, which would mean the view
    /// silently never follows anything.
    #[test]
    fn an_older_config_gains_the_default_kinds() {
        let parsed: OnDisk = serde_json::from_str(
            r#"{"preferences": {"follow_motion": true, "follow_dwell_seconds": 12.0}}"#,
        )
        .unwrap();
        assert_eq!(parsed.preferences.follow_kinds, vec!["people", "vehicle"]);
    }

    /// A config written before the weather existed must come back with it off
    /// rather than switched on with nowhere to point.
    #[test]
    fn an_older_config_has_no_weather() {
        let json = r#"{"devices": [], "preferences": {"grid_size": 9}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert!(!parsed.preferences.weather_enabled);
        assert!(!parsed.preferences.weather_usable());
        assert!(!parsed.preferences.radar_usable());
        assert_eq!(parsed.preferences.weather_source(), crate::weather::poller::Source::Nws);
        // And the settings that do have opinions kept theirs.
        assert_eq!(parsed.preferences.grid_size, 9);
        assert!(parsed.preferences.keep_awake_fullscreen);
    }

    /// Switched on with no address is the state that would poll nothing
    /// forever.
    #[test]
    fn the_weather_needs_somewhere_to_read_it_from() {
        let mut prefs = Preferences {
            weather_enabled: true,
            ..Preferences::default()
        };
        assert!(!prefs.weather_usable(), "no ZIP code has been resolved");

        prefs.weather_lat = "42.062".into();
        prefs.weather_lon = "-72.626".into();
        assert!(prefs.weather_usable());

        // Moving to weewx makes the coordinate irrelevant and the address
        // required.
        prefs.weather_source = "weewx".into();
        assert!(!prefs.weather_usable());
        prefs.weewx_url = "http://weewx.local/weather.json".into();
        assert!(prefs.weather_usable());
    }

    /// The radar is addressed by coordinate whichever source the readings come
    /// from — a weewx server has thermometers and no idea where it is.
    #[test]
    fn the_radar_needs_a_coordinate_even_on_a_weewx_wall() {
        let mut prefs = Preferences {
            weather_enabled: true,
            weather_source: "weewx".into(),
            weewx_url: "http://weewx.local/weather.json".into(),
            radar_enabled: true,
            ..Preferences::default()
        };
        assert!(prefs.weather_usable());
        assert!(!prefs.radar_usable(), "no ZIP code means no radar");

        prefs.weather_lat = "42.062".into();
        prefs.weather_lon = "-72.626".into();
        assert!(prefs.radar_usable());

        // And the radar cannot outlive the weather it lives inside.
        prefs.weather_enabled = false;
        assert!(!prefs.radar_usable());
    }

    /// The floor is a minute regardless of what is stored: anything quicker is
    /// asking a weewx archive for the same numbers again.
    #[test]
    fn the_poll_interval_has_a_floor() {
        let prefs = Preferences {
            weather_poll_seconds: 1.0,
            ..Preferences::default()
        };
        assert_eq!(
            prefs.weather_settings().interval,
            std::time::Duration::from_secs(60)
        );
    }

    #[test]
    fn the_weather_settings_survive_a_round_trip() {
        let json = r#"{"devices": [], "preferences": {
            "weather_enabled": true, "weather_source": "weewx",
            "weewx_url": "https://weewx.lan/weather.json",
            "weather_zip": "01001", "weather_lat": "42.062", "weather_lon": "-72.626",
            "weather_metric": true, "radar_enabled": true, "radar_basemap": "dark",
            "radar_span_km": 400, "clock_24_hour": true
        }}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        let prefs = &parsed.preferences;

        assert_eq!(prefs.weather_source(), crate::weather::poller::Source::Weewx);
        assert!(prefs.weather_usable());
        assert!(prefs.radar_usable());
        assert!(prefs.weather_settings().metric);
        assert_eq!(prefs.radar_span_km, 400);
        assert_eq!(prefs.radar_basemap, "dark");
        assert!(prefs.clock_is_24_hour());
        assert_eq!(prefs.weather_description(), "https://weewx.lan/weather.json");
    }

    /// The description is what preferences shows under the source, and each
    /// half-configured state has its own thing to say.
    #[test]
    fn the_description_says_what_is_missing() {
        let mut prefs = Preferences::default();
        assert!(prefs.weather_description().contains("ZIP code"));

        prefs.weather_zip = "01001".into();
        assert!(
            prefs.weather_description().contains("not looked up"),
            "a typed code with no coordinate is not yet usable"
        );

        prefs.weather_lat = "42.062".into();
        prefs.weather_lon = "-72.626".into();
        assert_eq!(prefs.weather_description(), "weather.gov  ·  01001");
    }

    #[test]
    fn keyring_account_matches_the_python_scheme() {
        let device = DeviceConfig {
            id: "a4a42c622b38".into(),
            username: "admin".into(),
            host: "192.0.2.242".into(),
            ..Default::default()
        };
        assert_eq!(device.keyring_account(), "a4a42c622b38:admin@192.0.2.242");
    }

    #[test]
    fn unknown_fields_and_missing_sections_are_tolerated() {
        let parsed: OnDisk =
            serde_json::from_str(r#"{"devices":[{"host":"1.2.3.4","future_field":9}]}"#).unwrap();
        assert_eq!(parsed.devices[0].username, "admin");
        assert_eq!(parsed.devices[0].port, 80);
        assert!(parsed.devices[0].enabled);
    }

    #[test]
    fn an_untouched_config_is_not_rewritten() {
        let dir = std::env::temp_dir().join(format!("kestrel-dirty-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut store = ConfigStore::load_from(path.clone());
        store.keyring_ok = false;
        store.devices.push(DeviceConfig { host: "198.51.100.9".into(), ..Default::default() });
        store.save().unwrap();

        let mut reopened = ConfigStore::load_from(path.clone());
        reopened.keyring_ok = false;
        assert!(reopened.is_unchanged(), "a freshly loaded config must look clean");

        // Another process edits the file while we hold it open.
        let meddled = std::fs::read_to_string(&path).unwrap().replace("\"grid_size\": 4", "\"grid_size\": 16");
        std::fs::write(&path, meddled).unwrap();

        reopened.save_if_changed().unwrap();
        let after = ConfigStore::load_from(path.clone());
        assert_eq!(after.preferences.grid_size, 16, "we clobbered another writer's change");

        // A real change is still written.
        reopened.preferences.grid_size = 9;
        reopened.save_if_changed().unwrap();
        assert_eq!(ConfigStore::load_from(path).preferences.grid_size, 9);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn a_view(id: &str, name: &str, zoom: f32) -> VirtualCamera {
        VirtualCamera {
            id: id.into(),
            channel: 0,
            name: name.into(),
            zoom,
            centre: (0.62, 0.38),
            hidden: false,
        }
    }

    /// A crop is written down beside the device it belongs to and comes back
    /// exactly as it went in — it is the whole point of the feature that the
    /// framing survives a restart.
    #[test]
    fn virtual_cameras_round_trip() {
        let mut device = DeviceConfig { host: "nvr".into(), ..Default::default() };
        device.add_virtual_camera(a_view("gate", "Front gate", 2.5));

        let disk = OnDisk { version: 1, devices: vec![device], preferences: Preferences::default() };
        let text = serde_json::to_string(&disk).unwrap();
        let parsed: OnDisk = serde_json::from_str(&text).expect("should parse");

        let views = &parsed.devices[0].virtual_cameras;
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "Front gate");
        assert_eq!(views[0].zoom, 2.5);
        assert_eq!(views[0].centre, (0.62, 0.38));
        assert_eq!(views[0].channel, 0);
        assert!(!views[0].hidden);
    }

    /// A file written before virtual cameras existed has none, rather than
    /// failing to load.
    #[test]
    fn an_older_config_has_no_virtual_cameras() {
        let json = r#"{"devices": [{"host": "nvr", "id": "abc"}], "preferences": {}}"#;
        let parsed: OnDisk = serde_json::from_str(json).expect("should parse");
        assert!(parsed.devices[0].virtual_cameras.is_empty());
        assert!(parsed.devices[0].unfollowed_views.is_empty());
        assert!(parsed.devices[0].views_of(0).next().is_none());
    }

    /// Adding, editing and removing one, and the exception that goes with it.
    #[test]
    fn virtual_cameras_can_be_edited_and_removed() {
        let mut device = DeviceConfig::default();
        device.add_virtual_camera(a_view("gate", "Front gate", 2.5));
        device.add_virtual_camera(a_view("porch", "Porch", 4.0));
        assert_eq!(device.views_of(0).count(), 2);

        assert!(device.update_virtual_camera(a_view("gate", "The gate", 3.0)));
        assert_eq!(device.virtual_camera("gate").unwrap().name, "The gate");
        assert!(!device.update_virtual_camera(a_view("nope", "Nowhere", 2.0)), "unknown id");

        assert!(device.set_view_hidden("porch", true));
        assert!(!device.set_view_hidden("porch", true), "and only once");
        assert!(device.virtual_camera("porch").unwrap().hidden);

        assert!(device.view_follows_motion("gate"), "nothing said means it follows");
        assert!(device.set_view_follows_motion("gate", false));
        assert!(!device.view_follows_motion("gate"));
        assert_eq!(device.unfollowed_views, vec!["gate".to_string()]);

        // Removing it takes the exception with it, so a later camera given the
        // same id does not inherit a decision nobody made about it.
        assert!(device.remove_virtual_camera("gate"));
        assert!(device.unfollowed_views.is_empty());
        assert!(device.virtual_camera("gate").is_none());
        assert!(!device.remove_virtual_camera("gate"), "and only once");
        assert_eq!(device.views_of(0).count(), 1);
    }

    /// A session whose only change was a virtual camera must still be written.
    /// The fingerprint decides whether anything is saved on exit, so a crop it
    /// cannot see is a crop that quietly does not survive the app closing.
    #[test]
    fn editing_only_a_virtual_camera_still_saves() {
        let dir = std::env::temp_dir().join(format!("kestrel-views-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut store = ConfigStore::load_from(path.clone());
        store.keyring_ok = false;
        store.devices.push(DeviceConfig { host: "198.51.100.9".into(), ..Default::default() });
        store.save().unwrap();

        let mut reopened = ConfigStore::load_from(path.clone());
        reopened.keyring_ok = false;
        assert!(reopened.is_unchanged(), "a freshly loaded config looks clean");

        reopened.devices[0].add_virtual_camera(a_view("gate", "Front gate", 2.5));
        assert!(!reopened.is_unchanged(), "adding one is a change");
        reopened.save_if_changed().unwrap();
        assert_eq!(
            ConfigStore::load_from(path.clone()).devices[0].virtual_cameras.len(),
            1
        );

        // So is renaming one, which touches nothing else on the device.
        let mut again = ConfigStore::load_from(path.clone());
        again.keyring_ok = false;
        assert!(again.is_unchanged());
        again.devices[0].update_virtual_camera(a_view("gate", "The gate", 2.5));
        assert!(!again.is_unchanged(), "renaming one is a change");
        again.save_if_changed().unwrap();
        assert_eq!(
            ConfigStore::load_from(path).devices[0].virtual_cameras[0].name,
            "The gate"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saves_with_restrictive_permissions() {
        let dir = std::env::temp_dir().join(format!("kestrel-cfg-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut store = ConfigStore::load_from(path.clone());
        store.keyring_ok = false; // force the file fallback
        store.devices.push(DeviceConfig {
            host: "198.51.100.9".into(),
            password: "secret".into(),
            ..Default::default()
        });
        store.save().unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must not be world-readable");

        let reloaded = ConfigStore::load_from(path);
        assert_eq!(reloaded.devices.len(), 1);
        assert_eq!(reloaded.devices[0].password, "secret");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Whether the radar takes a cell on the camera wall, and on what terms.
///
/// A choice rather than a switch because both answers are reasonable and
/// neither is safe to assume: `Spare` shows nothing at all on a wall of 4, 6, 9
/// or 16 cameras that tile exactly, which is a setting that looks broken; and
/// `Always` shrinks every camera on the wall to make room for weather, which is
/// not something to do to somebody's cameras without asking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RadarTile {
    /// The Weather tab only.
    Never,
    /// A cell the cameras left over, and nothing if they tile exactly — so no
    /// camera ever changes size for it.
    Spare,
    /// An item on the wall in its own right. Four cameras and the radar lay out
    /// as six cells rather than four: no camera is dropped, but they are all
    /// smaller.
    Always,
}

impl RadarTile {
    pub fn key(self) -> &'static str {
        match self {
            RadarTile::Never => "never",
            RadarTile::Spare => "spare",
            RadarTile::Always => "always",
        }
    }
}

/// Whether the forecast takes cells on the camera wall, and on what terms.
///
/// The same three answers the radar gets, for the same reason — see
/// [`RadarTile`]. `Spare` is the Roku channel's behaviour and the default: the
/// forecast is a use for cells that were empty, not a claim on the wall.
/// `Always` is for a wall that is as much a weather display as a camera one,
/// where waiting for the camera count to leave a cell over is not an answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForecastTiles {
    /// The Weather tab and the strip at the top, and nothing on the wall.
    Never,
    /// Cells the cameras and the radar left over, and nothing when they tile
    /// exactly — so no camera ever changes size for the forecast.
    Spare,
    /// Cells of its own, taken before the shape of the wall is chosen: no
    /// camera is dropped, but they are all a little smaller.
    Always,
}

impl ForecastTiles {
    pub fn key(self) -> &'static str {
        match self {
            ForecastTiles::Never => "never",
            ForecastTiles::Spare => "spare",
            ForecastTiles::Always => "always",
        }
    }
}

/// How a picture sits in the cell it is given.
///
/// A wall is tiled into a square-ish grid and cameras are not square: three
/// columns and two rows of a 16:9 screen make cells of about 1.19:1 for a
/// picture of 1.78:1. Something has to give, and which thing is taste rather
/// than a right answer — so it is asked.
///
/// A choice rather than a switch because all three answers are ones people
/// actually want, and each gives up something the others keep: bars, the shape,
/// or the edges of the frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PictureFill {
    /// Stretched to the cell exactly. Nothing is lost and no space is wasted;
    /// the shape is not kept, so a wide camera in a squarish cell is squashed.
    ///
    /// The default, because black bars on a camera wall are the complaint
    /// people actually have — and the distortion is uniform across the wall,
    /// which the eye settles into in a way it never does with bars.
    #[default]
    Stretch,
    /// The whole frame, its shape kept, with bars where the cell is a different
    /// shape.
    Fit,
    /// The shape kept and scaled until it covers the cell: no bars and no
    /// distortion, at the price of the edges of the frame.
    Fill,
}

impl PictureFill {
    pub fn key(self) -> &'static str {
        match self {
            PictureFill::Stretch => "stretch",
            PictureFill::Fit => "fit",
            PictureFill::Fill => "fill",
        }
    }

    pub fn from_key(key: &str) -> PictureFill {
        match key {
            "fit" => PictureFill::Fit,
            "fill" => PictureFill::Fill,
            _ => PictureFill::Stretch,
        }
    }

    /// What the setting says it does, for the menu and the note under it.
    pub fn label(self) -> &'static str {
        match self {
            PictureFill::Stretch => "Stretch to fill the cell",
            PictureFill::Fit => "Fit the whole picture",
            PictureFill::Fill => "Fill the cell, cropping the edges",
        }
    }

    pub const ALL: [PictureFill; 3] = [PictureFill::Stretch, PictureFill::Fit, PictureFill::Fill];
}

