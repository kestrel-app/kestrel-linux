# Kestrel (Rust)

The compiled rewrite of Kestrel, so it ships as a distributable binary rather
than a Python tree. The PyQt6 client in `../kestrel/` remains the working
application until this reaches parity.

## Why a rewrite

PyQt6 is **GPL-3.0-only**. Distributing the Python client therefore requires
releasing it under GPLv3 or buying a commercial Qt binding licence. This port
uses permissively licensed crates, so the only copyleft left in the stack is
ffmpeg (LGPL) — and that is satisfied by shipping it as shared libraries.

## Build

```sh
./build.sh build --release     # builds vendored ffmpeg on first run, ~2 min
./build.sh test
./build.sh run -- 192.0.2.242 --user admin --search --stream
```

`build.sh` exists because two environment details have to be right: the vendored
ffmpeg's `pkg-config` directory, and a clang include path for bindgen on
machines with no clang package installed.

The password comes from `KESTREL_PASSWORD` rather than a flag, so it never lands
in shell history or a process listing.

## ffmpeg

`vendor/build-ffmpeg.sh` builds ffmpeg 7.1 into `vendor/prefix`, configured for
a distributable client:

- **LGPL only** — no `--enable-gpl`, no `--enable-nonfree`. Kestrel decodes; the
  GPL pieces (libx264/libx265) are encoders it never calls.
- **Shared libraries**, so the LGPL requirement that a user be able to replace
  the library is met by the shared-library mechanism. No object files need
  publishing.
- **`--disable-everything`** plus an explicit component list — only H.264/H.265
  decoding, the RTSP/MP4 demuxers, MP4 muxing, and swscale. All seven libraries
  total 5.6 MB.
- **x86 assembly enabled**, building nasm from source first if the machine has no
  assembler. Losing ffmpeg's hand-written SIMD roughly halves decode throughput,
  which matters at 16 simultaneous streams.

The binary carries an rpath of `$ORIGIN/lib`, so a release bundle finds the
libraries beside it.

## Status

| Layer | State |
|---|---|
| API client | Done — verified against an RLN36 |
| Video ingest | Done — RTSP demux/decode, warm streams, MP4 remux |
| Config / keyring | Not started |
| UI (egui) | Not started |
| Playback, PTZ panel, follow-motion | Not started |

Every firmware quirk found while building the Python client is carried over,
with the evidence in the comments:

- `Search` `action` negotiation — an RLN36 answers `action=1` with HTTP 502 and
  needs `action=0`; other firmware is the reverse. The working value is learned
  and cached, with fallback.
- `GetPtzPreset` returns a flat list, a `{"preset": [...]}` wrapper, or a single
  object depending on firmware.
- Recording entries may have no `name` and a string `size`; only the time span
  is dependable.
- `GetChannelstatus` is authoritative on channel count — `GetDevInfo` can report
  1 channel for a 36-channel NVR when the device is busy.
- Dual-lens (TrackMix) channel linking, which must not fire for a 2-channel NVR.
- HTTP 5xx is a *command* failure, not a transport failure.
- Tokens and RTSP credentials are redacted from every log line and error.

## Measured against the RLN36

| | first frame |
|---|---|
| cold connect | 6.10s |
| adopt a warm stream | **0.07s** |

Warm streams stay connected and demux without decoding, holding the latest
keyframe — the same architecture as the Python client, where it measured 6.6s
versus 0.09s.
