//! Keeping the screen awake.
//!
//! A camera wall is something you look at without touching, which is exactly
//! the case a screen blanker is built to catch: half an hour of a live picture
//! with nobody moving the mouse and the desktop decides the machine is idle,
//! blanks the display and — depending on the power settings — suspends. So
//! while Kestrel is fullscreen it asks the desktop not to.
//!
//! There is no single call for this on Linux, so two are made:
//!
//!   * **`org.freedesktop.ScreenSaver`** on the session bus. This is what a
//!     browser or a video player uses, and what GNOME, KDE, Xfce, Cinnamon and
//!     MATE all listen to. It stops the screen blanking and, on the desktops
//!     that tie the two together, the idle suspend with it.
//!   * **logind's `Inhibit`** on the system bus, taking an `idle` lock. That
//!     covers the case the session bus does not: a machine whose suspend timer
//!     is systemd's rather than the desktop's.
//!
//! Both are *held* rather than called. A screensaver inhibit is tied to the
//! caller's connection and a logind lock is a file descriptor, so both end the
//! moment the holder goes away — which is the right behaviour (a crash must not
//! leave the machine unable to sleep) and the reason this cannot be done by
//! shelling out to `gdbus` the way the notifications are. `gdbus call` returns
//! a cookie and then exits, taking the inhibit with it.
//!
//! Everything happens on a thread of its own. The UI must not wait on a bus
//! round trip — the same rule [`crate::notify`] follows — and a session bus
//! that has stopped answering would otherwise freeze the window at the moment
//! it went fullscreen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use log::{debug, info};

/// Session-bus services that understand `Inhibit(app, reason) -> cookie`, in
/// the order they are worth trying.
///
/// The interface is the same shape in all three, which is why one loop covers
/// them: whichever answers first is the one this desktop implements. The second
/// entry is the same service at the path GNOME has historically also exported,
/// and the third is the older power-management interface that Xfce and KDE
/// still carry.
const SESSION_TARGETS: [(&str, &str, &str); 3] = [
    (
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
        "org.freedesktop.ScreenSaver",
    ),
    (
        "org.freedesktop.ScreenSaver",
        "/ScreenSaver",
        "org.freedesktop.ScreenSaver",
    ),
    (
        "org.freedesktop.PowerManagement",
        "/org/freedesktop/PowerManagement/Inhibit",
        "org.freedesktop.PowerManagement.Inhibit",
    ),
];

/// True when there is no desktop to ask.
///
/// Test harnesses drive the real app, and one that quietly stopped a build
/// machine from sleeping would be a nuisance to trace back — the same reason
/// notifications carry an override.
fn suppressed() -> bool {
    if std::env::var_os("KESTREL_NO_INHIBIT").is_some() {
        return true;
    }
    std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none()
}

enum Command {
    Hold(String),
    Release,
}

/// What is currently being held, so it can be given back.
#[derive(Default)]
struct Held {
    /// The session-bus connection and the cookie it was given. Kept together
    /// because the cookie is only meaningful to the connection that owns it.
    session: Option<(zbus::blocking::Connection, &'static str, &'static str, &'static str, u32)>,
    /// logind's lock, which is the descriptor itself — dropping it releases.
    logind: Option<zbus::zvariant::OwnedFd>,
}

impl Held {
    fn is_empty(&self) -> bool {
        self.session.is_none() && self.logind.is_none()
    }
}

pub struct Inhibitor {
    commands: Option<mpsc::Sender<Command>>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Whether anything is actually being held, for the preferences pane to
    /// report. Asking for an inhibit and getting one are different things.
    active: Arc<AtomicBool>,
    /// What we last asked for, so the same request is not sent twice a frame.
    wanted: bool,
}

impl Inhibitor {
    pub fn new() -> Inhibitor {
        if suppressed() {
            debug!("sleep inhibition: no desktop session, not attempting it");
            return Inhibitor {
                commands: None,
                handle: None,
                active: Arc::new(AtomicBool::new(false)),
                wanted: false,
            };
        }

        let (commands, requests) = mpsc::channel();
        let active = Arc::new(AtomicBool::new(false));
        let handle = std::thread::Builder::new()
            .name("inhibit".into())
            .spawn({
                let active = Arc::clone(&active);
                move || run(requests, active)
            })
            .ok();

        Inhibitor {
            commands: Some(commands),
            handle,
            active,
            wanted: false,
        }
    }

