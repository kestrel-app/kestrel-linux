//! One Baichuan connection per device, shared by every tile showing it.
//!
//! Reolink's own app opens a single connection to an NVR and asks it for each
//! channel's video over that one session. Opening a session per tile instead
//! costs a login per tile and leans on however many sessions the device is
//! willing to hold — on a real NVR at an address some tiles' sessions failed and
//! went to RTSP. Measured on an RLN16-410: three Previews on one session, each
//! with its own `handle`, stream side by side, and every video message carries the
//! message number of the Preview it answers. That number is what routes it here.
//!
//! A [`Subscription`] is one tile's stream. The hub's thread owns the session:
//! it sends each Preview, routes the video to the right subscriber, and sends
//! `VIDEO_STOP` when a subscriber goes. The last subscriber to go takes the
//! connection with it. If the connection fails, every subscriber's stream ends,
//! and the next subscription opens a fresh one.
//!
//! A [`Fetch`] — downloading or replaying a recording — rides the same connection.
//! It used to open one of its own, and an NVR at an address that was already
//! streaming the wall never answered that second login.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use log::{info, warn};

use super::link::Reach;
use super::media::{VideoCodec, VideoStream};
use super::session::Session;
use super::video::preview_body;
use super::wire::Message;
use super::{cmd, login};
use crate::api::error::{Error, Result};
use crate::api::models::StreamType;

/// One tile's stream from a shared connection.
pub struct Subscription {
    /// Elementary-stream chunks, in order. Ends when the stream or connection does.
    pub bytes: Receiver<Vec<u8>>,
    /// The codec, sent once, when the first keyframe has arrived.
    pub codec: Receiver<VideoCodec>,
    /// Raise to give the stream up; the hub stops it on the device.
    pub closed: Arc<AtomicBool>,
    /// Holds the connection open while this stream is wanted.
    pub hub: Arc<Hub>,
}

struct Start {
    channel: u32,
    stream: StreamType,
    bytes: Sender<Vec<u8>>,
    codec: SyncSender<VideoCodec>,
    closed: Arc<AtomicBool>,
}

/// What a fetch hears back: footage, or a bodiless status (a download's closing
/// `300`, or a refusal).
pub enum FetchEvent {
    Data(Vec<u8>),
    Status(u16),
}

/// A recording being fetched over the shared connection.
pub struct Fetch {
    pub events: Receiver<FetchEvent>,
    closed: Arc<AtomicBool>,
    /// Holds the connection open while the fetch runs.
    _hub: Arc<Hub>,
}

impl Drop for Fetch {
    /// Gone before the device finished: the hub sends the stop.
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}

struct FetchStart {
    msg_id: u32,
    stop_id: u32,
    channel: u32,
    body: String,
    events: Sender<FetchEvent>,
    closed: Arc<AtomicBool>,
}

/// A command sent over the shared connection, and where its reply goes.
struct CallStart {
    msg_id: u32,
    body: String,
    reply: Sender<Message>,
}

enum Order {
    Live(Start),
    Fetch(FetchStart),
    Call(CallStart),
}

/// A live connection to one device. Dropped with its last subscriber.
pub struct Hub {
    starts: Mutex<Sender<Order>>,
    /// Raised by the hub's thread when the connection has failed.
    dead: Arc<AtomicBool>,
    /// Raised when the hub is dropped, telling its thread to close the connection.
    quit: Arc<AtomicBool>,
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
    }
}

/// Live hubs by device, each behind its own lock so one device's login does not
/// hold up another's.
fn registry() -> &'static Mutex<HashMap<String, Arc<Mutex<Weak<Hub>>>>> {
    static HUBS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Weak<Hub>>>>>> = OnceLock::new();
    HUBS.get_or_init(Default::default)
}

/// The device's shared connection: the open one, or a fresh one logged in.
fn hub_for(reach: &Reach, username: &str, password: &str, timeout: Duration) -> Result<Arc<Hub>> {
    let key = format!("{}|{username}", reach.describe());
    let slot = Arc::clone(registry().lock().unwrap().entry(key).or_default());
    // Held across the connect, so tiles of one device arriving together wait for
    // the one login rather than each starting their own.
    let mut current = slot.lock().unwrap();
    if let Some(hub) = current.upgrade().filter(|h| !h.dead.load(Ordering::Relaxed)) {
        return Ok(hub);
    }
    let hub = Hub::connect(reach, username, password, timeout)?;
    *current = Arc::downgrade(&hub);
    Ok(hub)
}

/// Keep the device's shared connection open while the returned handle lives —
/// for something that asks a run of requests (a player seeking, a fetch falling
/// back from download to replay), so the connection does not close between one
/// request ending and the next starting. A device that accepts a single
/// connection may not answer the login a fresh one needs.
pub fn hold(reach: &Reach, username: &str, password: &str, timeout: Duration) -> Result<Arc<Hub>> {
    hub_for(reach, username, password, timeout)
}

