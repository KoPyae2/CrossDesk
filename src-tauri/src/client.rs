//! Client role: connect to a host and replay whatever it sends.
//!
//! The client is deliberately the simple half. It does no layout maths and
//! keeps no model of the wall: coordinates arrive already translated into this
//! machine's own pixels, so the hot path is "decode a frame, hand it to the OS".
//!
//! Injection runs on its own thread rather than on the async runtime. That keeps
//! a busy task from inserting a scheduling delay between the wire and the OS,
//! and it means the platform backend lives on one thread for its whole life,
//! which is what the X11 and macOS APIs want anyway.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::clipboard::Clipboard;
use crate::crypto::{secret_from_code, Secret};
use crate::displays;
use crate::inject::{InjectError, Injector};
use crate::protocol::{id_to_hex, Clip, ClientMsg, DeviceInfo, Display, HostMsg, InputEvent};
use crate::settings::{self, Settings};
use crate::transport::{client_handshake, AuthMode, FrameReader, FrameWriter, TransportError};

/// Reconnect backoff. Starts fast because the usual cause is the host app
/// restarting, and settles low enough to be free while a laptop is asleep.
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// A host that is switched off shows up as a TCP connect that never completes,
/// which on some stacks means a 20-second wait. This bounds it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// The host pings every two seconds, so silence for this long means it is gone
/// whatever TCP still believes. Without it a dead host on a dropped WiFi link
/// takes minutes to notice.
const IDLE_TIMEOUT: Duration = Duration::from_secs(8);

/// How often the client re-checks its own monitors.
const DISPLAY_POLL: Duration = Duration::from_secs(1);

/// Where and how to connect.
#[derive(Clone, Debug)]
pub struct Target {
    pub address: String,
    pub port: u16,
    /// Hex host id from discovery, when known. Having it lets an already paired
    /// device reconnect without the code being typed again.
    pub host_id: Option<String>,
    /// The code the user typed, kept for the life of the session so a reconnect
    /// after the host restarts does not prompt again.
    pub code: Option<String>,
}

/// What the UI shows about the connection.
#[derive(Clone, Debug, Serialize, Default)]
pub struct Status {
    pub connected: bool,
    pub host_id: String,
    pub host_name: String,
    pub host_os: String,
    pub address: String,
    /// Set while the host has the pointer on this device.
    pub controlled: bool,
    /// Latest error, or empty while things are fine.
    pub message: String,
    pub attempts: u32,
}

pub struct Shared {
    pub status: Mutex<Status>,
    /// Bumped whenever the UI should refresh.
    pub dirty: Arc<AtomicBool>,
    pub settings: Mutex<Settings>,
    clipboard: Option<Clipboard>,
    input: UnboundedSender<InputEvent>,
    /// Sender for the connection that is up right now, if any.
    out: Mutex<Option<UnboundedSender<ClientMsg>>>,
    stopping: AtomicBool,
    stop: tokio::sync::Notify,
}

impl Shared {
    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Queues a message for the host. Silently dropped while disconnected —
    /// there is nothing useful to do with a clipboard change nobody can hear.
    fn send(&self, msg: ClientMsg) {
        if let Some(tx) = self.out.lock().as_ref() {
            let _ = tx.send(msg);
        }
    }

    fn set_error(&self, message: String) {
        let mut status = self.status.lock();
        status.connected = false;
        status.controlled = false;
        status.message = message;
        status.attempts = status.attempts.saturating_add(1);
        drop(status);
        self.mark_dirty();
    }
}

