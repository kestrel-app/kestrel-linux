//! Reolink's proprietary P2P protocol ("Baichuan"), for reaching a device by its
//! UID instead of an IP address.
//!
//! Everywhere else a Reolink device is an HTTP host: the [`reolink`] vendor logs
//! in over CGI and streams over RTSP, and that is the right path whenever the
//! device is reachable at an address. But a device registered to Reolink's cloud
//! is addressable by a **UID** — a short printable code — from anywhere, with no
//! port forwarding, by asking Reolink's registrars to broker the connection. That
//! is what this module speaks.
//!
//! The protocol is built in layers, each its own file:
//!
//! - [`crypto`] — the ciphers and CRC the rest is made of.
//! - [`udp`] — the discovery datagrams: UID → server set.
//! - [`transport`] — a reliable ordered byte stream over UDP, direct or relayed.
//! - [`wire`] — the control-message header that rides the stream.
//! - [`cmd`] — the command numbers.
//! - [`xml`] — the small XML bodies those commands carry.
//! - [`login`] — the credential handshake, including the proof-of-work step.
//! - [`session`] — request/reply over an authenticated stream.
//! - [`playback`] — recording search and playback commands.
//! - [`media`] — turning the device's video frames back into a byte stream.
//!
//! **Verification status.** [`crypto`], [`udp`], [`wire`], [`cmd`] and [`xml`] are
//! pure framing and are exercised by their own round-trip tests. The layers that
//! need a live device — [`transport`], [`login`], [`session`], [`playback`],
//! [`media`] — are implemented from the official app as the specification but have
//! not been run against hardware or a capture from this machine. Every such gap is
//! recorded in `docs/untested.md`. The guiding rule (`docs/untested.md`) is that a
//! confident wrong guess is worse than none, so the unconfirmed numbers and byte
//! layouts are marked as such at their definition rather than presented as known.
//!
//! [`reolink`]: super::reolink
//!
// The layers below the transport — opcodes, media demux, playback bodies — are
// built and tested but not yet *reached* at runtime, because `Transport::connect`
// honestly refuses until the handshake is confirmed against a device. They are
// staged, not dead: each is exercised by its own tests and wired for the moment
// the transport comes up. So dead-code warnings are allowed for this module alone,
// rather than left as noise that would train the eye to ignore them.
#![allow(dead_code)]

pub mod client;
pub mod cmd;
pub mod crypto;
pub mod udp;
pub mod wire;
pub mod xml;

pub mod login;
pub mod media;
pub mod playback;
pub mod session;
pub mod transport;
