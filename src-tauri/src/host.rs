//! Host role: own the input, decide where the pointer is, feed the peers.
//!
//! Shape of the thing:
//!
//! * one task accepts TCP connections and runs the handshake,
//! * one task per peer owns that peer's socket writer,
//! * one task drains the capture channel and drives the wall.
//!
//! The capture task never awaits anything but a channel send, so an input event
//! reaches the network in a couple of microseconds of our own code. Each peer
//! gets its own unbounded channel: a peer on a bad WiFi link cannot slow the
//! pointer down for anybody else.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::capture::{self, Capture, CaptureEvent};
use crate::clipboard::Clipboard;
use crate::crypto::{random_bytes, secret_from_code, Secret};
use crate::displays;
use crate::inject;
use crate::layout::{Device, LayoutView, Motion, Wall};
use crate::protocol::{
    id_to_hex, Clip, ClientMsg, DeviceInfo, HostMsg, InputEvent, DEFAULT_PORT,
};
use crate::settings::{self, Settings};
use crate::transport::{
    host_finish, host_read_intent, AuthMode, FrameReader, FrameWriter, TransportError,
};

/// How often the host re-checks its own monitor layout.
const DISPLAY_POLL: Duration = Duration::from_secs(1);
/// Latency probe interval. Frequent enough for a live number in the UI, rare
/// enough to be invisible on the wire.
const PING_EVERY: Duration = Duration::from_secs(2);
/// A peer that connects and then says nothing gets dropped, so a half-open
/// socket cannot sit in the pairing window forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Everything the host tasks share.
pub struct Host {
    pub state: Arc<Shared>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// A connected peer, from the host's point of view.
pub struct Peer {
    pub info: DeviceInfo,
    tx: UnboundedSender<HostMsg>,
    /// Round-trip time in milliseconds, or `None` before the first pong.
    pub latency_ms: Option<u32>,
    /// Which connection this is. A device that reconnects replaces its own
    /// entry, and this is how the older connection knows not to clean up after
    /// the newer one.
    session: u64,
}

/// Shared state. Locks are always taken in this order — wall, peers, settings —
/// so two of them can never be waiting on each other.
pub struct Shared {
    pub wall: Mutex<Wall>,
    pub peers: Mutex<HashMap<String, Peer>>,
    pub settings: Mutex<Settings>,
    /// Set while the host is willing to pair with an unknown device.
    pub accepting: Arc<AtomicBool>,
    pub clipboard: Option<Clipboard>,
    /// Bumped whenever the UI should refresh.
    pub dirty: Arc<AtomicBool>,
    /// Set the moment the host starts shutting down. Peer tasks are not owned by
    /// `Host` — they are spawned per connection — so this plus `stop` is how they
    /// are told to let go, rather than waiting on a client to notice.
    stopping: AtomicBool,
    stop: tokio::sync::Notify,
}

impl Shared {
    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Sends to one peer by hex id, if it is connected.
    fn send_hex(&self, device_hex: &str, msg: HostMsg) {
        if let Some(peer) = self.peers.lock().get(device_hex) {
            let _ = peer.tx.send(msg);
        }
    }

    fn broadcast(&self, msg: HostMsg) {
        for peer in self.peers.lock().values() {
            let _ = peer.tx.send(msg.clone());
        }
    }

    pub fn layout_view(&self) -> LayoutView {
        self.wall.lock().view()
    }

    /// Pulls the pointer back to this machine. Shared by the escape hotkey, the
    /// UI button and any peer that vanishes while holding it, because all three
    /// have to do the same four things in the same order.
    pub fn take_pointer_back(&self) {
        let step = {
            let mut wall = self.wall.lock();
            if wall.is_local() {
                None
            } else {
                let from = id_to_hex(&wall.devices[wall.owner()].id);
                wall.take_local();
                let (x, y) = wall.owner_local();
                Some((from, x, y))
            }
        };
        let Some((from, x, y)) = step else {
            return;
        };
        // Release first: the peer must not be left holding a key it can no
        // longer be told about.
        self.send_hex(&from, HostMsg::Input(InputEvent::ReleaseAll));
        self.send_hex(&from, HostMsg::Leave);
        // Warp before un-suppressing, or the first local event lands wherever
        // the cursor was parked when control left.
        inject::warp(x, y);
        capture::set_suppressed(false);
        self.mark_dirty();
    }