/// Holds the client's tasks. Dropping it disconnects.
pub struct Client {
    pub state: Arc<Shared>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Client {
    /// Starts connecting. A platform with no input backend at all is fatal —
    /// replaying input is the entire job of this role — but a backend the OS is
    /// merely *refusing* is not: see [`spawn_injector`].
    pub fn start(
        settings: Settings,
        target: Target,
        dirty: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let input = spawn_injector().map_err(|e| e.to_string())?;

        let clips = settings
            .clipboard_sync
            .then(|| Clipboard::start(settings.clipboard_images));
        let (clipboard, clip_rx) = match clips {
            Some((board, rx)) => (Some(board), Some(rx)),
            None => (None, None),
        };

        let state = Arc::new(Shared {
            status: Mutex::new(Status {
                address: format!("{}:{}", target.address, target.port),
                ..Status::default()
            }),
            dirty,
            settings: Mutex::new(settings),
            clipboard,
            input,
            out: Mutex::new(None),
            stopping: AtomicBool::new(false),
            stop: tokio::sync::Notify::new(),
        });

        let mut tasks = Vec::new();
        tasks.push(spawn_connect_loop(state.clone(), target));
        tasks.push(spawn_display_watch(state.clone()));
        if let Some(clip_rx) = clip_rx {
            tasks.push(spawn_clipboard(clip_rx, state.clone()));
        }

        Ok(Self { state, tasks })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Let go of anything the host left held before the socket dies, or this
        // machine is stuck with a key down and nothing to release it.
        let _ = self.state.input.send(InputEvent::ReleaseAll);
        self.state.send(ClientMsg::Bye {
            reason: "client stopped".into(),
        });
        self.state.stopping.store(true, Ordering::Relaxed);
        self.state.stop.notify_waiters();
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

/// Runs the injector on a dedicated thread and returns the queue into it.
///
/// The thread is what keeps replay off the async runtime: an input event is
/// pulled straight out of the channel and handed to the OS, with no chance of
/// waiting behind another task.
///
/// An `Injector` whose backend the OS is refusing still counts as ready. That is
/// the macOS case: the user grants Accessibility while this window is open, and
/// failing the connection here would mean they had to reconnect — or restart —
/// to benefit from a grant made seconds ago. The injector reopens its backend on
/// its own, and until it does the UI is the thing saying why nothing moves.
fn spawn_injector() -> Result<UnboundedSender<InputEvent>, InjectError> {
    let (tx, mut rx) = unbounded_channel::<InputEvent>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), InjectError>>();

    std::thread::Builder::new()
        .name("crossdesk-inject".into())
        .spawn(move || {
            let mut injector = match Injector::new() {
                Ok(injector) => {
                    let _ = ready_tx.send(Ok(()));
                    injector
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // `blocking_recv` is the point: this thread is not async and never
            // yields to the runtime.
            while let Some(event) = rx.blocking_recv() {
                injector.apply(&event);
            }
            // Dropping the injector releases whatever is still held.
        })
        .map_err(|e| InjectError::Unavailable(e.to_string()))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(tx),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(InjectError::Unavailable(
            "the input thread stopped before it started".into(),
        )),
    }
}

/// Connect, run, reconnect. The loop is the reason a host restart looks like a
/// brief pause rather than something the user has to fix.
fn spawn_connect_loop(state: Arc<Shared>, target: Target) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = RETRY_MIN;
        loop {
            let mut connected = false;
            match session(&state, &target, &mut connected).await {
                // A clean goodbye: the host is going away on purpose.
                Ok(()) => state.set_error("host disconnected".into()),
                Err(e) => state.set_error(e.to_string()),
            }

            // Whatever the host left held has to go, connection or not.
            let _ = state.input.send(InputEvent::ReleaseAll);
            *state.out.lock() = None;

            if state.stopping.load(Ordering::Relaxed) {
                return;
            }
            // A connection that actually worked resets the backoff, so a host
            // that restarts is picked up in half a second however long the
            // session lasted.
            backoff = if connected {
                RETRY_MIN
            } else {
                (backoff * 2).min(RETRY_MAX)
            };
            tokio::select! {
                () = tokio::time::sleep(backoff) => {}
                () = state.stop.notified() => return,
            }
        }
    })
}

/// The secrets to try, in order. A stored key comes first so a paired device
/// reconnects without a prompt; the typed code is kept as a fallback for a host
/// that has forgotten us (its settings were reset, or the pairing was revoked).
fn credentials(state: &Arc<Shared>, target: &Target) -> Vec<(AuthMode, Secret)> {
    let settings = state.settings.lock();
    let mut out = Vec::with_capacity(2);
    if let Some(host_id) = target.host_id.as_deref() {
        if let Some(key) = settings.paired_hosts.get(host_id) {
            out.push((AuthMode::Stored, *key));
        }
    }
    if let Some(code) = target.code.as_deref() {
        if !code.trim().is_empty() {
            out.push((AuthMode::Code, secret_from_code(code)));
        }
    }
    out
}

/// One connection attempt, from the TCP connect to the goodbye.
async fn session(
    state: &Arc<Shared>,
    target: &Target,
    connected: &mut bool,
) -> Result<(), TransportError> {
    let attempts = credentials(state, target);
    if attempts.is_empty() {
        return Err(TransportError::Rejected);
    }

    let me = {
        let settings = state.settings.lock();
        DeviceInfo {
            id: settings.id(),
            name: settings.device_name.clone(),
            os: settings::os_label(),
            displays: displays::enumerate(),
        }
    };

    let mut last = TransportError::Rejected;
    for (mode, auth) in attempts {
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((target.address.as_str(), target.port)),
        )
        .await
        .map_err(|_| timed_out("could not reach the host"))??;

        match client_handshake(stream, me.id, mode, &auth).await {
            Ok((reader, writer)) => {
                return run(state, target, reader, writer, me, connected).await;
            }
            // Wrong key: fall through to the next credential rather than
            // failing outright, so a host that was re-paired still works.
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn timed_out(what: &str) -> TransportError {
    TransportError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, what))
}