/// Request/reply commands over a device's shared connection — a search, the
/// calendar — for a device that will not take a second login while the wall is
/// streaming from it.
pub struct Caller {
    hub: Arc<Hub>,
}

impl Caller {
    pub fn open(reach: &Reach, username: &str, password: &str, timeout: Duration) -> Result<Caller> {
        Ok(Caller { hub: hub_for(reach, username, password, timeout)? })
    }
}

impl super::session::Calls for Caller {
    fn call(&mut self, msg_id: u32, xml: String, timeout: Duration) -> Result<Message> {
        let (reply, replies) = mpsc::channel();
        self.hub
            .starts
            .lock()
            .unwrap()
            .send(Order::Call(CallStart { msg_id, body: xml, reply }))
            .map_err(|_| Error::connection("baichuan connection has closed"))?;
        replies
            .recv_timeout(timeout)
            .map_err(|_| Error::connection(format!("BC cmd {msg_id}: no reply")))
    }
}

/// Subscribe to one channel's video, sharing the device's connection if one is
/// open and opening (and logging in) one if not.
pub fn subscribe(
    reach: &Reach,
    username: &str,
    password: &str,
    channel: u32,
    stream: StreamType,
    timeout: Duration,
) -> Result<Subscription> {
    hub_for(reach, username, password, timeout)?.start(channel, stream)
}

/// Send one recording request (`msg_id`, with `body`) over the device's shared
/// connection and hear what answers it. Dropping the [`Fetch`] early sends
/// `stop_id`.
#[allow(clippy::too_many_arguments)]
pub fn fetch(
    reach: &Reach,
    username: &str,
    password: &str,
    msg_id: u32,
    stop_id: u32,
    channel: u32,
    body: String,
    timeout: Duration,
) -> Result<Fetch> {
    let hub = hub_for(reach, username, password, timeout)?;
    let (events_tx, events) = mpsc::channel();
    let closed = Arc::new(AtomicBool::new(false));
    hub.starts
        .lock()
        .unwrap()
        .send(Order::Fetch(FetchStart {
            msg_id,
            stop_id,
            channel,
            body,
            events: events_tx,
            closed: Arc::clone(&closed),
        }))
        .map_err(|_| Error::connection("baichuan connection has closed"))?;
    Ok(Fetch { events, closed, _hub: hub })
}

impl Hub {
    fn connect(reach: &Reach, username: &str, password: &str, timeout: Duration) -> Result<Arc<Hub>> {
        let link = reach.open(timeout)?;
        let mut session = Session::new(link);
        login::login(&mut session, username, password, timeout)?;
        info!("baichuan: one connection open to {} for its video", reach.describe());

        let (starts, incoming) = mpsc::channel();
        let dead = Arc::new(AtomicBool::new(false));
        let quit = Arc::new(AtomicBool::new(false));
        let (thread_dead, thread_quit) = (Arc::clone(&dead), Arc::clone(&quit));
        let name = reach.describe();
        std::thread::Builder::new()
            .name("bc-hub".into())
            .spawn(move || {
                if let Err(err) = run(&mut session, &incoming, &thread_quit) {
                    warn!("baichuan connection to {name} ended: {err}");
                }
                thread_dead.store(true, Ordering::Relaxed);
            })
            .map_err(|e| Error::connection(format!("baichuan: {e}")))?;
        Ok(Arc::new(Hub { starts: Mutex::new(starts), dead, quit }))
    }

    fn start(self: Arc<Self>, channel: u32, stream: StreamType) -> Result<Subscription> {
        let (bytes_tx, bytes) = mpsc::channel();
        let (codec_tx, codec) = mpsc::sync_channel(1);
        let closed = Arc::new(AtomicBool::new(false));
        self.starts
            .lock()
            .unwrap()
            .send(Order::Live(Start {
                channel,
                stream,
                bytes: bytes_tx,
                codec: codec_tx,
                closed: Arc::clone(&closed),
            }))
            .map_err(|_| Error::connection("baichuan connection has closed"))?;
        Ok(Subscription { bytes, codec, closed, hub: self })
    }
}

/// One stream being carried for a subscriber.
struct Carried {
    channel: u32,
    stream: StreamType,
    handle: u32,
    demux: VideoStream,
    bytes: Sender<Vec<u8>>,
    codec: Option<SyncSender<VideoCodec>>,
    closed: Arc<AtomicBool>,
}

fn preview(session: &mut Session, num: u16, msg_id: u32, c: &Carried) -> Result<()> {
    let mut msg = Message::modern(msg_id, num, session.encryption(), preview_body(c.channel, c.handle, c.stream));
    msg.channel_id = c.channel as u8;
    msg.stream_type = u8::from(c.stream == StreamType::Sub);
    session.send_oneway(msg)
}

