# Untested paths

Things that are built and shipped but have never actually run, with what it
would take to exercise each. Kept here so they are not mistaken for verified.

Last reviewed: 31 August 2026.

## Vendors other than Reolink

Frigate, ZoneMinder, QNAP QVR, UniFi Protect and ONVIF are implemented from
their published APIs and have **never been run against any of them**. None was
reachable from this network. (The Reolink P2P/UID section below is now a partial
exception: the UID lookup has been verified against the live registrars.) They compile, they are covered by unit tests for
the parts that are pure logic — URL shapes, response parsing, label mapping —
and that is all that is known.

What is most likely to be wrong, per vendor:

- **Frigate** — live video assumes the bundled go2rtc republishes each camera
  over RTSP on port 8554 under its own name, with `_sub` for the detect stream.
  An install that has moved go2rtc, or one whose cameras are not restreamed,
  gets nothing. Authentication is assumed off, which is the default; with 0.14
  auth enabled every request returns 401.
- **ZoneMinder** — the `/zm` path prefix is assumed. An install at the web root
  needs it removed, and if that turns out to be common it belongs in the device
  config rather than a constant. Video comes from `nph-zms` as MJPEG, whose
  location moves between packagings, and which has no keyframes — so warm
  streams buy nothing there, though they cost nothing either.
- **QNAP** — the camera list path (`/qvrpro/apis/qvrpro/camera/list`) and the
  snapshot path differ between QVR Pro and QVR Elite in ways the documentation
  is vague about; both spellings of the list response are accepted, which is a
  guess. The RTSP path (`/qvrpro/<guid>/<profile>`) is the least certain thing
  in this file. The login response is XML and is read by string search rather
  than a parser, which is fine for the two fields wanted and would not be for
  more.
- **ONVIF** — the one implemented from a published *standard* rather than a
  vendor's API, which changes what is likely to be wrong. Not the message
  shapes — those are specified — but the places the standard leaves room and
  devices use it differently. Three in particular. The **snapshot** is fetched
  with HTTP Basic; ONVIF does not say which HTTP authentication a snapshot URL
  uses, and a camera that insists on Digest will refuse it and say so rather
  than returning a broken picture. The **main/sub split** is inferred by
  grouping profiles on their `SourceToken` and ordering by pixel count, because
  nothing in the standard marks a profile as the substream and profile *names*
  are unusable ("Profile_1", "mainStream", ""); a device that reuses one source
  token across physical cameras would collapse them into one tile. And
  **`GetStreamUri` is called once per profile at connect**, so a sixteen-channel
  NVR is thirty-odd round trips before the first tile appears.

  What *is* verified is the part most likely to be silently wrong, against
  `tools/onvif-stub.py`: the WS-Security digest. The stub recomputes
  `Base64(SHA1(nonce + created + password))` from what it is sent and rejects a
  mismatch, and it runs its clock forty minutes fast on purpose — so the
  timestamp correction in `Onvif::learn_the_clock` is exercised rather than
  assumed. Eleven authenticated calls, none refused; five profiles over three
  video sources grouped into three cameras with the right codecs and the right
  one flagged for PTZ. **PTZ itself has never been sent to anything** — the stub
  answers the media service only.

- **UniFi Protect** — needs a **local** account; a cloud account requires
  two-factor, which cannot be completed here (the console answers 499, and that
  is reported as such). Streams are RTSPS by an alias that Protect only
  publishes once the stream is enabled per camera; a camera without one says so
  rather than failing to connect. The self-signed certificate the console
  presents is no longer a blocker — "Trust this device's own certificate" has to
  be ticked for it, and that path is verified against a real self-signed device
  (see below) though not against Protect itself.

Playback is Reolink-only. The others report `supports_playback() == false` and
the Playback tab says so rather than showing an empty calendar. Floodlight is
Reolink-only for the same reason: the capability is reported false at
discovery, so the control does not appear at all on those systems.

