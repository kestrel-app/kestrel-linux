# Untested paths

Things that are built and shipped but have never actually run, with what it
would take to exercise each. Kept here so they are not mistaken for verified.

Last reviewed: 2 August 2026.

## Vendors other than Reolink

Frigate, ZoneMinder, QNAP QVR and UniFi Protect are implemented from their
published APIs and have **never been run against any of them**. None was
reachable from this network. They compile, they are covered by unit tests for
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
- **UniFi Protect** — needs a **local** account; a cloud account requires
  two-factor, which cannot be completed here (the console answers 499, and that
  is reported as such). Streams are RTSPS by an alias that Protect only
  publishes once the stream is enabled per camera; a camera without one says so
  rather than failing to connect. The self-signed certificate the console
  presents is no longer a blocker — "Trust this device's own certificate" has to
  be ticked for it, and that path is verified against a real self-signed device
  (see below) though not against Protect itself.

Playback is Reolink-only. The others report `supports_playback() == false` and
the Playback tab says so rather than showing an empty calendar. PTZ, presets and
floodlight are Reolink-only for the same reason: every capability is reported
false at discovery, so the control pane does not appear at all on those systems.

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

A wrong guess is cheap — the user can still pick the system by hand — but a
*confident* wrong guess would be worse than none, which is why each probe looks
for something structural rather than merely a 200.

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
