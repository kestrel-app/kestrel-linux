//! Desktop notifications.
//!
//! Sent straight to the freedesktop notification service. The PyQt client went
//! through the Qt tray for this, which silently dropped every notification on a
//! stock GNOME session because there is no StatusNotifierWatcher — so this port
//! talks to the bus directly from the start.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

use log::{debug, info};

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

fn gdbus() -> Option<&'static str> {
    static FOUND: OnceLock<Option<String>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            which("gdbus").map(|path| path.to_string_lossy().into_owned())
        })
        .as_deref()
}

fn which(binary: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(binary))
            .find(|candidate| candidate.is_file())
    })
}

/// True when there is no desktop to notify.
///
/// Test harnesses drive the real app with synthetic events; without this they
/// raise genuine notifications in the developer's session. That happened with
/// the Python client and took a while to trace back.
fn suppressed() -> bool {
    if std::env::var_os("KESTREL_NO_NOTIFICATIONS").is_some() {
        return true;
    }
    std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none()
}

pub struct Notifier {
    enabled: bool,
}

impl Notifier {
    pub fn new(enabled: bool) -> Self {
        let usable = enabled && !suppressed() && gdbus().is_some();
        info!(
            "desktop notifications: {}",
            if usable { "enabled" } else { "off" }
        );
        Notifier { enabled: usable }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled && !suppressed() && gdbus().is_some();
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Fire and forget: the UI thread must not wait on a bus round trip.
    pub fn notify(&self, title: &str, body: &str) {
        if !self.enabled {
            return;
        }
        let Some(gdbus) = gdbus() else { return };
        let result = Command::new(gdbus)
            .args([
                "call", "--session",
                "--dest", DEST,
                "--object-path", PATH,
                "--method", &format!("{DEST}.Notify"),
                "Kestrel",
                "0",
                "kestrel",
                title,
                body,
                "[]",
                "{}",
                "4000",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Err(err) = result {
            debug!("notification failed: {err}");
        }
    }
}
