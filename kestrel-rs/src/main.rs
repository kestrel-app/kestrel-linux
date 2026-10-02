//! Kestrel — live camera wall and PTZ control for your NVR.
//!
//! During the port this binary is a connectivity probe, so the API layer can be
//! verified against real hardware before any UI exists.

mod api;
mod config;
mod events;
mod manager;
mod notify;
mod power;
mod ui;
mod video;
mod weather;

use api::vendor::StreamSource;

use std::process::ExitCode;

use api::{redact_rtsp, ReolinkClient, StreamType};
use video::{StreamState, StreamWorker};

/// Compare a cold connect against adopting a warm stream, the way the live grid
/// does when a camera comes on screen.
fn benchmark_streams(client: &ReolinkClient, channel: u32) {
    use std::time::{Duration, Instant};

    let url = client.rtsp_url(channel, StreamType::Sub);
    println!("\n  Stream benchmark on channel {channel}");
    println!("  {}", redact_rtsp(&url));

    let wait_for_frame = |worker: &StreamWorker, budget: Duration| -> Option<f32> {
        let started = Instant::now();
        while started.elapsed() < budget {
            if worker.latest_frame().is_some() {
                return Some(started.elapsed().as_secs_f32());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    };

    // --- cold: what a reconnect costs -------------------------------------
    let cold = StreamWorker::start(StreamSource::new(url.clone()), "cold", false);
    let cold_time = wait_for_frame(&cold, Duration::from_secs(25));
    let stats = cold.stats();
    match cold_time {
        Some(t) => println!(
            "    cold connect      first frame {t:.2}s   {}x{} {}",
            stats.width, stats.height, stats.codec
        ),
        None => println!("    cold connect      TIMED OUT"),
    }
    drop(cold);

    // --- warm: connected in advance, then adopted --------------------------
    let warm = StreamWorker::start(StreamSource::new(url), "warm", true);
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline && warm.state().0 != StreamState::Playing {
        std::thread::sleep(Duration::from_millis(50));
    }
    // Let it collect a keyframe, as the warm pool would.
    std::thread::sleep(Duration::from_secs(6));
    assert!(warm.latest_frame().is_none(), "a warm stream must not decode");

    let started = Instant::now();
    warm.go_hot();
    let warm_time = wait_for_frame(&warm, Duration::from_secs(15));
    match warm_time {
        Some(t) => println!("    adopt warm stream first frame {t:.2}s", t = t),
        None => println!("    adopt warm stream TIMED OUT"),
    }
    let _ = started;

    if let (Some(cold), Some(warm)) = (cold_time, warm_time) {
        println!("    improvement       {:.2}s faster", cold - warm);
    }
}

fn launch_gui() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 880.0])
            .with_min_inner_size([940.0, 600.0])
            .with_title("Kestrel")
            .with_app_id("kestrel")
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Kestrel",
        options,
        Box::new(|cc| Ok(Box::new(ui::KestrelApp::new(cc)))),
    )
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: kestrel <host> [--user NAME] [--password PASS] [--port N] \
         [--rtsp-port N] [--https] [--search]\n\n\
         If --password is omitted, the KESTREL_PASSWORD environment variable is used."
    );
    ExitCode::from(2)
}

