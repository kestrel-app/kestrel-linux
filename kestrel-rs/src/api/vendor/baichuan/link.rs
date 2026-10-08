//! The byte stream a [`Session`](super::session::Session) rides, and how to open one.
//!
//! Baichuan messages are the same whichever way they travel. Reached by UID they go
//! over the reliable-UDP [`Transport`]; reached at an address they go over a plain
//! TCP connection to the device's BC port (9000 unless changed), with TCP doing the
//! ordering and retransmission the UDP layer does by hand. neolink is built the same
//! way: one message codec over either source.
//!
//! **Verification status.** The UDP side is verified live (by UID, through a P2P
//! broker). The TCP side is exercised end to end against a fake device on loopback
//! — login, an AES command, and pushed video — but not yet against hardware: the
//! test NVR sits behind satellite NAT with no port open. See `docs/untested.md`.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use super::transport::Transport;
use crate::api::error::{Error, Result};

/// The device's default Baichuan port.
pub const DEFAULT_BC_PORT: u16 = 9000;

/// An ordered, reliable byte stream to a device.
pub trait Link: Send {
    /// Send bytes, returning once the link has taken responsibility for them.
    fn send(&mut self, bytes: &[u8]) -> Result<()>;
    /// Whatever has arrived since the last call, waiting briefly if nothing has.
    fn take_inbound(&mut self) -> Vec<u8>;
}

impl Link for Transport {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        Transport::send(self, bytes)
    }

    fn take_inbound(&mut self) -> Vec<u8> {
        Transport::take_inbound(self)
    }
}

/// How long [`Link::take_inbound`] waits when nothing is waiting — the same budget
/// the UDP transport gives its socket, so callers' polling loops behave alike.
const READ_WAIT: Duration = Duration::from_millis(200);

/// Baichuan over a TCP connection to the device.
pub struct TcpLink {
    stream: TcpStream,
}

impl TcpLink {
    pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpLink> {
        let addrs: Vec<SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|e| Error::connection(format!("{host}: {e}")))?
            .collect();
        let mut last = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(stream) => return TcpLink::over(stream),
                Err(e) => last = Some(e),
            }
        }
        Err(Error::connection(match last {
            Some(e) => format!("{host}:{port}: {e}"),
            None => format!("{host}: no address"),
        }))
    }

    fn over(stream: TcpStream) -> Result<TcpLink> {
        let setup = |e: std::io::Error| Error::connection(format!("BC over TCP: {e}"));
        // Commands are small and waited on; Nagle would only add latency.
        stream.set_nodelay(true).map_err(setup)?;
        stream.set_read_timeout(Some(READ_WAIT)).map_err(setup)?;
        Ok(TcpLink { stream })
    }
}

impl Link for TcpLink {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream
            .write_all(bytes)
            .map_err(|e| Error::connection(format!("BC over TCP send: {e}")))
    }

    fn take_inbound(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        let started = Instant::now();
        loop {
            match self.stream.read(&mut buf) {
                // The device closed the connection; what was read is all there is.
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    // Keep reading while a full buffer suggests more is queued, but
                    // never hold a video frame back for long.
                    if n < buf.len() || started.elapsed() > READ_WAIT {
                        break;
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        out
    }
}

/// How a device is reached: by its cloud UID, or at an address on its BC port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    Uid(String),
    Tcp { host: String, port: u16 },
}

impl Reach {
    /// Open the stream. Neither kind is logged in; that is [`super::login`]'s job.
    pub fn open(&self, timeout: Duration) -> Result<Box<dyn Link>> {
        Ok(match self {
            Reach::Uid(uid) => Box::new(Transport::connect(uid, timeout)?),
            Reach::Tcp { host, port } => Box::new(TcpLink::connect(host, *port, timeout)?),
        })
    }

    /// A name for logs and fallback device names.
    pub fn describe(&self) -> String {
        match self {
            Reach::Uid(uid) => uid.clone(),
            Reach::Tcp { host, port } => format!("{host}:{port}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Bytes written by the device come back in order, across several reads, and a
    /// quiet link returns empty after a short wait rather than blocking.
    #[test]
    fn tcp_link_carries_bytes_both_ways() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let device = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = [0u8; 5];
            s.read_exact(&mut got).unwrap();
            assert_eq!(&got, b"hello");
            s.write_all(b"wor").unwrap();
            s.flush().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            s.write_all(b"ld").unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });

        let mut link = TcpLink::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        link.send(b"hello").unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while got.len() < 5 && Instant::now() < deadline {
            got.extend(link.take_inbound());
        }
        assert_eq!(got, b"world");

        let quiet = Instant::now();
        assert!(link.take_inbound().is_empty());
        assert!(quiet.elapsed() < Duration::from_secs(1), "a quiet link does not block");
        device.join().unwrap();
    }

    #[test]
    fn a_refused_connection_is_a_connection_error() {
        // Bind then drop, so the port is very likely closed.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let err = TcpLink::connect("127.0.0.1", port, Duration::from_secs(1)).err().unwrap();
        assert!(matches!(err, Error::Connection(_)));
    }

    #[test]
    fn reach_describes_itself() {
        let tcp = Reach::Tcp { host: "192.0.2.10".into(), port: DEFAULT_BC_PORT };
        assert_eq!(tcp.describe(), "192.0.2.10:9000");
        assert_eq!(Reach::Uid("95270000ABCDEFGH".into()).describe(), "95270000ABCDEFGH");
    }
}
