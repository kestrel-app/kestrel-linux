//! Request/reply over an authenticated control stream.
//!
//! A [`Link`] gives an ordered byte stream (UDP by UID, or TCP by address); [`wire::Message`] frames
//! one command on it. This ties them together: send a command, read whole messages
//! off the stream until the matching reply arrives, and hand it back.
//!
//! **Verification status.** The message accumulate-and-drain logic ([`drain_one`])
//! is pure and tested below, and the header/crypto it relies on were confirmed
//! against a real device. [`Session::call`] drives the live transport, which is
//! verified up to the login nonce but whose full in-Rust driver is not yet wired —
//! see [`super`] and `docs/untested.md`.

use std::time::Duration;

use super::link::Link;
use super::wire::{Encryption, Message};
use crate::api::error::{Error, Result};

/// Something that can send a modern command and wait for its reply: a session of
/// its own, or a device's shared connection ([`super::hub::Caller`]). Playback's
/// search and calendar take either, so a device reached by UID and one at an
/// address run the same code.
pub trait Calls {
    fn call(&mut self, msg_id: u32, xml: String, timeout: Duration) -> Result<Message>;
}

impl Calls for Session {
    fn call(&mut self, msg_id: u32, xml: String, timeout: Duration) -> Result<Message> {
        Session::call(self, msg_id, xml, timeout)
    }
}

/// An authenticated Baichuan control session.
pub struct Session {
    transport: Box<dyn Link>,
    /// How bodies are encrypted, and how replies are decrypted. Starts
    /// [`Encryption::None`]; login moves it to BCEncrypt for the login exchange and
    /// then to AES for everything afterwards.
    encryption: Encryption,
    /// Monotonic message number, echoed by the device so replies can be matched.
    msg_num: u16,
    /// Buffered stream bytes not yet framed into a message.
    buffer: Vec<u8>,
    /// Messages the device sent unasked — status reports it pushes after login,
    /// like the channel list — kept for whoever wants them rather than dropped
    /// while waiting for a reply. Bounded, newest kept.
    pushed: Vec<Message>,
}

/// How many unasked messages to hold. A device pushes a handful after login; this
/// only stops a long session from growing without bound.
const PUSHED_KEPT: usize = 32;

impl Session {
    pub fn new(transport: Box<dyn Link>) -> Self {
        Session {
            transport,
            encryption: Encryption::None,
            msg_num: 0,
            buffer: Vec::new(),
            pushed: Vec::new(),
        }
    }

    pub fn set_encryption(&mut self, encryption: Encryption) {
        self.encryption = encryption;
    }

    pub fn encryption(&self) -> Encryption {
        self.encryption
    }

    pub fn next_msg_num(&mut self) -> u16 {
        self.msg_num = self.msg_num.wrapping_add(1);
        self.msg_num
    }

    /// Send a prepared message and wait for a reply bearing the same `msg_id`.
    pub fn send(&mut self, request: Message, timeout: Duration) -> Result<Message> {
        let want = request.msg_id;
        self.transport.send(&request.to_bytes())?;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(msg) = drain_one(&mut self.buffer, self.encryption)? {
                if msg.msg_id == want {
                    return Ok(msg);
                }
                self.keep_pushed(msg); // an unrelated message (e.g. a push)
                continue;
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::connection(no_reply(want, &self.buffer, &self.pushed)));
            }
            let more = self.transport.take_inbound();
            if more.is_empty() {
                std::thread::yield_now();
            } else {
                self.buffer.extend_from_slice(&more);
            }
        }
    }

    /// Send a modern XML command and wait for its reply, managing the message
    /// number and the session's current encryption.
    pub fn call(&mut self, msg_id: u32, xml: impl Into<String>, timeout: Duration) -> Result<Message> {
        let num = self.next_msg_num();
        let request = Message::modern(msg_id, num, self.encryption, xml);
        self.send(request, timeout)
    }

    /// Send several requests at once and collect their replies, matched by message
    /// number — one round trip for all of them rather than one each, which over a
    /// slow link is the difference between a second and many. A reply that has not
    /// come by `timeout` is `None`.
    pub fn call_each(&mut self, requests: Vec<Message>, timeout: Duration) -> Result<Vec<Option<Message>>> {
        let nums: Vec<u16> = requests.iter().map(|r| r.msg_num).collect();
        for request in &requests {
            self.transport.send(&request.to_bytes())?;
        }
        let mut replies: Vec<Option<Message>> = vec![None; nums.len()];
        let deadline = std::time::Instant::now() + timeout;
        while replies.iter().any(Option::is_none) && std::time::Instant::now() < deadline {
            while let Some(msg) = drain_one(&mut self.buffer, self.encryption)? {
                match nums.iter().position(|n| *n == msg.msg_num && requests.iter().any(|r| r.msg_id == msg.msg_id)) {
                    Some(at) if replies[at].is_none() => replies[at] = Some(msg),
                    _ => self.keep_pushed(msg),
                }
            }
            let more = self.transport.take_inbound();
            self.buffer.extend_from_slice(&more);
        }
        Ok(replies)
    }

    fn keep_pushed(&mut self, msg: Message) {
        if self.pushed.len() >= PUSHED_KEPT {
            self.pushed.remove(0);
        }
        self.pushed.push(msg);
    }

    /// The latest message the device pushed with this id, waiting up to `timeout`
    /// for one if none has arrived yet. It and any older ones with the same id are
    /// taken out of the session's keeping.
    pub fn pushed(&mut self, msg_id: u32, timeout: Duration) -> Option<Message> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            while let Ok(Some(msg)) = drain_one(&mut self.buffer, self.encryption) {
                self.keep_pushed(msg);
            }
            if let Some(at) = self.pushed.iter().rposition(|m| m.msg_id == msg_id) {
                let latest = self.pushed.remove(at);
                // Older ones are superseded: each push is the device's whole picture.
                self.pushed.retain(|m| m.msg_id != msg_id);
                return Some(latest);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            let more = self.transport.take_inbound();
            self.buffer.extend_from_slice(&more);
        }
    }

    /// Send a message without waiting for a reply — used to start video, whose
    /// response is a continuous push rather than a single reply.
    pub fn send_oneway(&mut self, request: Message) -> Result<()> {
        self.transport.send(&request.to_bytes())
    }

    /// Service the socket and feed any video messages that arrived into `stream`.
    /// Video is high-rate and not request/reply, so it bypasses [`Session::call`]:
    /// the raw bytes are walked by [`super::video::drain_video`], which decodes each
    /// video message's extension and binary payload with the session's cipher.
    pub fn decode_video_into(&mut self, stream: &mut super::media::VideoStream) -> Result<()> {
        let more = self.transport.take_inbound();
        self.buffer.extend_from_slice(&more);
        let used = super::video::drain_video(&self.buffer, self.encryption, stream)?;
        self.buffer.drain(..used);
        Ok(())
    }
}

