# Kestrel

Live camera wall and PTZ control for your NVR — a Linux desktop client for
Reolink cameras and NVRs, covering the parts of the Windows client that matter
day to day. Talks to devices over their local HTTP CGI API and RTSP.

Written in Rust with [egui](https://github.com/emilk/egui), decoding through a
bundled LGPL build of ffmpeg. It ships as a single binary with its libraries
beside it: no interpreter, no system ffmpeg, no runtime install step.

Shares its name and palette with the Roku channel in `../kestrel-roku`, so the
two clients read as one product.

The source lives in [`kestrel-rs/`](kestrel-rs/); its README covers the build
and the licensing rationale in more detail.

## Install

Unpack the archive anywhere and run it:

```sh
tar xzf kestrel-0.1.0-x86_64.tar.gz
cd kestrel-0.1.0-x86_64
./kestrel

./install.sh            # optional: adds it to the application launcher
```

There is also an AppImage. On a machine without libfuse2, run it with
`--appimage-extract-and-run`.

Nothing is required on the target machine and nothing is installed system-wide.
Binaries link against **glibc 2.28**, so they run on anything from Debian 10 and
RHEL 8 forward. Sound is the one soft dependency: `libasound.so.2` is opened at
runtime if present, and audio reports itself unavailable if it is not.

### Building from source

```sh
cd kestrel-rs
./vendor/build-ffmpeg.sh    # ~2 minutes, cached afterwards
./build.sh test
./package.sh                # -> dist/kestrel-<version>-x86_64/ and a .tar.gz
./appimage.sh               # -> dist/Kestrel-<version>-x86_64.AppImage
```

Release builds link through a vendored Zig toolchain to pin the glibc floor, and
`package.sh` refuses to produce an archive that needs anything newer than the
target or that resolves its libraries from the build machine.

## Adding a device

Press **+** in the Cameras sidebar. Enter the address, username and password,
then use **Test connection** to confirm before saving — it reports the model,
firmware and channel list it found.

Point it at an NVR and all its channels appear as separate tiles. Standalone
cameras and NVR channels sit side by side in one grid.

## Features

- **Live view** — paged grid (1, 4, 6, 9 or 16 up) across every camera and NVR
  channel at once. Sub streams in the grid, main stream when a camera is
  expanded. Scroll to zoom, centred on the pointer; drag to pan.
- **Warm streams** — off-screen cameras stay connected and demux without
  decoding, so bringing one on screen takes about 0.07s instead of six seconds,
  for roughly 0.8% of a core each. Configurable, and capped.
- **Expand and fullscreen** — double-click a camera to fill the viewing pane;
  `F11` then takes the whole screen. `Esc` unwinds one level at a time. An
  expanded camera defaults to the best quality the device offers, with a
  selector in the header to drop to the sub stream.
- **PTZ** — hold-to-move pan/tilt with adjustable speed, and recall of presets
  stored on the camera, re-read each time you select it. The centre of the pad
  returns to the home position; **Calibrate** runs the pan/tilt self-calibration
  sweep (it asks first — the camera is unresponsive while it runs). The pane
  appears only for cameras that can actually move.
- **Floodlight** — on/off and brightness from the toolbar, for cameras that
  report one.
- **Dual-lens cameras** (TrackMix and similar) appear as a single camera rather
  than two, and zooming switches between the wide and telephoto lenses.
- **Virtual cameras** — a camera pointed along a driveway sees the gate, the
  porch and the road at once. Right-click a tile and **Make a virtual camera**:
  a box appears on the picture, which you drag to place and size by its corners
  — scroll works too — and double-click or `Enter` keeps it, `Esc` throws it
  away. The tile shows the whole camera while you choose, since you cannot pick
  part of a picture you cannot see, and a tile you had already zoomed opens with
  the box around exactly that. What you keep goes on the wall beside the camera
  it came from as a camera in its own right, with its own name, and pages,
  hides, expands and badges detections like any other. It costs no extra
  connection and no extra decode — a camera and every crop of it read the same
  stream, promoted to the main stream while a crop is on screen so there is
  something to magnify.
- **Playback** — calendar of days with footage, per-day clip list, streamed
  playback and download to disk.
- **Snapshots and recording** — full-resolution stills pulled from the device,
  and on-demand recording of the live stream to MP4 without re-encoding.
- **Audio** — the selected camera's microphone, on by default, one camera at a
  time.
- **Events** — polls motion and AI detection (person, vehicle, pet, face,
  package), badges the tile with an icon per type, and raises desktop alerts for
  the types you choose.
- **Follow motion** — an optional mode that points the live view at whatever is
  detecting, holding each camera for a configurable dwell so the view does not
  flick away the moment someone stops moving.
- **Weather** — off by default. A strip of conditions above the grid and a
  **Weather** tab with the full reading: current conditions, the readings the
  strip has no room for, the forecast period by period with its narrative, and
  watches and warnings. It reads either a **weewx** server on your own network
  or the **National Weather Service**, from a ZIP code and nothing else. Both
  fill the same model, so nothing on screen depends on which one you use, and a
  station reporting in metric displays metric with no setting to find.
- **Radar** — the National Weather Service *enhanced* radar inside the Weather
  tab: the seamless national mosaic rather than a single station's cone, over a
  street, terrain or dark map, with place names on top. Twenty minutes of sweeps
  at two-minute steps, played as a loop that holds on the current one. Only
  fetched while you are looking at it. Watch and warning polygons are drawn from
  the geometry rather than fetched as pictures, so their borders stay sharp at
  any zoom and clicking one names it without asking anyone.
- **Keep the screen awake** — a wall is something you watch without touching,
  which is exactly what a screen blanker treats as an idle machine. While
  fullscreen, Kestrel asks the desktop not to blank or sleep, and stops asking
  the moment you leave. On by default.

## Interface

- **Header** — wordmark, a **Live / Playback** switch (with **Weather** beside
  them once the weather is switched on), the controls that act on the selected
  camera (audio, snapshot, record, floodlight), then grid layout and page
  controls. Everything else lives behind the **⋯** overflow button.
- **Sidebar** — devices and their channels, with a status dot each. Click a
  channel to jump to it; double-click a device to return to the full grid; use
  the chevron to fold an NVR's channel list away; right-click a device to edit,
  reconnect or remove.
- **Right rail** — camera controls, shown only when the selected camera has any.
- **Toasts** — transient messages appear over the video rather than in a status
  bar.

Camera names hide after a couple of seconds of a still mouse and return on any
movement; detection badges stay visible either way. That behaviour, the delay,
and whether the pointer hides with them are all configurable.

Channels the device reports as offline — usually unpopulated NVR slots — are
hidden by default so an 8-channel box with 4 cameras does not show 4 dead tiles.

## Shortcuts

| Key | Action |
|---|---|
| `Ctrl+1` / `Ctrl+2` | Live / Playback |
| `Ctrl+3` | Weather (when it is switched on) |
| `Ctrl+S` | Snapshot the selected camera |
| `Ctrl+R` | Toggle recording on the selected camera |
| `←` / `→` | Page through the grid, or step camera to camera when expanded |
| `F11` | Fullscreen |
| `Esc` | Put away a virtual camera's framing box, then leave fullscreen, then leave the expanded view |

Double-click a tile to expand it; double-click again to go back. Right-click a
tile for per-camera actions.

## Where things go

| What | Where |
|---|---|
| Devices and preferences | `~/.config/kestrel/config.json` (mode 0600) |
| Passwords | System keyring via Secret Service; falls back to the config file if no keyring is available |
| Snapshots, recordings, downloads | `~/Videos/Kestrel/` — change in Preferences |

Nothing from the weather is written to disk. Readings and radar layers are held
in memory for as long as they are on screen and fetched again when they go
stale.

## Notes and limitations

- **Local network only.** This speaks to devices directly over HTTP and RTSP. It
  does not use Reolink's P2P/UID cloud relay, which is an undocumented
  proprietary protocol. The one exception is the weather, which is off by
  default and is the only part of Kestrel that reaches outside your network — a
  weewx server on your own LAN keeps even that local, apart from the radar.
- **The weather.gov source covers the United States and its territories.**
  api.weather.gov has no forecast for anywhere else and says so plainly. A
  weewx server works anywhere, but the radar does not: it is a National Weather
  Service product.
- **The ZIP code table is carried, not looked up.** weather.gov is addressed by
  coordinate, so the code is resolved against the Census Bureau's ZCTA
  gazetteer shipped inside the binary. Nothing is asked of any third party to
  do it, and there is no geocoding service to stop working. Codes that are a
  single building or a row of PO boxes are not tabulation areas and are not in
  the table; a neighbouring code covers them.
- **Keeping the screen awake needs a desktop that listens.** Kestrel asks over
  `org.freedesktop.ScreenSaver` and takes a logind idle lock. Whether that is
  honoured is the desktop's business; **About Kestrel** reports whether the
  request was actually granted.
- **Events are polled, not pushed.** The documented API has no push channel, so
  detections are sampled and reported on the rising edge. A detection shorter
  than the poll interval can be missed.
- **The event feed starts empty each session.** Devices do not expose a
  queryable detection log, so only what was observed while running is shown.
- **Face and package detection depend on the camera.** Every channel on an
  RLN36 reports people, vehicle and pet support, while face and package report
  unsupported — so those alerts never fire on that hardware.
- **Device settings** (recording schedules, detection zones, network config) are
  not exposed; use the device's own web UI. The read-modify-write layer beneath
  them exists and is tested, and refuses destructive commands outright.
- **Recording a virtual camera records the whole picture.** Kestrel remuxes
  packets rather than re-encoding them — the bundled ffmpeg is a decode-only
  LGPL build, which is what lets the whole thing be distributed the way it is —
  and a picture cannot be narrowed without encoding a new one. Snapshots *are*
  cropped: those come from the device as a full-resolution still, so a 2.5x view
  of a 4K camera saves at around 1600 across.
- **Two-way talk** is not implemented.
- **Some NVR firmware lists recordings but will not serve them.** On an RLN36
  running v3.5.0.329, `Search` returns clips with no `name` field, and there is
  no working way to fetch them over the HTTP API. Established by probing the
  device directly:
  - `Search` results carry only `StartTime`/`EndTime`/`PlaybackTime`/`size`
    (as a string) — no file handle, on either `action` value.
  - `Playback`/`Download` treat `source` as a filesystem path: omitting it gives
    403, supplying a synthesised one gives 404. The endpoint works; the path is
    the missing piece.
  - `NvrDownload` is supported (unknown commands answer `-9 "not support"`,
    this one answers `-17 "rcv failed"`) but fails for every parameter shape,
    stream type and time range tried, including exact file spans.

## Firmware quirks worth knowing

Behaviours established by probing an RLN36 directly, each of which the client
now works around:

- **`SetWhiteLed` applies asynchronously.** The command returns in 0.1–0.35s but
  `Get` keeps serving the old value for up to ~2s, so a write verified
  immediately looks like a failure. Writes poll until the device agrees.
- **A `WhiteLed` payload containing `state` ignores every other field.**
  Brightness sent alongside it never applies; sent alone it applies in 0.30s and
  changes nothing else.
- **The camera can veto "off".** Inside its lighting schedule, and while a
  detection is live, a manual switch-off is discarded.
- **A single PTZ move does not run until Stop.** The firmware applies its own
  safety timeout, so holding a direction re-issues the command.
- **`GetPtzCurPos` is unsupported**, so PTZ movement cannot be verified
  programmatically on this hardware.

## Brand assets

Generated, not vendored. After changing the palette:

```sh
python3 kestrel-rs/tools/brand.py     # needs Pillow: pip install --user pillow
```

## History

Kestrel began as a PyQt6 client. It was replaced by this Rust rewrite because
PyQt6 is GPL-3.0-only, which on distribution would have forced the whole
application under GPLv3. The Python client is preserved in git history at commit
`89ed19f` if it is ever wanted again.
