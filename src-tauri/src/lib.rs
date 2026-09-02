//! Tauri command surface: the bridge between the UI and the two roles.
//!
//! Only one role runs at a time, and swapping roles tears the old one down —
//! being a host and a client at once is a way to build an input loop, not a
//! feature.
//!
//! The UI is not told about every input event. State changes set a dirty flag
//! and a timer emits at most one `crossdesk://state` event per tick, so a
//! thousand mouse moves a second cannot turn into a thousand React renders.

mod capture;
mod client;
mod clipboard;
mod crypto;
mod discovery;
mod displays;
mod host;
mod inject;
mod layout;
mod permission;
mod protocol;
mod settings;
mod transport;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::client::{Client, Target};
use crate::crypto::random_pairing_code;
use crate::host::Host;
use crate::layout::LayoutView;
use crate::protocol::{Display, DEFAULT_PORT};
use crate::settings::Settings;

/// UI refresh rate. Fast enough that latency numbers and the pointer's owner
/// feel live, slow enough that the frontend is never the bottleneck.
const UI_TICK: Duration = Duration::from_millis(250);

/// How long a discovery scan listens before answering.
const SCAN_WINDOW: Duration = Duration::from_millis(700);

/// Which role is running, if any.
enum Role {
    Idle,
    Host(Host),
    Client(Client),
}

impl Role {
    fn label(&self) -> &'static str {
        match self {
            Role::Idle => "idle",
            Role::Host(_) => "host",
            Role::Client(_) => "client",
        }
    }
}

struct App {
    role: Mutex<Role>,
    /// The single source of truth while idle; each role gets a clone and writes
    /// back through `Settings::save`.
    settings: Mutex<Settings>,
    dirty: Arc<AtomicBool>,
}

impl App {
    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Applies a change to whichever copy of the settings is live and persists
    /// it. Locks are taken role-then-settings here and in `snapshot`, which is
    /// the same order the host uses internally.
    fn edit_settings(&self, f: impl FnOnce(&mut Settings)) -> Settings {
        let mut role = self.role.lock();
        let updated = {
            let mut settings = match &mut *role {
                Role::Host(host) => host.state.settings.lock(),
                Role::Client(client) => client.state.settings.lock(),
                Role::Idle => self.settings.lock(),
            };
            f(&mut settings);
            settings.save();
            settings.clone()
        };
        // Keep the idle copy current too, so stopping a role does not roll a
        // setting back.
        *self.settings.lock() = updated.clone();
        drop(role);
        self.mark_dirty();
        updated
    }
}

/// Everything the UI needs for one render. Sent as one object so the frontend
/// never has to stitch several commands together and risk showing a half-state.
#[derive(Serialize, Clone)]
struct Snapshot {
    role: &'static str,
    device_name: String,
    device_id: String,
    pairing_code: String,
    /// Set while the host will accept a device it has not seen before.
    accepting: bool,
    clipboard_sync: bool,
    clipboard_images: bool,
    port: u16,
    /// False where input capture has no backend yet, which today means anything
    /// that is not Windows. Such a machine is a perfectly good client; it just
    /// cannot be the one holding the keyboard, so the UI says so up front
    /// instead of failing when the button is pressed.
    can_host: bool,
    /// Whether the OS will let this machine replay input at all. macOS gates it
    /// behind Accessibility and refuses in silence, so the UI has to ask rather
    /// than wait for an error that never comes.
    input_access: permission::Access,
    /// This machine's own monitors, for the layout preview while idle.
    displays: Vec<Display>,
    /// Host role only.
    layout: Option<LayoutView>,
    peers: Vec<PeerView>,
    /// Client role only. Carries its own message, because a client spends its
    /// life reconnecting and the reason matters.
    connection: Option<client::Status>,
}

#[derive(Serialize, Clone)]
struct PeerView {
    id: String,
    name: String,
    os: String,
    latency_ms: Option<u32>,
    displays: Vec<Display>,
}

