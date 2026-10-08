//! Device lifecycle.
//!
//! Connecting talks to the network, so it never happens on the UI thread. Each
//! device is connected on its own thread and the result is published into a
//! shared map the UI polls each frame.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use log::{info, warn};

use crate::api::vendor::{self, StreamSource, Vendor};
use crate::api::{Channel, SourceId, StreamType};
use crate::config::{DeviceConfig, VirtualCamera};

#[derive(Default)]
struct State {
    clients: HashMap<String, Arc<dyn Vendor>>,
    connecting: HashSet<String>,
    errors: HashMap<String, String>,
}

#[derive(Clone, Default)]
pub struct DeviceManager {
    state: Arc<Mutex<State>>,
    configs: Arc<Mutex<Vec<DeviceConfig>>>,
    /// Empty NVR slots report as offline channels; hiding them keeps a
    /// 36-channel box with 15 cameras from showing 21 dead tiles.
    pub show_offline: Arc<Mutex<bool>>,
    /// Devices whose hidden channels are currently being revealed, by id.
    ///
    /// A view state rather than a preference: it is how you undo hiding, not a
    /// way to live, so it is deliberately forgotten on restart.
    ///
    /// Per device rather than one flag for the wall, because that is where the
    /// question is asked. "Which of this NVR's inputs did I hide?" is a
    /// question about one box, and answering it by putting every hidden camera
    /// from every device back on the wall is a worse wall than the one you
    /// were trying to fix.
    pub reveal_hidden: Arc<Mutex<HashSet<String>>>,
    /// The telephoto half of a dual-lens camera is reached by zooming on the
    /// wide tile, so it is not a camera in its own right.
    pub show_secondary_lens: Arc<Mutex<bool>>,
}

/// One camera as the wall sees it: a device's channel, and — when this is a
/// virtual camera — the crop of it to show.
///
/// Carrying the parent's whole [`Channel`] rather than a reference to it means
/// a virtual camera answers every capability question the way its parent does,
/// which is correct: the lens that can pan, zoom, floodlight and detect is the
/// same lens either way.
#[derive(Debug, Clone)]
pub struct Source {
    pub id: SourceId,
    pub channel: Channel,
    /// The saved crop, when this is a virtual camera.
    pub view: Option<VirtualCamera>,
}

impl Source {
    pub fn is_virtual(&self) -> bool {
        self.view.is_some()
    }

    /// What the connection behind this camera is keyed by.
    pub fn stream_key(&self) -> (String, u32) {
        self.id.stream_key()
    }
}

impl DeviceManager {
    pub fn set_configs(&self, configs: Vec<DeviceConfig>) {
        *self.configs.lock().unwrap() = configs;
    }

    pub fn configs(&self) -> Vec<DeviceConfig> {
        self.configs.lock().unwrap().clone()
    }

    pub fn connect_all(&self) {
        for config in self.configs() {
            self.connect(config);
        }
    }

    pub fn connect(&self, config: DeviceConfig) {
        if !config.enabled {
            return;
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.connecting.contains(&config.id) {
                return;
            }
            state.connecting.insert(config.id.clone());
            state.errors.remove(&config.id);
        }

        let state = Arc::clone(&self.state);
        let configs = Arc::clone(&self.configs);
        std::thread::Builder::new()
            .name(format!("connect:{}", config.host))
            .spawn(move || {
                // Which module this is depends on the device's vendor, and
                // nothing below this line knows which one it got.
                let mut client = vendor::build(&config);
                match client.connect() {
                    Ok(info) => {
                        info!(
                            "connected to {} ({}, {} channels)",
                            info.name, info.model, info.channel_count
                        );
                        // A previous client for this device would otherwise hold
                        // its session open until the lease expired.
                        let replaced = state.lock().unwrap().clients.remove(&config.id);
                        if let Some(old) = replaced {
                            std::thread::spawn(move || old.logout());
                        }
                        // Cache identity so the UI can label a device before it
                        // answers on the next run.
                        if let Ok(mut list) = configs.lock() {
                            if let Some(stored) = list.iter_mut().find(|c| c.id == config.id) {
                                stored.cached_name = info.name.clone();
                                stored.cached_model = info.model.clone();
                                stored.cached_channels = info.channel_count as u32;
                            }
                        }
                        let mut state = state.lock().unwrap();
                        state.clients.insert(config.id.clone(), Arc::from(client));
                        state.connecting.remove(&config.id);
                    }
                    Err(err) => {
                        warn!("{}: {err}", config.host);
                        let mut state = state.lock().unwrap();
                        state.errors.insert(config.id.clone(), err.to_string());
                        state.connecting.remove(&config.id);
                    }
                }
            })
            .expect("failed to spawn connect thread");
    }