impl Session {
    /// Service the link and hand every whole message that arrived to `route`: its
    /// header, its still-encrypted body, and the cipher to read it with. For a
    /// connection carrying several things at once (see [`super::hub`]).
    pub fn decode_routed(
        &mut self,
        route: impl FnMut(&super::wire::MsgHeader, &[u8], Encryption),
    ) -> Result<()> {
        let more = self.transport.take_inbound();
        self.buffer.extend_from_slice(&more);
        let used = super::video::drain_routed(&self.buffer, self.encryption, route)?;
        self.buffer.drain(..used);
        Ok(())
    }
}

/// Say what did arrive when a reply did not: nothing at all, other messages, or
/// bytes that never formed one — three different faults that read alike as "no
/// reply".
fn no_reply(want: u32, buffer: &[u8], pushed: &[Message]) -> String {
    if !buffer.is_empty() {
        let head: Vec<String> = buffer.iter().take(24).map(|b| format!("{b:02x}")).collect();
        return format!(
            "BC cmd {want}: no reply; {} unframed byte(s) arrived, starting {}",
            buffer.len(),
            head.join("")
        );
    }
    if !pushed.is_empty() {
        let ids: Vec<String> = pushed.iter().map(|m| m.msg_id.to_string()).collect();
        return format!("BC cmd {want}: no reply; other messages arrived ({})", ids.join(", "));
    }
    format!("BC cmd {want}: no reply; the device sent nothing")
}

/// Pull the first whole control message out of `buffer`, removing its bytes,
/// decrypting its body with `cipher`. `Ok(None)` when the buffer does not yet hold a
/// complete message.
pub fn drain_one(buffer: &mut Vec<u8>, cipher: Encryption) -> Result<Option<Message>> {
    match Message::parse(buffer, cipher)? {
        Some((msg, used)) => {
            buffer.drain(..used);
            Ok(Some(msg))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::vendor::baichuan::cmd;

    #[test]
    fn drains_messages_one_at_a_time_and_keeps_the_remainder() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&Message::modern(cmd::LOGIN, 1, Encryption::None, "<a/>").to_bytes());
        buf.extend_from_slice(&Message::modern(cmd::PING, 2, Encryption::None, "<b/>").to_bytes());

        let first = drain_one(&mut buf, Encryption::None).unwrap().unwrap();
        assert_eq!(first.msg_id, cmd::LOGIN);
        let second = drain_one(&mut buf, Encryption::None).unwrap().unwrap();
        assert_eq!(second.msg_id, cmd::PING);
        assert!(drain_one(&mut buf, Encryption::None).unwrap().is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn a_partial_message_is_left_in_the_buffer() {
        let whole = Message::modern(cmd::PING, 1, Encryption::None, "<b/>").to_bytes();
        let mut buf = whole[..whole.len() - 2].to_vec();
        assert!(drain_one(&mut buf, Encryption::None).unwrap().is_none());
        assert_eq!(buf.len(), whole.len() - 2, "nothing consumed yet");
    }

    #[test]
    fn no_reply_says_what_did_arrive() {
        assert_eq!(no_reply(1, &[], &[]), "BC cmd 1: no reply; the device sent nothing");
        assert!(no_reply(1, &[0xab, 0xcd], &[]).contains("2 unframed byte(s) arrived, starting abcd"));
        let push = Message::modern(cmd::CHANNEL_INFO_LIST, 0, Encryption::None, "<a/>");
        assert!(no_reply(1, &[], &[push]).contains("other messages arrived (145)"));
    }
}
