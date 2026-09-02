//! Host-side input capture.
//!
//! Two jobs, and on Windows they need two different mechanisms:
//!
//! * **Suppression** — while a remote device owns the pointer, nothing may
//!   reach local windows. Low-level hooks (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`)
//!   can swallow an event by returning non-zero, which is the only way to do
//!   this without a driver.
//! * **Motion** — hook coordinates are clamped to the desktop, so once the
//!   cursor is parked against an edge they stop changing and can never tell us
//!   "the user kept pushing right". Raw Input (`WM_INPUT`) reports true device
//!   deltas regardless of where the cursor is, so that is what drives the wall.
//!
//! Everything runs on one dedicated thread with its own message loop. Hook
//! callbacks do no allocation and no locking beyond a channel send, because
//! they execute inline on every input event in the whole system.

use tokio::sync::mpsc::UnboundedReceiver;

use crate::protocol::Button;

/// A raw event straight off the input hardware, before any layout maths.
#[derive(Debug, Clone, Copy)]
pub enum CaptureEvent {
    /// True device motion, in device units.
    Motion { dx: i32, dy: i32 },
    /// Where the real cursor ended up. Only acted on while control is local;
    /// it keeps the wall in step with anything else that moves the pointer.
    MotionAbs { x: i32, y: i32 },
    Button { button: Button, down: bool },
    /// Wheel deltas in 1/120 of a notch, so hi-res wheels keep precision.
    Wheel { dx: i16, dy: i16 },
    Key {
        vk: u16,
        scan: u16,
        ext: bool,
        down: bool,
    },
    /// The escape hotkey was pressed: pull the pointer back to this machine.
    Escape,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("input capture is not implemented on this platform yet, so this device can only join as a client")]
    Unsupported,
    #[error("could not install the system input hooks (error {0}); another program may already own them")]
    HookFailed(u32),
}

/// Handle to the capture thread. Dropping it tears the hooks down.
pub struct Capture {
    pub events: UnboundedReceiver<CaptureEvent>,
    #[cfg(windows)]
    thread_id: u32,
}

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::INJECTED_TAG;

/// Whether this machine can take the host role.
///
/// Capture is the only half of CrossDesk that needs per-platform code: a client
/// just injects what it is told, which `enigo` does everywhere. So this is the
/// one thing the UI has to ask about before offering to host.
pub const fn supported() -> bool {
    cfg!(windows)
}

/// Begins capturing. Only one capture may be live per process.
pub fn start() -> Result<Capture, CaptureError> {
    #[cfg(windows)]
    {
        win::start()
    }
    #[cfg(not(windows))]
    {
        Err(CaptureError::Unsupported)
    }
}

/// Turns local delivery of input on and off. While suppressed, the host's own
/// windows see nothing: every event belongs to whichever remote device owns the
/// pointer.
pub fn set_suppressed(on: bool) {
    #[cfg(windows)]
    win::set_suppressed(on);
    #[cfg(not(windows))]
    let _ = on;
}

impl Drop for Capture {
    fn drop(&mut self) {
        set_suppressed(false);
        #[cfg(windows)]
        win::stop(self.thread_id);
    }
}