    /// Cuts a peer off with a reason it can show the user.
    ///
    /// Only the goodbye is sent here: queueing it closes the writer, the reader
    /// then fails, and the usual per-connection cleanup runs. Removing the peer
    /// from here instead would race that cleanup for the same entry.
    pub fn disconnect_peer(&self, device_hex: &str, reason: &str) {
        self.send_hex(
            device_hex,
            HostMsg::Bye {
                reason: reason.to_string(),
            },
        );
    }

    /// The secret to authenticate a connecting client with.
    ///
    /// A stored key is tried first so a paired device reconnects silently; the
    /// pairing code only applies while the host is accepting new devices.
    fn auth_for(&self, device_hex: &str, mode: AuthMode) -> Option<Secret> {
        let settings = self.settings.lock();
        match mode {
            AuthMode::Stored => settings.paired_clients.get(device_hex).copied(),
            AuthMode::Code => self
                .accepting
                .load(Ordering::Relaxed)
                .then(|| secret_from_code(&settings.pairing_code)),
        }
    }
}

impl Host {
    /// Starts listening and capturing. Capture failing is fatal for the host
    /// role — without it there is nothing to share.
    ///
    /// `dirty` is the app-wide "the UI is stale" flag, passed in rather than
    /// created here so one timer can watch both roles.
    pub async fn start(settings: Settings, dirty: Arc<AtomicBool>) -> Result<Self, String> {
        let displays = displays::enumerate();
        let id = settings.id();
        let name = settings.device_name.clone();

        let mut wall = Wall::new(Device::new(id, name.clone(), displays));
        // Put remembered devices back where the user left them, before any of
        // them reconnect, so the layout does not jump around on start-up.
        let saved = settings.layout.clone();

        let clips = settings
            .clipboard_sync
            .then(|| Clipboard::start(settings.clipboard_images));
        let (clipboard, clip_rx) = match clips {
            Some((board, rx)) => (Some(board), Some(rx)),
            None => (None, None),
        };

        // Line the host's wall position up with the real cursor straight away.
        if let Some((x, y)) = inject::cursor_position() {
            wall.sync_local(x, y);
        }

        let state = Arc::new(Shared {
            wall: Mutex::new(wall),
            peers: Mutex::new(HashMap::new()),
            settings: Mutex::new(settings),
            accepting: Arc::new(AtomicBool::new(true)),
            clipboard,
            dirty,
            stopping: AtomicBool::new(false),
            stop: tokio::sync::Notify::new(),
        });

        let listener = TcpListener::bind(("0.0.0.0", DEFAULT_PORT))
            .await
            .map_err(|e| format!("could not listen on port {DEFAULT_PORT}: {e}"))?;

        let capture = capture::start().map_err(|e| e.to_string())?;

        let mut tasks = Vec::new();
        tasks.push(spawn_accept(listener, state.clone(), saved));
        tasks.push(spawn_capture(capture, state.clone()));
        tasks.push(spawn_display_watch(state.clone()));
        if let Some(clip_rx) = clip_rx {
            tasks.push(spawn_clipboard(clip_rx, state.clone()));
        }
        tasks.push(crate::discovery::serve_announcements(
            id,
            name,
            settings::os_label(),
            DEFAULT_PORT,
            state.accepting.clone(),
        ));

        Ok(Self { state, tasks })
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // Hand the pointer back before anything else unwinds, or the local
        // machine is left with its input suppressed and no way to fix it.
        capture::set_suppressed(false);

        // Queue a goodbye, then release the peer tasks. They are spawned per
        // connection rather than owned here, so they have to be told; the writer
        // tasks get a moment to flush the Bye before they go.
        self.state.broadcast(HostMsg::Bye {
            reason: "host stopped".into(),
        });
        self.state.stopping.store(true, Ordering::Relaxed);
        self.state.stop.notify_waiters();

        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

/// Milliseconds since the first call. Used as the ping stamp: only differences
/// matter, and a monotonic base means a clock change cannot show up as latency.
fn now_stamp() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Distinguishes one connection from the next for the same device, so a
/// reconnect cannot have its predecessor's cleanup tear it down.
static SESSION: AtomicU64 = AtomicU64::new(1);

fn spawn_accept(
    listener: TcpListener,
    state: Arc<Shared>,
    saved: HashMap<String, (i32, i32)>,
) -> tokio::task::JoinHandle<()> {
    let saved = Arc::new(saved);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _addr)) = listener.accept().await else {
                // Accept only fails for good on a broken listener; a transient
                // per-connection error should not end the host.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let state = state.clone();
            let saved = saved.clone();
            tokio::spawn(async move {
                // Nothing to report to the peer: a failed handshake means we
                // have no channel to report it over.
                let _ = serve_peer(stream, state, saved).await;
            });
        }
    })
}

