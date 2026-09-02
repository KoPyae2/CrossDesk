//! Client-side input injection.
//!
//! The host has already done every coordinate translation, so this module is
//! deliberately dumb: take an `InputEvent`, hand it to the OS, return. It also
//! remembers what it is holding down, because a connection that drops mid-drag
//! must not leave a key or button stuck.

use std::time::{Duration, Instant};

use crate::protocol::{Button, InputEvent};

/// How long to wait before trying again to open a backend that was blocked.
///
/// Only reached while injection is impossible anyway, so it costs nothing in the
/// normal case, and it is short enough that granting permission looks immediate.
const REOPEN_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("could not open an input device on this platform: {0}")]
    Unavailable(String),
    /// The platform has a backend but is refusing to let this process use it,
    /// and that refusal can be lifted while the app runs. Separate from
    /// `Unavailable` because it must not be fatal: the user grants macOS
    /// Accessibility *while looking at this window*, and a client that gave up
    /// at startup would have to be restarted to notice.
    ///
    /// Only the macOS path produces it — Windows has nothing to be refused by —
    /// but the enum stays whole so `Injector` handles both cases on every
    /// platform rather than growing a `cfg`.
    #[cfg_attr(windows, allow(dead_code))]
    #[error("{0}")]
    Blocked(String),
}

#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as sys;

#[cfg(not(windows))]
mod other;
#[cfg(not(windows))]
use other as sys;

/// Replays remote input on this machine.
pub struct Injector {
    /// `None` while the OS is refusing us. Retried rather than fatal.
    sys: Option<sys::Backend>,
    /// When to next attempt to open the backend, if it is not open.
    retry_at: Instant,
    keys: Vec<KeyId>,
    buttons: Vec<Button>,
}

/// A key as the host described it. Kept whole so the release matches the press
/// exactly, whatever the backend ends up doing with it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct KeyId {
    vk: u16,
    scan: u16,
    ext: bool,
}

impl Injector {
    /// Opens a backend, or reports why not.
    ///
    /// A `Blocked` platform still yields a working `Injector` — one that drops
    /// what it is given until permission arrives — because refusing the whole
    /// client role over a permission the user is about to grant is worse than
    /// dropping the first few packets. Nothing is returned about the refusal
    /// because `permission` reports it better: it can say what the OS claims
    /// *and* whether the pointer actually moves, and it keeps saying it as the
    /// answer changes rather than only at startup.
    pub fn new() -> Result<Self, InjectError> {
        let sys = match sys::Backend::new() {
            Ok(backend) => Some(backend),
            Err(InjectError::Blocked(_)) => None,
            Err(e) => return Err(e),
        };
        Ok(Self {
            sys,
            retry_at: Instant::now() + REOPEN_AFTER,
            keys: Vec::with_capacity(8),
            buttons: Vec::with_capacity(4),
        })
    }

    /// The backend, reopening it if it is time to try again.
    ///
    /// The clock is only read when there is no backend, so the normal path is a
    /// null check and nothing else.
    fn backend(&mut self) -> Option<&mut sys::Backend> {
        if self.sys.is_none() {
            let now = Instant::now();
            if now >= self.retry_at {
                self.retry_at = now + REOPEN_AFTER;
                self.sys = sys::Backend::new().ok();
            }
        }
        self.sys.as_mut()
    }

    pub fn apply(&mut self, event: &InputEvent) {
        // Book-keeping first, and unconditionally, so the ledger mirrors what the
        // *host* believes it is holding rather than what this machine managed to
        // post. The two differ only while the OS is refusing us, and the
        // asymmetry decides it: releasing a key that was never pressed is a
        // spurious key-up and harmless, while failing to release one that was
        // leaves it stuck with nothing left to free it.
        match *event {
            InputEvent::Button { button, down } => {
                if down {
                    if !self.buttons.contains(&button) {
                        self.buttons.push(button);
                    }
                } else {
                    self.buttons.retain(|b| *b != button);
                }
            }
            InputEvent::Key {
                vk,
                scan,
                ext,
                down,
            } => {
                let id = KeyId { vk, scan, ext };
                if down {
                    if !self.keys.contains(&id) {
                        self.keys.push(id);
                    }
                } else {
                    self.keys.retain(|k| *k != id);
                }
            }
            InputEvent::ReleaseAll => {
                self.release_all();
                return;
            }
            _ => {}
        }

        let Some(sys) = self.backend() else {
            return;
        };
        match *event {
            InputEvent::MoveAbs { x, y } => sys.move_abs(x, y),
            InputEvent::Button { button, down } => sys.button(button, down),
            InputEvent::Wheel { dx, dy } => sys.wheel(dx, dy),
            InputEvent::Key {
                vk,
                scan,
                ext,
                down,
            } => sys.key(vk, scan, ext, down),
            InputEvent::Text(ref text) => sys.text(text),
            // Handled above, before the backend is consulted.
            InputEvent::ReleaseAll => {}
        }
    }

    /// Lets go of everything we are holding. Called on `ReleaseAll`, when the
    /// pointer leaves this device, and when the connection dies.
    pub fn release_all(&mut self) {
        let keys = std::mem::take(&mut self.keys);
        let buttons = std::mem::take(&mut self.buttons);
        // Not worth opening a backend for: nothing can be held down through one
        // that never posted a press.
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        for key in keys {
            sys.key(key.vk, key.scan, key.ext, false);
        }
        for button in buttons {
            sys.button(button, false);
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// Moves this machine's own cursor. Used on the host when control comes back,
/// so the visible pointer picks up where the wall says it should be.
pub fn warp(x: i32, y: i32) {
    sys::warp(x, y);
}

/// Current cursor position in this machine's virtual-desktop pixels.
pub fn cursor_position() -> Option<(i32, i32)> {
    sys::cursor_position()
}