PTZ is now Reolink **and** ONVIF. On ONVIF it is offered only for a camera whose
profile carries a `PTZConfiguration` *and* whose device advertises a PTZ
service, so a fixed camera still shows no pad. Focus is deliberately refused
rather than mapped: it belongs to ONVIF's imaging service, which is a different
endpoint and a different call, and a focus button that silently did nothing
would be worse than one that is not there.

Detections are implemented for Frigate (in-progress events) and UniFi (the
`isMotionDetected` flag in the bootstrap), both unrun. ZoneMinder and QNAP
report none, so follow motion never triggers for them.

What *is* verified is the seam: Reolink connects, enumerates 36 channels and
streams live video through the vendor dispatcher with no behaviour change, on
the real RLN36.

## Identifying a system

The probe is verified against Reolink only — `192.0.2.242` answers
`Detected { vendor: "reolink", detail: "Reolink", port: 80 }` in 0.18s. The
other four branches have never seen the response they are matching on:

- Frigate is identified by a short plain-text body at `/api/version`.
- UniFi by a 401 from `/proxy/protect/api/bootstrap` — which needs the device's
  certificate trusted first, or the probe never gets far enough to be refused.
- ZoneMinder by `version` in `/zm/api/host/getVersion.json`.
- QNAP by `QDocRoot` appearing in `/cgi-bin/authLogin.cgi`.
- ONVIF by a `GetSystemDateAndTimeResponse` to the one call the standard defines
  as needing no credentials. This one **is** verified, against
  `tools/onvif-stub.py`: `127.0.0.1` answers
  `Detected { vendor: "onvif", detail: "ONVIF device", port: 80 }`.

ONVIF is probed **last**, and that ordering is load-bearing rather than
incidental — see `PROBE_ORDER`. A Reolink NVR with ONVIF switched on answers
both probes, and identified as ONVIF it would lose playback, floodlight,
presets and detections. Verified with a stub answering both: it comes back
`vendor: "reolink"`, and the ONVIF probe is never reached.

A wrong guess is cheap — the user can still pick the system by hand — but a
*confident* wrong guess would be worse than none, which is why each probe looks
for something structural rather than merely a 200.

## Finding devices on the network

`api::discover` has two halves and they are verified very differently.

**The sweep is exercised; the announcement is not.** Reading the attached
networks out of `/proc/net/route`, expanding one to its hosts, refusing anything
wider than a `/22`, and the little-endian column order are all unit-tested
against a real routing table — that last one is the detail that fails silently,
since `0002A8C0` read the wrong way round is a plausible-looking `0.2.168.192`.
Identification of a found host is verified end to end against the ONVIF stub.

**WS-Discovery has never had a device answer it.** The probe message and the
`ProbeMatches` parsing are unit-tested, but no multicast reply has ever been
received: the build machine shares a network with no cameras. What is untested
is everything between — whether the socket receives replies that a real camera
sends after its randomised delay, whether the 2.5-second window is long enough
on a busy network, and whether the extra broadcast packet helps or is ignored.

Two known limits, neither a bug: **IPv6 is not swept** (the routing table is
read as IPv4 only, though an IPv6 `XAddrs` from a probe reply is parsed and kept
correctly), and a device on a network this machine is *not* attached to is only
found if it announces itself. Both cases still work by typing the address.

## Trusting a device's certificate

Verified against the RLN36, which serves HTTPS on 443 with a self-signed X.509
**v1** certificate (`CN=CERTIFICATE`):

- with the setting off, the connection is refused —
  `invalid peer certificate: UnsupportedCertVersion`
- with it on, the same request returns HTTP 200

What has *not* been tried is a device whose certificate is merely untrusted
rather than unparseable — a v3 self-signed certificate, which is what UniFi and
most modern appliances present. That path is strictly easier than the one
verified, but it has not been run.

## Audio