/// One connection, from the first byte to the last.
async fn serve_peer(
    mut stream: TcpStream,
    state: Arc<Shared>,
    saved: Arc<HashMap<String, (i32, i32)>>,
) -> Result<(), TransportError> {
    // A peer that opens a socket and says nothing must not hold the pairing
    // window open, so the intro is the only part that gets a deadline.
    let intent = tokio::time::timeout(HANDSHAKE_TIMEOUT, host_read_intent(&mut stream))
        .await
        .map_err(|_| TransportError::Handshake)??;

    let device_hex = id_to_hex(&intent.device);
    let auth = state
        .auth_for(&device_hex, intent.mode)
        .ok_or(TransportError::Rejected)?;

    let (mut reader, mut writer) = host_finish(stream, &intent, &auth).await?;

    let ClientMsg::Hello { device } = reader.recv().await? else {
        return Err(TransportError::Handshake);
    };
    // The id is authenticated by the handshake, so a client claiming to be
    // somebody else in its Hello is not something to take at face value.
    if device.id != intent.device {
        return Err(TransportError::Handshake);
    }

    // First contact: mint a long-term key so the code is only typed once.
    let psk = (intent.mode == AuthMode::Code).then(random_bytes::<32>);
    let clipboard_sync = {
        let mut settings = state.settings.lock();
        if let Some(psk) = psk {
            settings.paired_clients.insert(device_hex.clone(), psk);
            settings.save();
        }
        settings.clipboard_sync
    };

    let host_info = {
        let wall = state.wall.lock();
        let host = wall.host();
        DeviceInfo {
            id: host.id,
            name: host.name.clone(),
            os: settings::os_label(),
            displays: host.displays.clone(),
        }
    };
    writer
        .send(&HostMsg::Welcome {
            host: host_info,
            psk,
            clipboard_sync,
        })
        .await?;

    let session = SESSION.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = unbounded_channel();

    {
        let mut wall = state.wall.lock();
        let idx = wall.upsert(device.id, device.name.clone(), device.displays.clone());
        // A device the user has positioned before goes back where they left it,
        // rather than to the end of the row.
        if let Some(&(x, y)) = saved.get(&device_hex) {
            wall.set_offset(idx, x, y);
        }
    }

    state.peers.lock().insert(
        device_hex.clone(),
        Peer {
            info: device,
            tx,
            latency_ms: None,
            session,
        },
    );
    state.mark_dirty();

    // The writer half owns the socket from here; this task keeps reading.
    let write_task = tokio::spawn(peer_writer(writer, rx));
    let ping_task = spawn_pings(state.clone(), device_hex.clone());

    // Either the peer goes away or the host shuts down. Without the second arm
    // this task would outlive the `Host` that spawned it.
    let result = tokio::select! {
        result = peer_reader(&mut reader, &state, &device_hex) => result,
        () = wait_for_stop(&state) => Ok(()),
    };

    ping_task.abort();
    // Drop the peer entry first: that closes the writer's queue, so it flushes
    // whatever is still pending — including the goodbye — and then stops on its
    // own. The timeout is there for a peer whose socket has stopped draining.
    disconnect(&state, &device_hex, session);
    let _ = tokio::time::timeout(Duration::from_millis(200), write_task).await;
    result
}

/// Resolves when the host is shutting down.
async fn wait_for_stop(state: &Arc<Shared>) {
    // Register interest before the check, so a stop between the two is not lost.
    let notified = state.stop.notified();
    if state.stopping.load(Ordering::Relaxed) {
        return;
    }
    notified.await;
}