fn snapshot(app: &App) -> Snapshot {
    // One role lock for the whole snapshot: reading settings through the running
    // role is what keeps the UI honest about, say, a pairing code that was just
    // regenerated, and doing it under the same lock means the two halves cannot
    // disagree about which role is running.
    let role = app.role.lock();
    let settings = match &*role {
        Role::Host(host) => host.state.settings.lock().clone(),
        Role::Client(client) => client.state.settings.lock().clone(),
        Role::Idle => app.settings.lock().clone(),
    };

    let (layout, peers, connection, accepting) = match &*role {
        Role::Host(host) => {
            let mut peers: Vec<PeerView> = host
                .state
                .peers
                .lock()
                .iter()
                .map(|(id, peer)| PeerView {
                    id: id.clone(),
                    name: peer.info.name.clone(),
                    os: peer.info.os.clone(),
                    latency_ms: peer.latency_ms,
                    displays: peer.info.displays.clone(),
                })
                .collect();
            // Stable order, so a peer does not jump around the list between
            // ticks just because the map iterated differently.
            peers.sort_by(|a, b| a.name.cmp(&b.name));
            (
                Some(host.state.layout_view()),
                peers,
                None,
                host.state.accepting.load(Ordering::Relaxed),
            )
        }
        Role::Client(client) => (
            None,
            Vec::new(),
            Some(client.state.status.lock().clone()),
            false,
        ),
        Role::Idle => (None, Vec::new(), None, false),
    };

    Snapshot {
        role: role.label(),
        device_name: settings.device_name,
        device_id: settings.device_id,
        pairing_code: settings.pairing_code,
        accepting,
        clipboard_sync: settings.clipboard_sync,
        clipboard_images: settings.clipboard_images,
        port: DEFAULT_PORT,
        can_host: capture::supported(),
        input_access: permission::status(),
        displays: displays::enumerate(),
        layout,
        peers,
        connection,
    }
}

#[tauri::command]
fn get_state(app: tauri::State<'_, Arc<App>>) -> Snapshot {
    snapshot(&app)
}

#[tauri::command]
async fn start_host(app: tauri::State<'_, Arc<App>>) -> Result<Snapshot, String> {
    let app = app.inner().clone();
    // Stop whatever is running first: two roles at once is how you build an
    // input loop, and the port would be taken anyway.
    stop_role(&app);

    let settings = app.settings.lock().clone();
    let host = Host::start(settings, app.dirty.clone()).await?;
    *app.role.lock() = Role::Host(host);
    app.mark_dirty();
    Ok(snapshot(&app))
}

#[tauri::command]
async fn start_client(
    app: tauri::State<'_, Arc<App>>,
    address: String,
    port: Option<u16>,
    host_id: Option<String>,
    code: Option<String>,
) -> Result<Snapshot, String> {
    let app = app.inner().clone();
    let address = address.trim().to_string();
    if address.is_empty() {
        return Err("enter the host's address, or pick it from the list".into());
    }
    stop_role(&app);

    let settings = app.settings.lock().clone();
    let target = Target {
        address,
        port: port.unwrap_or(DEFAULT_PORT),
        host_id,
        code,
    };
    let client = Client::start(settings, target, app.dirty.clone())?;
    *app.role.lock() = Role::Client(client);
    app.mark_dirty();
    Ok(snapshot(&app))
}

#[tauri::command]
fn stop(app: tauri::State<'_, Arc<App>>) -> Snapshot {
    stop_role(&app);
    snapshot(&app)
}

/// Drops the running role. `Drop` does the real work — un-suppressing input,
/// saying goodbye, releasing held keys — so this is just about ordering: the
/// lock is released before the old role is destroyed, because tearing a host
/// down touches the same state a command might be reading.
fn stop_role(app: &Arc<App>) {
    let previous = std::mem::replace(&mut *app.role.lock(), Role::Idle);
    drop(previous);
    app.mark_dirty();
}

/// Scans the LAN for hosts. Takes a moment by design: it waits for replies.
#[tauri::command]
async fn scan_hosts() -> Vec<discovery::Found> {
    discovery::scan(SCAN_WINDOW).await
}

/// Moves a device on the wall. Called at pointer rate during a drag, so it does
/// the least it possibly can: place it, flag the UI, return nothing. Persisting
/// here would mean a disk write per frame, and building a snapshot would mean
/// re-enumerating this machine's monitors per frame.
#[tauri::command]
fn move_device(
    app: tauri::State<'_, Arc<App>>,
    device_id: String,
    x: i32,
    y: i32,
) -> Result<(), String> {
    let role = app.role.lock();
    let Role::Host(host) = &*role else {
        return Err("the layout only applies while hosting".into());
    };
    let mut wall = host.state.wall.lock();
    let Some(idx) = wall
        .devices
        .iter()
        .position(|d| protocol::id_to_hex(&d.id) == device_id)
    else {
        return Err("no such device".into());
    };
    // The host is the origin of the wall; moving it would move everything else
    // relative to it, which is not what the drag means.
    if idx == 0 {
        return Err("the host stays where it is; drag the other devices".into());
    }
    wall.set_offset(idx, x, y);
    drop(wall);
    drop(role);
    app.mark_dirty();
    Ok(())
}