/// The hub's thread: start and stop streams as asked, and route the video.
/// A fetch being carried: where its answers go.
struct Fetching {
    msg_id: u32,
    stop_id: u32,
    channel: u32,
    events: Sender<FetchEvent>,
    closed: Arc<AtomicBool>,
    finished: bool,
}

fn run(session: &mut Session, incoming: &Receiver<Order>, quit: &AtomicBool) -> Result<()> {
    // Keyed by the message number its request went out with, which its answers echo.
    let mut carried: HashMap<u16, Carried> = HashMap::new();
    let mut fetching: HashMap<u16, Fetching> = HashMap::new();
    let mut calls: HashMap<u16, (u32, Sender<Message>)> = HashMap::new();
    let mut next_handle = 0u32;
    while !quit.load(Ordering::Relaxed) {
        while let Ok(order) = incoming.try_recv() {
            let start = match order {
                Order::Live(start) => start,
                Order::Call(c) => {
                    let num = session.next_msg_num();
                    session.send_oneway(Message::modern(c.msg_id, num, session.encryption(), c.body))?;
                    calls.insert(num, (c.msg_id, c.reply));
                    continue;
                }
                Order::Fetch(f) => {
                    let num = session.next_msg_num();
                    let mut msg = Message::modern(f.msg_id, num, session.encryption(), f.body);
                    msg.channel_id = f.channel as u8;
                    session.send_oneway(msg)?;
                    fetching.insert(
                        num,
                        Fetching {
                            msg_id: f.msg_id,
                            stop_id: f.stop_id,
                            channel: f.channel,
                            events: f.events,
                            closed: f.closed,
                            finished: false,
                        },
                    );
                    continue;
                }
            };
            let c = Carried {
                channel: start.channel,
                stream: start.stream,
                handle: next_handle,
                demux: VideoStream::new(),
                bytes: start.bytes,
                codec: Some(start.codec),
                closed: start.closed,
            };
            next_handle = next_handle.wrapping_add(1);
            let num = session.next_msg_num();
            preview(session, num, cmd::VIDEO, &c)?;
            carried.insert(num, c);
        }

        // Stop what nobody is watching any more.
        let gone: Vec<u16> = carried
            .iter()
            .filter(|(_, c)| c.closed.load(Ordering::Relaxed))
            .map(|(num, _)| *num)
            .collect();
        for num in gone {
            if let Some(c) = carried.remove(&num) {
                let stop = session.next_msg_num();
                preview(session, stop, cmd::VIDEO_STOP, &c)?;
            }
        }

        // Fetches the device finished, or whose asker went: stop the latter.
        let done: Vec<u16> = fetching
            .iter()
            .filter(|(_, f)| f.finished || f.closed.load(Ordering::Relaxed))
            .map(|(num, _)| *num)
            .collect();
        for num in done {
            if let Some(f) = fetching.remove(&num) {
                if !f.finished {
                    let mut stop = Message::modern(f.stop_id, session.next_msg_num(), session.encryption(), String::new());
                    stop.channel_id = f.channel as u8;
                    session.send_oneway(stop)?;
                }
            }
        }

        session.decode_routed(|header, body, cipher| {
            let (id, num, code) = (header.msg_id, header.msg_num, header.response_code);
            // A command's reply, matched by the number it was sent with.
            if calls.get(&num).is_some_and(|(want, _)| *want == id) {
                let (_, reply) = calls.remove(&num).unwrap();
                let _ = reply.send(Message {
                    msg_id: id,
                    channel_id: header.channel_id,
                    stream_type: 0,
                    msg_num: num,
                    response_code: code,
                    class: header.class,
                    encryption: cipher,
                    body: if body.is_empty() { Vec::new() } else { cipher.decrypt(body) },
                    extension_only: false,
                });
                return;
            }
            if header.class != 0 {
                return;
            }
            if id == cmd::VIDEO {
                if let Some(c) = carried.get_mut(&num) {
                    c.demux.push(&super::video::media_of(header, body, cipher));
                }
                return;
            }
            let Some(f) = fetching.get_mut(&num).filter(|f| f.msg_id == id) else { return };
            let event = if body.is_empty() {
                // 300 is a download saying it is all sent; 400 and up a refusal.
                if code == 300 || code >= 400 {
                    f.finished = true;
                }
                FetchEvent::Status(code)
            } else {
                FetchEvent::Data(super::video::media_of(header, body, cipher))
            };
            if f.events.send(event).is_err() {
                f.closed.store(true, Ordering::Relaxed);
            }
        })?;
        for c in carried.values_mut() {
            let out = c.demux.take();
            if out.is_empty() {
                continue;
            }
            if let Some(codec) = c.demux.codec() {
                if let Some(tx) = c.codec.take() {
                    let _ = tx.send(codec);
                }
            }
            if c.bytes.send(out).is_err() {
                c.closed.store(true, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}