    /// Ask for the screen to be kept awake, or stop asking.
    ///
    /// Cheap to call every frame: the request only crosses to the bus thread
    /// when it changes.
    pub fn set(&mut self, on: bool, reason: &str) {
        if on == self.wanted {
            return;
        }
        self.wanted = on;
        let Some(commands) = &self.commands else { return };
        let message = if on {
            Command::Hold(reason.to_string())
        } else {
            Command::Release
        };
        let _ = commands.send(message);
    }

    /// Whether the desktop actually granted it. False both when nothing has
    /// been asked for and when nothing on this machine answered.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

impl Default for Inhibitor {
    fn default() -> Self {
        Inhibitor::new()
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        // Dropping the sender ends the loop, which drops the connection and the
        // descriptor — so the lock would go anyway. Releasing first is tidier
        // and makes the log read in the order things happened.
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(Command::Release);
            drop(commands);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(requests: mpsc::Receiver<Command>, active: Arc<AtomicBool>) {
    let mut held = Held::default();

    while let Ok(command) = requests.recv() {
        match command {
            Command::Hold(reason) => {
                if !held.is_empty() {
                    continue;
                }
                held = acquire(&reason);
                active.store(!held.is_empty(), Ordering::Relaxed);
                if held.is_empty() {
                    info!("nothing on this desktop answered a request to stay awake");
                } else {
                    info!(
                        "keeping the screen awake ({})",
                        match (&held.session, &held.logind) {
                            (Some((_, dest, _, _, _)), Some(_)) => format!("{dest} and logind"),
                            (Some((_, dest, _, _, _)), None) => (*dest).to_string(),
                            _ => "logind".to_string(),
                        }
                    );
                }
            }
            Command::Release => {
                if held.is_empty() {
                    continue;
                }
                release(&mut held);
                active.store(false, Ordering::Relaxed);
                info!("letting the screen sleep again");
            }
        }
    }

    release(&mut held);
    active.store(false, Ordering::Relaxed);
}

fn acquire(reason: &str) -> Held {
    let mut held = Held::default();

    // The session bus first: it is the one that stops the screen blanking,
    // which is the visible half of the problem.
    match zbus::blocking::Connection::session() {
        Ok(connection) => {
            for (dest, path, interface) in SESSION_TARGETS {
                let reply = connection.call_method(
                    Some(dest),
                    path,
                    Some(interface),
                    "Inhibit",
                    &("Kestrel", reason),
                );
                match reply.and_then(|message| message.body().deserialize::<u32>()) {
                    Ok(cookie) => {
                        held.session = Some((connection, dest, path, interface, cookie));
                        break;
                    }
                    Err(err) => debug!("{dest} did not take an inhibit: {err}"),
                }
            }
        }
        Err(err) => debug!("no session bus: {err}"),
    }

    // And logind, for the machine whose suspend timer is systemd's rather than
    // the desktop's. `idle` rather than `sleep`: this should stop the machine
    // deciding on its own that nothing is happening, and must not stop somebody
    // closing the lid.
    match zbus::blocking::Connection::system() {
        Ok(connection) => {
            let reply = connection.call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "Inhibit",
                &("idle", "Kestrel", reason, "block"),
            );
            match reply.and_then(|message| message.body().deserialize::<zbus::zvariant::OwnedFd>()) {
                Ok(lock) => held.logind = Some(lock),
                Err(err) => debug!("logind did not take an idle lock: {err}"),
            }
        }
        Err(err) => debug!("no system bus: {err}"),
    }

    held
}

fn release(held: &mut Held) {
    if let Some((connection, dest, path, interface, cookie)) = held.session.take() {
        let result = connection.call_method(Some(dest), path, Some(interface), "UnInhibit", &(cookie,));
        if let Err(err) = result {
            debug!("{dest} refused to release the inhibit: {err}");
        }
    }
    // Closing the descriptor is the whole of releasing a logind lock.
    held.logind = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The override exists so a test harness driving the real app does not stop
    /// the machine it is running on from sleeping.
    #[test]
    fn the_override_suppresses_it_outright() {
        // Set for the duration of this test only; the other tests here do not
        // read the environment.
        let previous = std::env::var_os("KESTREL_NO_INHIBIT");
        std::env::set_var("KESTREL_NO_INHIBIT", "1");
        assert!(suppressed());

        let mut inhibitor = Inhibitor::new();
        inhibitor.set(true, "test");
        assert!(!inhibitor.is_active(), "nothing should have been asked for");

        match previous {
            Some(value) => std::env::set_var("KESTREL_NO_INHIBIT", value),
            None => std::env::remove_var("KESTREL_NO_INHIBIT"),
        }
    }

    /// Called every frame from the UI, so repeats must not reach the bus.
    #[test]
    fn asking_twice_is_asking_once() {
        let (commands, requests) = mpsc::channel();
        let mut inhibitor = Inhibitor {
            commands: Some(commands),
            handle: None,
            active: Arc::new(AtomicBool::new(false)),
            wanted: false,
        };

        inhibitor.set(true, "fullscreen");
        inhibitor.set(true, "fullscreen");
        inhibitor.set(true, "fullscreen");
        inhibitor.set(false, "");
        inhibitor.set(false, "");
        inhibitor.set(true, "fullscreen");

        // Releasing state that was never asked for is also a no-op, which is
        // what makes the first call after startup free.
        drop(inhibitor);

        let sent: Vec<&'static str> = requests
            .iter()
            .map(|command| match command {
                Command::Hold(_) => "hold",
                Command::Release => "release",
            })
            .collect();
        // Three transitions, plus the release Drop sends.
        assert_eq!(sent, vec!["hold", "release", "hold", "release"]);
    }

    /// Against a real bus. Ignored by default, since it needs one:
    ///
    ///   dbus-run-session -- env DISPLAY=:0 \
    ///     cargo test -- --ignored --nocapture live_inhibit
    ///
    /// Run it with `tests/fake-screensaver.py` in the background to exercise
    /// the session-bus half without a desktop; run it on a real desktop session
    /// to check that this machine's screensaver takes the inhibit.
    #[test]
    #[ignore]
    fn live_inhibit() {
        assert!(
            !suppressed(),
            "set DISPLAY or WAYLAND_DISPLAY, and unset KESTREL_NO_INHIBIT"
        );

        let mut inhibitor = Inhibitor::new();
        inhibitor.set(true, "Kestrel test");

        // The bus round trip happens on the inhibit thread, so this is the one
        // place that has to wait for it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !inhibitor.is_active() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        println!("  granted: {}", inhibitor.is_active());
        assert!(
            inhibitor.is_active(),
            "nothing on this bus took the inhibit — run with RUST_LOG=kestrel=debug to see \
             what each service said"
        );

        inhibitor.set(false, "");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && inhibitor.is_active() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!inhibitor.is_active(), "the inhibit was not released");
    }

    /// Held state is what decides whether a release is worth sending, so the
    /// emptiness test has to be right about both halves.
    #[test]
    fn holding_either_lock_counts_as_holding_one() {
        let mut held = Held::default();
        assert!(held.is_empty());

        // A logind lock on its own is a real inhibit, even with no session bus.
        let descriptor: std::os::fd::OwnedFd =
            std::fs::File::open("/dev/null").expect("/dev/null").into();
        held.logind = Some(zbus::zvariant::OwnedFd::from(descriptor));
        assert!(!held.is_empty());

        release(&mut held);
        assert!(held.is_empty(), "releasing must clear it");
    }
}