/// Writes the wall to disk. Called once when a drag ends, rather than on every
/// frame of it.
#[tauri::command]
fn commit_layout(app: tauri::State<'_, Arc<App>>) -> Snapshot {
    let app = app.inner().clone();
    persist_layout(&app);
    snapshot(&app)
}

/// Nudges every device onto its nearest neighbour's edge. The threshold is in
/// wall pixels — generous, because the UI works in a scaled-down preview.
#[tauri::command]
fn snap_layout(app: tauri::State<'_, Arc<App>>) -> Result<Snapshot, String> {
    let app = app.inner().clone();
    {
        let role = app.role.lock();
        let Role::Host(host) = &*role else {
            return Err("the layout only applies while hosting".into());
        };
        host.state.wall.lock().snap_edges(120);
    }
    persist_layout(&app);
    app.mark_dirty();
    Ok(snapshot(&app))
}

/// Back to the default left-to-right row.
#[tauri::command]
fn auto_arrange(app: tauri::State<'_, Arc<App>>) -> Result<Snapshot, String> {
    let app = app.inner().clone();
    {
        let role = app.role.lock();
        let Role::Host(host) = &*role else {
            return Err("the layout only applies while hosting".into());
        };
        host.state.wall.lock().auto_arrange();
    }
    persist_layout(&app);
    app.mark_dirty();
    Ok(snapshot(&app))
}

/// Writes every device's offset into settings, so the wall survives a restart.
fn persist_layout(app: &Arc<App>) {
    let offsets: Vec<(String, (i32, i32))> = {
        let role = app.role.lock();
        let Role::Host(host) = &*role else {
            return;
        };
        let wall = host.state.wall.lock();
        wall.devices
            .iter()
            .skip(1)
            .map(|d| (protocol::id_to_hex(&d.id), d.offset))
            .collect()
    };
    app.edit_settings(|settings| {
        for (id, offset) in offsets {
            settings.layout.insert(id, offset);
        }
    });
}

/// Pulls the pointer back to this machine. The same thing Ctrl+Alt+Home does,
/// for when the pointer is somewhere the hotkey cannot be typed.
#[tauri::command]
fn recall_pointer(app: tauri::State<'_, Arc<App>>) -> Snapshot {
    let app = app.inner().clone();
    {
        let role = app.role.lock();
        if let Role::Host(host) = &*role {
            host.state.take_pointer_back();
        }
    }
    snapshot(&app)
}

/// Forgets a device: drops its key, its saved position, and its connection.
#[tauri::command]
fn forget_device(app: tauri::State<'_, Arc<App>>, device_id: String) -> Snapshot {
    let app = app.inner().clone();
    {
        let role = app.role.lock();
        if let Role::Host(host) = &*role {
            // Take the pointer back first if this is where it is, then tell the
            // peer why it is being cut off.
            let owns = {
                let wall = host.state.wall.lock();
                !wall.is_local() && protocol::id_to_hex(&wall.devices[wall.owner()].id) == device_id
            };
            if owns {
                host.state.take_pointer_back();
            }
            host.state.disconnect_peer(&device_id, "unpaired by the host");

            let mut wall = host.state.wall.lock();
            if let Some(idx) = wall
                .devices
                .iter()
                .position(|d| protocol::id_to_hex(&d.id) == device_id)
            {
                wall.remove(idx);
            }
        }
    }
    app.edit_settings(|settings| {
        settings.paired_clients.remove(&device_id);
        settings.paired_hosts.remove(&device_id);
        settings.layout.remove(&device_id);
    });
    snapshot(&app)
}

/// Renames this device. Peers see the new name the next time they connect;
/// renaming mid-session is not worth a protocol message.
#[tauri::command]
fn set_device_name(app: tauri::State<'_, Arc<App>>, name: String) -> Snapshot {
    let app = app.inner().clone();
    let name = name.trim().to_string();
    if !name.is_empty() {
        app.edit_settings(|settings| settings.device_name = name.clone());
    }
    snapshot(&app)
}

/// Rolls a new pairing code. Devices that have already paired hold a key of
/// their own, so this only affects who can pair from now on.
#[tauri::command]
fn regenerate_code(app: tauri::State<'_, Arc<App>>) -> Snapshot {
    let app = app.inner().clone();
    app.edit_settings(|settings| settings.pairing_code = random_pairing_code());
    snapshot(&app)
}