/// The window icon, embedded so a single binary needs no icon file beside it.
///
/// A failure here is not worth refusing to start over: the window simply gets
/// the desktop's default icon.
fn app_icon() -> std::sync::Arc<eframe::egui::IconData> {
    const PNG: &[u8] = include_bytes!("../assets/icon-256.png");
    let rgba = image::load_from_memory(PNG)
        .map(|img| img.to_rgba8())
        .unwrap_or_else(|_| image::RgbaImage::new(1, 1));
    let (width, height) = rgba.dimensions();
    std::sync::Arc::new(eframe::egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

fn main() -> ExitCode {
    // Third-party crates are noisy at info (zbus in particular logs every D-Bus
    // frame), so only our own logging is on by default. RUST_LOG overrides it.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,kestrel=info"),
    )
    .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("--version") | Some("-V")) {
        println!(
            "Kestrel {} (built {})",
            env!("CARGO_PKG_VERSION"),
            env!("KESTREL_BUILD_DATE")
        );
        return ExitCode::SUCCESS;
    }

    if args.first().map(String::as_str) == Some("--check-config") {
        let store = config::ConfigStore::load();
        println!("config      {}", store.path.display());
        println!("keyring     {}", if store.keyring_available() { "available" } else { "unavailable (file fallback)" });
        println!("media dir   {}", store.preferences.media_dir);
        println!("grid        {}-up, warm streams {} (max {})",
                 store.preferences.grid_size,
                 store.preferences.warm_streams,
                 store.preferences.max_warm_streams);
        println!("devices     {}", store.devices.len());
        for d in &store.devices {
            println!("  {:<14} {}:{}  user={}  password={}  channels={}",
                     d.display_name(), d.host, d.port, d.username,
                     if d.password.is_empty() { "MISSING" } else { "loaded" },
                     d.cached_channels);
        }
        return ExitCode::SUCCESS;
    }

    if args.is_empty() {
        return match launch_gui() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("could not start the interface: {err}");
                ExitCode::FAILURE
            }
        };
    }

    if args[0].starts_with("--") {
        return usage();
    }

    let host = args[0].clone();
    let mut user = "admin".to_string();
    let mut password = std::env::var("KESTREL_PASSWORD").unwrap_or_default();
    let (mut port, mut rtsp_port, mut https, mut search) = (80u16, 554u16, false, false);
    let mut stream_bench = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--user" => { i += 1; user = args.get(i).cloned().unwrap_or(user); }
            "--password" => { i += 1; password = args.get(i).cloned().unwrap_or(password); }
            "--port" => { i += 1; port = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(port); }
            "--rtsp-port" => { i += 1; rtsp_port = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(rtsp_port); }
            "--https" => https = true,
            "--search" => search = true,
            "--stream" => stream_bench = true,
            other => { eprintln!("unknown option {other}"); return usage(); }
        }
        i += 1;
    }

    let mut client = ReolinkClient::with_options(host, user, password, port, https, rtsp_port, 30, 4, https);
    let info = match client.connect() {
        Ok(info) => info,
        Err(err) => {
            eprintln!("FAILED: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!("\n  Connected to {}", if info.name.is_empty() { "(unnamed)" } else { &info.name });
    println!("  Model      {}", info.model);
    println!("  Firmware   {}", info.firmware);
    println!("  Kind       {:?}  ({} channel(s), {} disk(s))", info.kind(), info.channel_count, info.hdd_count);

    let online: Vec<_> = client.channels.iter().filter(|c| c.online).collect();
    println!("\n  Channels ({} total, {} online):", client.channels.len(), online.len());
    for ch in client.channels.iter().filter(|c| c.online) {
        let mut caps = Vec::new();
        if ch.ptz_supported { caps.push("PTZ".to_string()); }
        if ch.ptz_presets_supported { caps.push("presets".to_string()); }
        if !ch.ai_types.is_empty() { caps.push(format!("AI:{}", ch.ai_types.join(","))); }
        println!(
            "    [{:>2}] {:<22} {}/{:<5} {}",
            ch.index, ch.display_name(), ch.main_codec, ch.sub_codec,
            if caps.is_empty() { "-".into() } else { caps.join(" ") }
        );
    }

    if let Some(first) = client.channels.iter().find(|c| c.online) {
        println!("\n  RTSP  {}", redact_rtsp(&client.rtsp_url(first.index, StreamType::Sub)));
        match client.snapshot(first.index) {
            Ok(bytes) => println!(
                "  Snapshot   {} bytes, valid JPEG: {}",
                bytes.len(),
                bytes.starts_with(&[0xFF, 0xD8])
            ),
            Err(err) => println!("  Snapshot   FAILED: {err}"),
        }
        match client.ptz_presets(first.index) {
            Ok(presets) => println!("  Presets    {presets:?}"),
            Err(err) => println!("  Presets    {err}"),
        }
    }

    if search {
        let today = chrono::Local::now().date_naive();
        let (start, end) = (today.and_hms_opt(0, 0, 0).unwrap(), today.and_hms_opt(23, 59, 59).unwrap());
        for ch in client.channels.iter().filter(|c| c.online).take(2) {
            match client.search_recordings(ch.index, start, end, StreamType::Main) {
                Ok(clips) => {
                    let fetchable = clips.iter().filter(|c| c.is_fetchable()).count();
                    println!("\n  Recordings ch{} today: {} ({} fetchable)", ch.index, clips.len(), fetchable);
                    for clip in clips.iter().take(3) {
                        println!("    {}  {:.2} GB  name={:?}", clip.label(), clip.size as f64 / 1e9, clip.name);
                    }
                }
                Err(err) => println!("\n  Recordings ch{}: FAILED {err}", ch.index),
            }
        }
    }

    if stream_bench {
        if let Some(first) = client.channels.iter().find(|c| c.online) {
            benchmark_streams(&client, first.index);
        }
    }

    client.logout();
    println!();
    ExitCode::SUCCESS
}