    /// Log out of every device.
    ///
    /// Reolink caps concurrent sessions and does *not* free one when the socket
    /// closes — it holds the token until its hour-long lease expires. An app
    /// that exits without logging out therefore leaks a session per run, and
    /// after enough runs the device answers new logins with "max session" and
    /// locks the user out of their own NVR.
    pub fn shutdown(&self) {
        let clients: Vec<Arc<dyn Vendor>> = {
            let mut state = self.state.lock().unwrap();
            state.clients.drain().map(|(_, client)| client).collect()
        };
        // In parallel, and bounded: quitting must not hang on an unresponsive
        // device, but the sessions are worth a moment to release cleanly.
        let handles: Vec<_> = clients
            .into_iter()
            .map(|client| std::thread::spawn(move || client.logout()))
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
        info!("logged out of all devices");
    }

    /// Release one device's session, e.g. before reconnecting or removing it.
    pub fn disconnect(&self, device_id: &str) {
        let client = self.state.lock().unwrap().clients.remove(device_id);
        if let Some(client) = client {
            std::thread::spawn(move || client.logout());
        }
    }

    pub fn client(&self, device_id: &str) -> Option<Arc<dyn Vendor>> {
        self.state.lock().unwrap().clients.get(device_id).cloned()
    }

    pub fn is_connecting(&self, device_id: &str) -> bool {
        self.state.lock().unwrap().connecting.contains(device_id)
    }

    pub fn error(&self, device_id: &str) -> Option<String> {
        self.state.lock().unwrap().errors.get(device_id).cloned()
    }

    /// Everything the grid can show, in the order it shows it.
    ///
    /// This is the flat list the live view binds to, so an NVR's channels and a
    /// standalone camera appear side by side with no special casing — and each
    /// camera's virtual cameras follow it immediately, so a crop is never
    /// pages away from the picture it was cut out of.
    pub fn sources(&self) -> Vec<Source> {
        let show_offline = *self.show_offline.lock().unwrap();
        let revealing = self.reveal_hidden.lock().unwrap().clone();
        let show_tele = *self.show_secondary_lens.lock().unwrap();
        let configs = self.configs();
        let state = self.state.lock().unwrap();

        let mut out = Vec::new();
        for config in &configs {
            if !config.enabled {
                continue;
            }
            let Some(client) = state.clients.get(&config.id) else { continue };
            for channel in client.channels() {
                if channel.lens == crate::api::Lens::Tele && !show_tele {
                    continue;
                }
                if config.is_hidden(channel.index) && !revealing.contains(&config.id) {
                    continue;
                }
                if !(channel.online || show_offline) {
                    continue;
                }
                // Every reason to leave a camera off the wall is a reason to
                // leave its crops off too, and each is tested once above
                // rather than once per view: a crop of a camera that is not
                // there is a cell of nothing with a name on it.
                out.push(Source {
                    id: SourceId::camera(&config.id, channel.index),
                    channel: channel.clone(),
                    view: None,
                });
                for view in config.views_of(channel.index) {
                    if view.hidden && !revealing.contains(&config.id) {
                        continue;
                    }
                    out.push(Source {
                        id: SourceId::virtual_camera(&config.id, channel.index, &view.id),
                        channel: channel.clone(),
                        view: Some(view.clone()),
                    });
                }
            }
        }
        out
    }