/// Opens or closes the pairing window. Closed means an unknown device is turned
/// away even with the right code, which is the point: the code is only useful
/// while the user is actually adding a device.
#[tauri::command]
fn set_accepting(app: tauri::State<'_, Arc<App>>, accepting: bool) -> Snapshot {
    let app = app.inner().clone();
    {
        let role = app.role.lock();
        if let Role::Host(host) = &*role {
            host.state.accepting.store(accepting, Ordering::Relaxed);
        }
    }
    app.mark_dirty();
    snapshot(&app)
}

/// Clipboard toggles. Both take effect on the next start of a role: the polling
/// thread is created with the role, and restarting it mid-session would mean
/// losing track of what has already been seen.
#[tauri::command]
fn set_clipboard(
    app: tauri::State<'_, Arc<App>>,
    sync: Option<bool>,
    images: Option<bool>,
) -> Snapshot {
    let app = app.inner().clone();
    app.edit_settings(|settings| {
        if let Some(sync) = sync {
            settings.clipboard_sync = sync;
        }
        if let Some(images) = images {
            settings.clipboard_images = images;
        }
    });
    snapshot(&app)
}

/// Asks the OS for permission to replay input, prompting if it has not been
/// decided, and opens the pane where it is granted.
///
/// Both halves matter on macOS. The request is what registers *this* binary in
/// the Accessibility list — ticking a hand-added entry grants permission to
/// whichever copy that path points at, which is why a granted-looking CrossDesk
/// can still be unable to move the pointer. Opening the pane is then just so the
/// user can see the entry that appeared.
#[tauri::command]
async fn request_input_access(app: tauri::State<'_, Arc<App>>) -> Result<Snapshot, String> {
    let app = app.inner().clone();
    // Opening System Settings shells out, so keep it off the UI thread.
    let opened = tauri::async_runtime::spawn_blocking(permission::open_settings)
        .await
        .map_err(|e| e.to_string())?;
    app.mark_dirty();
    match opened {
        Ok(()) => Ok(snapshot(&app)),
        Err(e) => Err(e),
    }
}

/// Moves this machine's own pointer and reports whether it actually moved.
///
/// The only honest answer available on macOS: `CGEventPost` reports nothing, and
/// the Accessibility list can claim a grant that no longer matches this build. A
/// pointer that visibly moves settles it either way.
#[tauri::command]
async fn test_input(app: tauri::State<'_, Arc<App>>) -> Result<permission::Probe, String> {
    let app = app.inner().clone();
    let probe = tauri::async_runtime::spawn_blocking(permission::probe)
        .await
        .map_err(|e| e.to_string())?;
    app.mark_dirty();
    Ok(probe)
}

/// The UI's clock. One event per tick, and only when something actually changed,
/// so the frontend re-renders at a human rate no matter how fast input arrives.
fn spawn_ui_tick(handle: tauri::AppHandle, app: Arc<App>) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(UI_TICK);
        // Nothing marks the state dirty when the *user* grants Accessibility in
        // System Settings — it happens entirely outside this process. So the one
        // piece of state that can change without us is polled here, which is what
        // lets the warning clear by itself rather than needing a button. Two C
        // calls reading process state, four times a second, and not even that off
        // macOS: `NotNeeded` can never change.
        let mut access = permission::status();
        loop {
            ticker.tick().await;
            if access != permission::Access::NotNeeded {
                let now = permission::status();
                if now != access {
                    access = now;
                    app.mark_dirty();
                }
            }
            // `swap` rather than load-then-clear: a state change that lands
            // between the two would otherwise be dropped.
            if !app.dirty.swap(false, Ordering::Relaxed) {
                continue;
            }
            let _ = handle.emit("crossdesk://state", snapshot(&app));
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let settings = Settings::load();
    let app = Arc::new(App {
        role: Mutex::new(Role::Idle),
        settings: Mutex::new(settings),
        dirty: Arc::new(AtomicBool::new(true)),
    });

    tauri::Builder::default()
        .manage(app.clone())
        .setup(move |handle| {
            spawn_ui_tick(handle.handle().clone(), app.clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window ends the process, and a role that dies without
            // running `Drop` leaves real damage behind: input suppressed on the
            // host, or keys held down on a client. Tear it down while there is
            // still a runtime to do it on.
            if matches!(event, tauri::WindowEvent::Destroyed) {
                if let Some(app) = window.try_state::<Arc<App>>() {
                    stop_role(&app);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            start_host,
            start_client,
            stop,
            scan_hosts,
            move_device,
            commit_layout,
            snap_layout,
            auto_arrange,
            recall_pointer,
            forget_device,
            set_device_name,
            regenerate_code,
            set_accepting,
            set_clipboard,
            request_input_access,
            test_input,
        ])
        .run(tauri::generate_context!())
        .expect("error while running CrossDesk");
}
