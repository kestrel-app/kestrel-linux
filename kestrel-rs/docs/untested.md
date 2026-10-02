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
- **Register + data channel** — **verified live.** `C2R_C` to the register server
  returns `R2C_C_R rsp=0` with the device's LAN, public (dmap) and relay addresses
  and a session id; `C2D_T` then brings the data channel up (`D2C_CFM rsp=0`) over
  the relay *and* via a direct NAT hole-punch to the device's public address. What
  is not yet done is the in-Rust port of this flow: `Transport::connect` still
  returns `Unsupported` rather than running the register/hole-punch/retransmit loop,
  so connecting by UID in the app still falls back to the LAN/RTSP path. The flow
  itself is measured and documented; it just needs writing.
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
- **Playback** (`playback`) — search, recorded-days, and open/seek/stop by time
  over the control channel. The result parser and the request bodies are tested;
  the control opcodes are the disassembled values; the request path rides the
  unverified session. The one remaining opcode guess is the recording search.
- **Video** (`media`) — video arrives as BCMedia frames, not RTSP. The demuxer
  reassembles an elementary stream, but the last hop — feeding that into the
  in-process `ffmpeg_next` decoder through a custom AVIO read callback — is **not
  built**: `video::stream` only opens a URL, so `stream()` on a UID device
  refuses rather than returning one that would not play. Of the BCMedia magics,
  only the info-frame magic (`1001`) was confirmed in the binary from here.

What unblocked the handshake was measuring it directly: outbound UDP (the Bash
sandbox drops UDP, so the probes ran with it disabled) against a real UID. That
turned the lookup, register, data channel and the **whole login** (a real NVR
returned `200` and its `DeviceInfo`) from guesses into confirmed behaviour, and
corrected the CRC, the BCEncrypt key, the header layout, the DATA-packet zero word,
the MD5 digest, and the fact that login stays BCEncrypt while AES is requested. What
remains is the in-Rust port of the connection driver, **AES-128-CFB** for the
post-login messages (so commands and video work), the video path, and the
recording-search opcode, which the trace showed is only recoverable from a capture.
