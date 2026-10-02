//! Request/reply over an authenticated control stream.
//!
//! [`transport::Transport`] gives an ordered byte stream; [`wire::Message`] frames
//! one command on it. This ties them together: send a command, read whole messages
//! off the stream until the matching reply arrives, and hand it back.
//!
//! **Verification status.** The message accumulate-and-drain logic ([`drain_one`])
//! is pure and tested below, and the header/crypto it relies on were confirmed
//! against a real device. [`Session::call`] drives the live transport, which is
//! verified up to the login nonce but whose full in-Rust driver is not yet wired —
//! see [`super`] and `docs/untested.md`.

use std::time::Duration;

use super::transport::Transport;
use super::wire::{Encryption, Message};
use crate::api::error::{Error, Result};

/// An authenticated Baichuan control session.
pub struct Session {
    transport: Transport,
    /// How bodies are encrypted. Starts [`Encryption::None`]; login raises it (to
    /// BCEncrypt, or AES on newer firmware once that is implemented).
    encryption: Encryption,
    /// The BCEncrypt offset to try when decrypting replies. The device's login
    /// reply uses 0.
    decrypt_offset: u32,
    /// Monotonic message number, echoed by the device so replies can be matched.
    msg_num: u16,
    /// Buffered stream bytes not yet framed into a message.
    buffer: Vec<u8>,
}

impl Session {
    pub fn new(transport: Transport) -> Self {
        Session {
            transport,
            encryption: Encryption::None,
            decrypt_offset: 0,
            msg_num: 0,
            buffer: Vec::new(),
        }
    }

    pub fn set_encryption(&mut self, encryption: Encryption) {
        self.encryption = encryption;
    }

    pub fn encryption(&self) -> Encryption {
        self.encryption
    }

    fn next_msg_num(&mut self) -> u16 {
        self.msg_num = self.msg_num.wrapping_add(1);
        self.msg_num
    }

    /// Send a prepared message and wait for a reply bearing the same `msg_id`.
    pub fn send(&mut self, request: Message, timeout: Duration) -> Result<Message> {
        let want = request.msg_id;
        self.transport.send(&request.to_bytes())?;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(msg) = drain_one(&mut self.buffer, self.decrypt_offset)? {
                if msg.msg_id == want {
                    return Ok(msg);
                }
                continue; // an unrelated message (e.g. a push); keep reading
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::connection(format!("BC cmd {want}: no reply")));
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
}

/// Pull the first whole control message out of `buffer`, removing its bytes.
/// `Ok(None)` when the buffer does not yet hold a complete message.
pub fn drain_one(buffer: &mut Vec<u8>, decrypt_offset: u32) -> Result<Option<Message>> {
    match Message::parse(buffer, decrypt_offset)? {
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

        let first = drain_one(&mut buf, 0).unwrap().unwrap();
        assert_eq!(first.msg_id, cmd::LOGIN);
        let second = drain_one(&mut buf, 0).unwrap().unwrap();
        assert_eq!(second.msg_id, cmd::PING);
        assert!(drain_one(&mut buf, 0).unwrap().is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn a_partial_message_is_left_in_the_buffer() {
        let whole = Message::modern(cmd::PING, 1, Encryption::None, "<b/>").to_bytes();
        let mut buf = whole[..whole.len() - 2].to_vec();
        assert!(drain_one(&mut buf, 0).unwrap().is_none());
        assert_eq!(buf.len(), whole.len() - 2, "nothing consumed yet");
    }
}