Verified against Reolink: AAC 16 kHz mono decoded, resampled and played through
ALSA. No other vendor's audio has been tried, and only Reolink's RTSP streams
are known to carry any.

## Some NVR firmware lists recordings but will not serve them

On an RLN36 running v3.5.0.329, `Search` returns clips with no `name` field and
there is no working way to fetch them over the HTTP API. Detail is in the
top-level README; it is a firmware limitation, not an untested path, but it is
the thing most likely to be mistaken for a bug in playback.

## Reolink P2P / UID (`api::vendor::baichuan`)

The proprietary "Baichuan" transport that reaches a Reolink device by its cloud
UID instead of an address. Built from the official Reolink app
(`com.mcu.reolink` 4.63.0.3) read as the specification, and from the protocol as
publicly understood. **The whole handshake up to the login nonce is now verified
against a real device** (see below) — lookup, register, data channel (direct and
relay) and the first login leg. What is *not* yet done is completing the password
login (needs the device password), the full in-Rust connection driver, and video.
Verifying it took measuring on the wire, which corrected several things that were
wrong when written from the spec alone.

Verified here (pure logic, unit-tested; several anchored to real captured bytes):

- **Crypto** — the discovery XOR cipher, the discovery CRC (uncomplemented,
  init 0 — a bug a test caught), the BCEncrypt body cipher (key measured by
  decrypting a device's real login reply), and from-scratch MD5 (the login digest)
  and SHA-256, all against standard or captured vectors.
- **Framing** — discovery packets, the BC message header (the measured layout:
  `magic|msg_id|body_len|channel|stream|msg_num|response_code|class|[payload_offset]`),
  the reliable DATA packets (a 20-byte header with the easy-to-miss zero word), the
  BCMedia demuxer, and the XML bodies all round-trip.

Unverified, in the order they would be exercised:

- **UID lookup** (`transport::lookup_uid`) — **now verified live.** Run against the
  real registrars with a real UID: the owning registrar answers `rsp=0` with the
  register/relay/log/device addresses, the others `rsp=-3`. This flushed out a real
  bug — the discovery CRC must init to 0, not 0xffffffff; with the wrong init the
  registrars silently dropped every packet. Fixed in `crypto::bc_crc` and anchored
  to a captured packet in its test. This is the first part of the P2P stack
  confirmed against live infrastructure.
- **Register + data channel** — **verified live**, and now **ported to Rust**
  (`Transport::connect` runs lookup → `C2R_C`/`R2C_C_R` → `C2D_T`/`D2C_CFM`, direct
  hole-punch with a relay fallback, over a reliable-UDP layer with per-packet ACKs
  and in-order reassembly). The message builders/parsers and the reassembler are
  unit-tested against captured replies; the socket I/O runs only against a real
  device (CI has none, and the Bash sandbox drops UDP), so the live driver is not
  exercised in CI.
- **AES-128-CFB** (`crypto`) — the post-login cipher is now implemented from scratch
  (block cipher pinned to FIPS-197, CFB to NIST SP800-38A) and wired in: the session
  switches to it after login, keyed by `make_aes_key` from the nonce and password.
  The cipher is verified by vectors; its use end to end against a device (a command
  round-trip) is not yet confirmed.
- **Login** (`login::login`) — **verified live, end to end.** A full login against a
  real NVR returns `response_code == 200` and a `DeviceInfo` body. The digest is
  uppercase MD5 of `value+nonce` truncated to 31 chars (`login_hash`), **not** the
  SHA-256 that was there before, and there is **no proof-of-work** (the app's PoW is
  for cloud-account auth). The subtlety that made it work: every login message
  (`msg_id==1`), both directions, is **BCEncrypt even though leg 1 requests AES** —
  AES only takes over *after* login. Leg 1 must request AES (`0xdc12`); the device
  ignores a BCEncrypt-only request outright.
- **Command opcodes** (`cmd`) — every command *name* is confirmed present in the
  binary. The control-path numbers (login, video, ping…) are the settled public
  values. The playback-by-time V2 opcodes (open 381, stop 382, seek 383), the
  legacy seek (123) and the calendar command (`GET_RECFILEDATE` 142) were since
  **recovered by disassembling** the functions that send them and reading the
  command id loaded — cross-checked against `downloadSnap`, which loads the known
  `SNAP` opcode (109). These replaced earlier guesses that were off by hundreds.
  Still a guess: the *search* opcode, playback-by-name, and the download family —
  these stay `[UNVERIFIED]`. The search opcode was then traced in full and shown to
  be **not statically recoverable**: `rfsSearch` packs the command from a struct
  field (`BC_FILE_FIND+0x7c`) and dispatches through a `std::function`, and nothing
  in the native libraries or the JS bundle writes that field with a constant. It
  can only come from a packet capture. A flat refusal of one of these means the
  number is wrong.
- **Playback** (`playback`) — **finding recordings is verified live; playing them
  is not built.** The search is three commands — open (272, `findAlarmVideo` with
  channel, stream and range; reply `fileHandle`), fetch a page of up to 50 clips (273,
  `alarmVideoInfo` with `bFinished`), close (274) — taken from reolink_aio's Baichuan
  client after the app's own path proved unreadable by disassembly, and confirmed on
  an RLN16-410: 98 clips for one day, paged, in ~8s over the satellite link. The
  calendar (142) answers a `DayRecords` request (the app's own builder's shape) with
  the recorded days as offsets from the first day asked about. Each clip carries the
  event's times, its trigger (`md`, `people`, `dog_cat`…) and, in its file name,
  the start of its recording file.
  **Fetching a recording is verified live too** (`replay`): `ReplayByTimeV2` (381),
  whose body was read out of the app's request builder — anything short of its full
  shape, with the day in a `durationList` and the span as seconds of that day, is
  refused with `400`. The footage comes back as the same BCMedia messages live video
  uses, after one stream-info message that is not media and is skipped. Stop (382)
  has no body. Replay is paced at real time, so an hour-long recording took an hour
  to fetch — it is now the fallback. Fetching is by **download** (143, from
  `downloadFileByTime`, its `FileInfoList` body traced the same way): as fast as the
  link allows, the same BCMedia, ended by a bodiless `300`. Fetches ride the
  device's shared connection (`hub`): on its own connection a fetch from the NVR at
  an address got no answer to its login while the wall was streaming from that NVR
  over Baichuan. Live by UID, with a tile streaming: the same 25 s span in 10 s over
  that one connection. Tested over TCP against a fake device that accepts a single
  connection; not yet against the NVR at an address.
  **Playing** no longer waits for a fetch: the player (`PlaybackWorker::
  start_recording`) asks for a replay from the point wanted and decodes each frame
  as it arrives, timed by its BCMedia timestamp; seek, pause and speed changes end
  the replay and ask again from where it was. Live by UID over satellite: first
  picture 1.7s after asking with the wall already streaming (9.1s from cold, mostly
  the P2P connect), then playing at real time. Download still fetches the whole
  recording (143) to save it.
  The NVR at an address refuses replay with `405` (and pre-V2 replay opens by file
  name, which its clips lack), so a refused replay falls back to **downloading a
  window at a time** (30 s, the next asked for while half a window is still queued,
  the overlap from each window's leading keyframe skipped). Tested against a fake
  device that refuses replay and serves real H.264 frames: pictures, and playing on
  across the join; not yet against the NVR itself. Our side takes a download in at
  ~600 MB/s release (~130 MB/s debug) over loopback, so download speed is the
  device's and the link's. The player and fetch hold the shared connection between
  requests: letting it close between the refused replay and the download meant a
  fresh login, which a device that accepts one connection never answered.
  **Playback is the same for both NVRs.** The one at an address now searches and
  reads its calendar over Baichuan too, as request/reply commands carried by the
  shared connection (`hub::Caller`), so its clips carry triggers; HTTP only if
  Baichuan cannot answer. Tested over TCP against the one-connection fake device
  with a tile streaming; not against the NVR itself. Triggers cover every name the
  search's own request lists, and Playback filters on them. A span chosen with two
  handles on the zoomable timeline downloads exactly that span (143): live by UID,
  a one-minute range came back as 63.5 s of sub-stream H.264 (from the keyframe
  before) and was saved where the UI said. Each fetch runs on a connection of its own and is written to an MP4
  without re-encoding: a 25-second clip came back as 258 frames of H.264 640x360 in
  ~25s over the satellite link, and played in the Playback tab. Because it is by
  time, it also fetches what an NVR at an address lists over HTTP without a file
  name — the clips that API cannot serve — but that use has not run against
  hardware. The Download button saves any recording as an MP4 in Downloads; a
  download over HTTP copies the device's file the same way, but was not clicked
  under Xvfb.
- **Video** (`media`) — video arrives as BCMedia frames, not RTSP, and the whole
  framing was **confirmed live**: a `Preview` request (msg_id 3, class 0x6414)
  returns `200` and a stream of msg_id 3 messages (class 0x0000), codec **H.265**.
  The decode chain is now **built and validated end to end against the NVR**: a
  capture was run through it and ffprobe identified the output as `h264`, 640×360,
  yuv420p. In the code:
  - `media` demuxes the real frame format (magic, `H264`/`H265` type, payload size,
    additional-header size, 8-byte padding; the INFO frame's size field is the frame's
    total length) into an elementary stream.
  - `video` undoes the per-message FullAes layer (`bcmedia_bytes`): the extension is
    AES XML giving `<binaryData>` (payload plaintext) or `<encryptLen>N</encryptLen>`
    (first N payload bytes AES, rest plaintext), and `drain_video` walks a raw message
    stream feeding each video message's BCMedia to the demuxer.
  - `video::VideoStreamer` sends `Preview` and polls the stream; `Session` gains
    `send_oneway` and `decode_video_into`.
  The **decoder input** is now built too: `video::bc_avio` opens an ffmpeg input backed
  by a byte source through a custom `AVIOContext` (the one piece of `unsafe` FFI), with
  `SliceSource` and a channel-backed `ChannelSource` to feed it. Its decode path is
  **validated locally** — a test decodes a synthetic H.264 stream through the custom
  AVIO and gets frames at the right dimensions. The glue is now in place too:
  `BaichuanClient::stream` returns a P2P `StreamSource`, and the stream worker's
  `open_baichuan` spawns a feeder thread that opens its own session, logs in, starts
  `VideoStreamer`, and pushes the elementary stream into a `ChannelSource` the custom
  AVIO decodes — the ordinary decode loop runs unchanged, and dropping the input (worker
  stop or reconnect) closes the channel and stops the feeder.

  **The Rust path itself now runs against the NVR**, through two opt-in live tests
  (`--ignored`, driven by `KESTREL_TEST_UID`/`KESTREL_TEST_PASS`):
  `connects_to_a_real_device_by_uid` (lookup → register → data channel → login →
  GetVersion → 24 channels) and `streams_real_video_by_uid` (sub stream → FullAes decode
  → demux → custom AVIO → 35 frames decoded at 640×360). Running them is what found the
  two bugs the hand-written probes had hidden: the device's ack carries the *client* id,
  and the ack's packet id lives at offset 16 of a 28-byte header. A third,
  `a_uid_tile_shows_a_picture`, drives the stream worker itself and inspects the
  published frames: the sub stream reaches a tile as a real picture (no green).
  **The main stream does not, and that is the link, not the code.** The test NVR is on
  geostationary satellite: a ~1.7s data-channel round trip and ~20–35 KB/s of goodput,
  with over half the packets duplicates even with no gaps (the device resends before an
  ack can get back). A ~526 KB H.265 keyframe cannot arrive in time, so a tile asking
  for main falls back to sub after `timeout_secs`. Measured on the way: the ack's word
  at offset 20 is a latency in µs the device paces its resends by — reporting the round
  trip moved resends from ~0.7s to ~3s and roughly doubled goodput. Whether main plays
  over P2P on a fast link is untested.
- **Finding a UID device on the local network** (`transport::run_local`, and the
  register reply's `dev` address) — before going through the registrars, a UID connect
  broadcasts `C2D_C` on ports 2015/2018 (as Reolink's client and neolink do) and, after
  register, tries the device's own LAN address. Either keeps the stream off the
  internet, which is the only way the main stream can play by UID on a satellite-linked
  NVR (its public address is shared carrier NAT, so even a client on the same LAN went
  out and back over the satellite). The request and the reply matching are unit-tested;
  **neither has been answered by a device**, since this machine is not on the NVR's
  network. Failing, they cost ~1.5s each and the remote path runs as before.
- **Baichuan over TCP** (`link::TcpLink`, `Reach::Tcp`) — the same session over a TCP
  connection to the device's BC port (9000), for a device reached at an address. Login,
  an AES command and pushed video run end to end in tests against a fake device on
  loopback built from this codec — which checks the plumbing, not the device's side.
  **Not yet run against hardware**: the test NVR has no port open to this machine.
  Even so, a Reolink device at an address now takes its **live video** over Baichuan by
  default (`vendor::bc_video`), because it cannot make things worse: the stream keeps
  its RTSP URL, a tile whose Baichuan video fails falls back to RTSP (tested with a
  closed port and a local file standing in for RTSP), and the first failure switches
  the whole device to RTSP. Everything else — channels, snapshots, detections, PTZ,
  floodlight, playback — stays HTTP, since the Baichuan client does not have those
  commands yet. Rollback: the device's "Live video" setting off gives the plain HTTP
  client, and the `pre-bc-direct-ip` tag is the code before this change.
- **One connection per device for video** (`baichuan::hub`) — on a real NVR at an
  address, four channels' Baichuan sessions failed and went to RTSP while the others
  played. Video now shares one connection and one login per device, as Reolink's own
  app does: measured over P2P, several Previews on one session stream side by side as
  long as each has its own `handle` (a reused handle replaced the earlier stream), and
  each video message echoes its Preview's message number, which routes it. Verified
  live by UID (two tiles on channels 0 and 3 playing over one connection) and over TCP
  against the fake device, which accepts only one connection. Not yet seen on the NVR
  at an address. The device's control session (channel list, playback) is still a
  connection of its own, so a device holds two.

What unblocked the handshake was measuring it directly: outbound UDP (the Bash
sandbox drops UDP, so the probes ran with it disabled) against a real UID. That
turned the lookup, register, data channel and the **whole login** (a real NVR
returned `200` and its `DeviceInfo`) from guesses into confirmed behaviour, and
corrected the CRC, the BCEncrypt key, the header layout, the DATA-packet zero word,
the MD5 digest, and the fact that login stays BCEncrypt while AES is requested. Since
then an **AES command round-trip was confirmed live** too: `GetVersion` after login
returned `200` and a decryptable `VersionInfo` (model RLN16-410), proving the AES
cipher end to end, and the connect driver, AES and channel enumeration are now in
Rust. `connect` logs in, reads the channel count from the login `DeviceInfo` and the
model/firmware from `GetVersion`, and builds the channel list. The video framing was
then measured live too (H.265 over BCMedia; the demuxer matches it). What remains is
per-channel detail (names and online status via a channel-status command), the rest
of the video path (FullAes-binary decode in the session, the streaming loop, and the
custom ffmpeg input), and the recording-search opcode, which the trace showed is only
recoverable from a capture.