/// Drops a peer, but only if the entry still belongs to this connection.
fn disconnect(state: &Arc<Shared>, device_hex: &str, session: u64) {
    {
        let mut peers = state.peers.lock();
        match peers.get(device_hex) {
            // A newer connection for the same device already took over.
            Some(peer) if peer.session != session => return,
            None => return,
            _ => {}
        }
        peers.remove(device_hex);
    }

    // Take the pointer back first if this peer had it, then mark it offline so
    // it cannot be crossed onto while it is gone.
    let had_pointer = {
        let wall = state.wall.lock();
        !wall.is_local() && id_to_hex(&wall.devices[wall.owner()].id) == device_hex
    };
    if had_pointer {
        state.take_pointer_back();
    }
    {
        let mut wall = state.wall.lock();
        if let Some(idx) = wall.devices.iter().position(|d| id_to_hex(&d.id) == device_hex) {
            wall.set_online(idx, false);
        }
    }
    state.mark_dirty();
}

/// Owns the socket's write half. One task per peer, so a slow link only ever
/// blocks its own queue.
async fn peer_writer(mut writer: FrameWriter, mut rx: UnboundedReceiver<HostMsg>) {
    while let Some(msg) = rx.recv().await {
        let bye = matches!(msg, HostMsg::Bye { .. });
        if writer.send(&msg).await.is_err() {
            break;
        }
        if bye {
            break;
        }
    }
    writer.shutdown().await;
}

async fn peer_reader(
    reader: &mut FrameReader,
    state: &Arc<Shared>,
    device_hex: &str,
) -> Result<(), TransportError> {
    loop {
        match reader.recv::<ClientMsg>().await? {
            ClientMsg::DisplaysChanged { displays } => {
                {
                    let mut wall = state.wall.lock();
                    if let Some(idx) = wall
                        .devices
                        .iter()
                        .position(|d| id_to_hex(&d.id) == device_hex)
                    {
                        wall.devices[idx].set_displays(displays.clone());
                    }
                }
                if let Some(peer) = state.peers.lock().get_mut(device_hex) {
                    peer.info.displays = displays;
                }
                state.mark_dirty();
            }
            ClientMsg::Clipboard(clip) => {
                // Apply locally, then fan out to the other peers so every
                // device ends up with the same clipboard.
                if let Some(board) = state.clipboard.as_ref() {
                    board.apply(clip.clone());
                }
                for (id, peer) in state.peers.lock().iter() {
                    if id != device_hex {
                        let _ = peer.tx.send(HostMsg::Clipboard(clip.clone()));
                    }
                }
            }
            ClientMsg::Pong { stamp } => {
                let rtt = now_stamp().saturating_sub(stamp);
                if let Some(peer) = state.peers.lock().get_mut(device_hex) {
                    peer.latency_ms = Some(rtt.min(u32::MAX as u64) as u32);
                }
                state.mark_dirty();
            }
            ClientMsg::Hello { .. } => {
                // Only legal once, during the handshake.
                return Err(TransportError::Handshake);
            }
            ClientMsg::Bye { .. } => return Ok(()),
        }
    }
}

/// Latency probe. Cheap enough to leave running: two small frames every two
/// seconds per peer.
fn spawn_pings(state: Arc<Shared>, device_hex: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PING_EVERY);
        loop {
            ticker.tick().await;
            let sent = {
                match state.peers.lock().get(&device_hex) {
                    Some(peer) => peer
                        .tx
                        .send(HostMsg::Ping {
                            stamp: now_stamp(),
                        })
                        .is_ok(),
                    None => false,
                }
            };
            if !sent {
                return;
            }
        }
    })
}