/// A live connection: say hello, then pump frames until one side stops.
async fn run(
    state: &Arc<Shared>,
    target: &Target,
    mut reader: FrameReader,
    mut writer: FrameWriter,
    me: DeviceInfo,
    connected: &mut bool,
) -> Result<(), TransportError> {
    writer.send(&ClientMsg::Hello { device: me }).await?;

    let HostMsg::Welcome {
        host,
        psk,
        clipboard_sync,
    } = reader.recv().await?
    else {
        // Welcome is the only legal first word from a host.
        return Err(TransportError::Handshake);
    };

    let host_hex = id_to_hex(&host.id);
    // A freshly issued key means the code has done its job; store it so this
    // device never has to be paired by hand again.
    if let Some(psk) = psk {
        let mut settings = state.settings.lock();
        settings.paired_hosts.insert(host_hex.clone(), psk);
        settings.save();
    }

    *connected = true;
    {
        let mut status = state.status.lock();
        status.connected = true;
        status.controlled = false;
        status.message = String::new();
        status.attempts = 0;
        status.host_id = host_hex;
        status.host_name = host.name.clone();
        status.host_os = host.os.clone();
        status.address = format!("{}:{}", target.address, target.port);
    }
    state.mark_dirty();

    let (tx, rx) = unbounded_channel();
    *state.out.lock() = Some(tx);
    let write_task = tokio::spawn(writer_task(writer, rx));

    let result = tokio::select! {
        result = reader_task(&mut reader, state, clipboard_sync) => result,
        () = wait_for_stop(state) => Ok(()),
    };

    *state.out.lock() = None;
    let _ = tokio::time::timeout(Duration::from_millis(200), write_task).await;
    {
        let mut status = state.status.lock();
        status.connected = false;
        status.controlled = false;
    }
    state.mark_dirty();
    result
}

/// Resolves when the client is shutting down.
async fn wait_for_stop(state: &Arc<Shared>) {
    // Register interest before the check, so a stop between the two is not lost.
    let notified = state.stop.notified();
    if state.stopping.load(Ordering::Relaxed) {
        return;
    }
    notified.await;
}

/// Owns the socket's write half, so nothing on the read path ever waits on a
/// send.
async fn writer_task(mut writer: FrameWriter, mut rx: UnboundedReceiver<ClientMsg>) {
    while let Some(msg) = rx.recv().await {
        let bye = matches!(msg, ClientMsg::Bye { .. });
        if writer.send(&msg).await.is_err() {
            break;
        }
        if bye {
            break;
        }
    }
    writer.shutdown().await;
}

/// The hot path: decode a frame, hand the payload straight to the input thread.
async fn reader_task(
    reader: &mut FrameReader,
    state: &Arc<Shared>,
    clipboard_sync: bool,
) -> Result<(), TransportError> {
    loop {
        // The host pings on a timer, so a read that goes quiet for longer than
        // the timeout means the link is dead even if TCP has not noticed.
        let msg = tokio::time::timeout(IDLE_TIMEOUT, reader.recv::<HostMsg>())
            .await
            .map_err(|_| timed_out("the host stopped responding"))??;

        match msg {
            HostMsg::Input(event) => {
                if state.input.send(event).is_err() {
                    // The input thread is gone; without it this role is useless.
                    return Err(timed_out("the input backend stopped"));
                }
            }
            HostMsg::Enter { x, y } => {
                // Put the pointer where the host says before anything else
                // arrives, so the first click lands in the right place.
                let _ = state.input.send(InputEvent::MoveAbs { x, y });
                state.status.lock().controlled = true;
                state.mark_dirty();
            }
            HostMsg::Leave => {
                let _ = state.input.send(InputEvent::ReleaseAll);
                state.status.lock().controlled = false;
                state.mark_dirty();
            }
            HostMsg::Clipboard(clip) => {
                if clipboard_sync {
                    if let Some(board) = state.clipboard.as_ref() {
                        board.apply(clip);
                    }
                }
            }
            HostMsg::Ping { stamp } => state.send(ClientMsg::Pong { stamp }),
            HostMsg::Welcome { .. } => return Err(TransportError::Handshake),
            HostMsg::Bye { .. } => return Ok(()),
        }
    }
}

/// Reports this machine's monitors whenever they change, so the host's wall — and
/// the layout editor — stay honest about where the edges are.
fn spawn_display_watch(state: Arc<Shared>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(DISPLAY_POLL);
        let mut last: Vec<Display> = displays::enumerate();
        loop {
            ticker.tick().await;
            let current = displays::enumerate();
            if current == last {
                continue;
            }
            last = current.clone();
            state.send(ClientMsg::DisplaysChanged { displays: current });
        }
    })
}

/// Sends local clipboard changes up to the host, which fans them out.
fn spawn_clipboard(
    mut changes: UnboundedReceiver<Clip>,
    state: Arc<Shared>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(clip) = changes.recv().await {
            state.send(ClientMsg::Clipboard(clip));
        }
    })
}