    /// Qualify a camera's name with its device when more than one is
    /// configured.
    ///
    /// A virtual camera answers with its own name rather than its parent's:
    /// naming it is the whole reason it exists, and a wall of "Driveway,
    /// Driveway, Driveway" is the thing it was made to stop.
    pub fn source_label(&self, source: &Source) -> String {
        let configs = self.configs();
        let device_name = configs
            .iter()
            .find(|c| c.id == source.id.device)
            .map(|c| c.display_name().to_string())
            .unwrap_or_else(|| source.id.device.clone());

        let own = match &source.view {
            Some(view) => view.display_name(),
            None => source.channel.display_name(),
        };
        if configs.len() > 1 {
            format!("{device_name} · {own}")
        } else {
            own
        }
    }

    /// The name of one channel, for anything that has no [`Source`] in hand.
    pub fn channel_label(&self, device_id: &str, channel: &Channel) -> String {
        let configs = self.configs();
        let device_name = configs
            .iter()
            .find(|c| c.id == device_id)
            .map(|c| c.display_name().to_string())
            .unwrap_or_else(|| device_id.to_string());

        if configs.len() > 1 {
            format!("{device_name} · {}", channel.display_name())
        } else {
            channel.display_name()
        }
    }

    /// Where a camera's video comes from, asked of whichever system owns it.
    pub fn stream_source(
        &self,
        device_id: &str,
        channel: &Channel,
        stream: StreamType,
    ) -> Option<StreamSource> {
        let client = self.client(device_id)?;
        match client.stream(channel, stream) {
            Ok(source) => Some(source),
            Err(err) => {
                warn!("no stream for {}: {err}", channel.display_name());
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{DeviceInfo, Lens, Recording, StreamType};
    use crate::api::Result;
    use crate::config::VirtualCamera;

    /// A device that answers with the channels it was built with and nothing
    /// else. Enough to ask [`DeviceManager::sources`] what it puts on the wall.
    struct FakeDevice {
        channels: Vec<Channel>,
    }

    impl Vendor for FakeDevice {
        fn vendor_id(&self) -> &'static str {
            "fake"
        }
        fn connect(&mut self) -> Result<DeviceInfo> {
            unreachable!("tests attach an already-connected client")
        }
        fn logout(&self) {}
        fn channels(&self) -> &[Channel] {
            &self.channels
        }
        fn stream(&self, _channel: &Channel, _stream: StreamType) -> Result<StreamSource> {
            Ok(StreamSource::default())
        }
        fn snapshot(&self, _channel: &Channel) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn search_recordings(
            &self,
            _channel: u32,
            _start: chrono::NaiveDateTime,
            _end: chrono::NaiveDateTime,
            _stream: StreamType,
        ) -> Result<Vec<Recording>> {
            Ok(Vec::new())
        }
    }

    fn view(id: &str, channel: u32, name: &str) -> VirtualCamera {
        VirtualCamera {
            id: id.into(),
            channel,
            name: name.into(),
            zoom: 2.5,
            centre: (0.5, 0.5),
            hidden: false,
        }
    }

    /// A manager holding one connected device with `count` channels.
    fn manager_with(config: DeviceConfig, channels: Vec<Channel>) -> DeviceManager {
        let manager = DeviceManager::default();
        manager.set_configs(vec![config.clone()]);
        manager
            .state
            .lock()
            .unwrap()
            .clients
            .insert(config.id.clone(), Arc::new(FakeDevice { channels }));
        manager
    }

    fn named(index: u32, name: &str) -> Channel {
        let mut channel = Channel::new(index);
        channel.name = name.into();
        channel
    }

    /// A crop belongs beside the picture it was cut out of. Anywhere else and
    /// it is pages away from the camera it is a view of, on a wall whose whole
    /// ordering is "the order the sidebar lists them in".
    #[test]
    fn a_virtual_camera_follows_the_camera_it_crops() {
        let mut config = DeviceConfig { host: "nvr".into(), ..Default::default() };
        config.add_virtual_camera(view("gate", 0, "Front gate"));
        config.add_virtual_camera(view("porch", 0, "Porch"));
        config.add_virtual_camera(view("bins", 1, "Bins"));

        let manager = manager_with(
            config,
            vec![named(0, "Driveway"), named(1, "Side"), named(2, "Back")],
        );

        let names: Vec<String> = manager
            .sources()
            .iter()
            .map(|source| manager.source_label(source))
            .collect();
        assert_eq!(
            names,
            vec!["Driveway", "Front gate", "Porch", "Side", "Bins", "Back"]
        );

        // And each answers with its parent's channel, because that is where the
        // one connection behind them both is keyed.
        let sources = manager.sources();
        assert_eq!(sources[1].stream_key(), sources[0].stream_key());
        assert!(sources[1].is_virtual());
        assert!(!sources[0].is_virtual());
    }

    /// Every reason to leave a camera off the wall is a reason to leave its
    /// crops off too. A crop of a camera that is not there is a cell of nothing
    /// with a name on it.
    #[test]
    fn a_camera_that_is_not_shown_takes_its_crops_with_it() {
        let base = || {
            let mut config = DeviceConfig { host: "nvr".into(), ..Default::default() };
            config.add_virtual_camera(view("gate", 0, "Front gate"));
            config
        };

        // Hidden.
        let mut config = base();
        config.set_hidden(0, true);
        let manager = manager_with(config, vec![named(0, "Driveway"), named(1, "Side")]);
        assert_eq!(manager.sources().len(), 1, "only the other camera");

        // Offline, with offline channels not shown.
        let mut offline = named(0, "Driveway");
        offline.online = false;
        let manager = manager_with(base(), vec![offline.clone(), named(1, "Side")]);
        assert_eq!(manager.sources().len(), 1);
        // Shown, and its crop comes back with it.
        *manager.show_offline.lock().unwrap() = true;
        assert_eq!(manager.sources().len(), 3);

        // The telephoto half of a dual-lens camera, which is not a camera in
        // its own right.
        let mut tele = named(0, "Driveway");
        tele.lens = Lens::Tele;
        let manager = manager_with(base(), vec![tele, named(1, "Side")]);
        assert_eq!(manager.sources().len(), 1);

        // The whole device switched off.
        let mut disabled = base();
        disabled.enabled = false;
        let manager = manager_with(disabled, vec![named(0, "Driveway")]);
        assert!(manager.sources().is_empty());
    }

    /// Hiding a crop takes it off the wall without touching the camera, and
    /// revealing puts it back beside the hidden channels.
    #[test]
    fn a_crop_hides_on_its_own() {
        let mut config = DeviceConfig { host: "nvr".into(), ..Default::default() };
        let mut hidden = view("gate", 0, "Front gate");
        hidden.hidden = true;
        config.add_virtual_camera(hidden);
        let device_id = config.id.clone();

        let manager = manager_with(config, vec![named(0, "Driveway")]);
        assert_eq!(manager.sources().len(), 1, "the camera stays");

        manager.reveal_hidden.lock().unwrap().insert(device_id);
        assert_eq!(manager.sources().len(), 2, "revealing brings it back");
    }

    /// Naming one is the whole reason it exists, so it answers with its own
    /// name — and a device qualifies both kinds the same way.
    #[test]
    fn a_virtual_camera_is_called_what_it_was_named() {
        let mut config = DeviceConfig { host: "nvr".into(), ..Default::default() };
        config.add_virtual_camera(view("gate", 0, "Front gate"));
        config.label = "Front NVR".into();
        let manager = manager_with(config.clone(), vec![named(0, "Driveway")]);

        let sources = manager.sources();
        assert_eq!(manager.source_label(&sources[1]), "Front gate");

        // With a second device configured, both kinds pick up the device name.
        let mut other = DeviceConfig { host: "other".into(), ..Default::default() };
        other.label = "Shed".into();
        manager.set_configs(vec![config, other]);
        let sources = manager.sources();
        assert_eq!(manager.source_label(&sources[0]), "Front NVR · Driveway");
        assert_eq!(manager.source_label(&sources[1]), "Front NVR · Front gate");
    }

    /// An unnamed crop still has something to put in its name strip.
    #[test]
    fn an_unnamed_crop_says_what_it_is() {
        let mut config = DeviceConfig { host: "nvr".into(), ..Default::default() };
        config.add_virtual_camera(view("gate", 0, ""));
        let manager = manager_with(config, vec![named(0, "Driveway")]);
        assert_eq!(manager.source_label(&manager.sources()[1]), "2.5x view");
    }
}