/// Drains the capture channel and drives the wall.
///
/// This is the latency-critical path, so it is deliberately plain: no awaits
/// except the channel receive, no allocation per event, and the peer's queue is
/// unbounded so a send never blocks.
fn spawn_capture(mut capture: Capture, state: Arc<Shared>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // How many mouse buttons are down. Non-zero means a drag is in flight.
        let mut held: u8 = 0;

        while let Some(event) = capture.events.recv().await {
            match event {
                CaptureEvent::Motion { dx, dy } => on_motion(&state, dx, dy),
                CaptureEvent::MotionAbs { x, y } => {
                    // Only meaningful while control is local; `sync_local`
                    // ignores it otherwise. This is what keeps the wall in step
                    // with anything else that moves the cursor.
                    state.wall.lock().sync_local(x, y);
                }
                CaptureEvent::Button { button, down } => {
                    // A drag pins the pointer: releasing a button on a
                    // different machine from where it was pressed leaves the
                    // original one stuck mid-drag.
                    held = if down {
                        held.saturating_add(1)
                    } else {
                        held.saturating_sub(1)
                    };
                    state.wall.lock().set_locked(held > 0);
                    forward(&state, InputEvent::Button { button, down });
                }
                CaptureEvent::Wheel { dx, dy } => forward(&state, InputEvent::Wheel { dx, dy }),
                CaptureEvent::Key {
                    vk,
                    scan,
                    ext,
                    down,
                } => forward(
                    &state,
                    InputEvent::Key {
                        vk,
                        scan,
                        ext,
                        down,
                    },
                ),
                CaptureEvent::Escape => {
                    held = 0;
                    state.wall.lock().set_locked(false);
                    state.take_pointer_back();
                }
            }
        }
        // Dropping the capture handle unhooks and un-suppresses; the channel
        // only ends when the capture thread is already gone, but be explicit.
        drop(capture);
    })
}

/// Feeds one motion sample into the wall and does whatever it asks for.
fn on_motion(state: &Arc<Shared>, dx: i32, dy: i32) {
    // Resolve everything under one lock, then do the I/O outside it.
    let (motion, from, to, host_hex) = {
        let mut wall = state.wall.lock();
        let before = wall.owner();
        let motion = wall.move_by(dx, dy);
        let after = wall.owner();
        (
            motion,
            id_to_hex(&wall.devices[before].id),
            id_to_hex(&wall.devices[after].id),
            id_to_hex(&wall.host().id),
        )
    };

    match motion {
        // Nothing to do: the real cursor moved on its own. Warping here would
        // fight the OS and add a frame of jitter.
        Motion::Local { .. } => {}
        Motion::Remote { x, y, .. } => {
            state.send_hex(&to, HostMsg::Input(InputEvent::MoveAbs { x, y }));
        }
        Motion::Switch { x, y, .. } => {
            if from != host_hex {
                // Let go of whatever the old owner is holding before it stops
                // hearing from us, then tell it control has left.
                state.send_hex(&from, HostMsg::Input(InputEvent::ReleaseAll));
                state.send_hex(&from, HostMsg::Leave);
            }

            if to == host_hex {
                // Coming home: put the real cursor where the wall says before
                // letting local input through, or the first event lands
                // wherever the cursor was parked when control left.
                inject::warp(x, y);
                capture::set_suppressed(false);
            } else {
                // Going out: swallow local input first, so nothing leaks into
                // the host's own windows during the crossing.
                capture::set_suppressed(true);
                state.send_hex(&to, HostMsg::Enter { x, y });
            }
            state.mark_dirty();
        }
    }
}

/// Sends a non-motion event to whoever owns the pointer. Events that belong to
/// the host are left alone: the OS already delivered them.
fn forward(state: &Arc<Shared>, event: InputEvent) {
    let target = {
        let wall = state.wall.lock();
        if wall.is_local() {
            None
        } else {
            Some(id_to_hex(&wall.devices[wall.owner()].id))
        }
    };
    if let Some(target) = target {
        state.send_hex(&target, HostMsg::Input(event));
    }
}

/// Watches the host's own monitors. Docking a laptop or unplugging a screen
/// changes the wall, and a stale wall means the pointer crosses in the wrong
/// place — worth one cheap syscall a second.
fn spawn_display_watch(state: Arc<Shared>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(DISPLAY_POLL);
        let mut last = {
            let wall = state.wall.lock();
            wall.host().displays.clone()
        };

        loop {
            ticker.tick().await;
            let current = displays::enumerate();
            if current == last {
                continue;
            }
            last = current.clone();

            {
                let mut wall = state.wall.lock();
                wall.devices[0].set_displays(current);
                // The cursor may now be outside every monitor.
                if wall.is_local() {
                    if let Some((x, y)) = inject::cursor_position() {
                        wall.sync_local(x, y);
                    }
                }
            }
            state.mark_dirty();
        }
    })
}

/// Fans local clipboard changes out to every peer.
fn spawn_clipboard(
    mut changes: UnboundedReceiver<Clip>,
    state: Arc<Shared>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(clip) = changes.recv().await {
            state.broadcast(HostMsg::Clipboard(clip));
        }
    })
}
