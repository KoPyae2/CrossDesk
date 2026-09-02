//! Client-side input injection.
//!
//! The host has already done every coordinate translation, so this module is
//! deliberately dumb: take an `InputEvent`, hand it to the OS, return. It also
//! remembers what it is holding down, because a connection that drops mid-drag
//! must not leave a key or button stuck.

use crate::protocol::{Button, InputEvent};

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("could not open an input device on this platform: {0}")]
    Unavailable(String),
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
    sys: sys::Backend,
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
    pub fn new() -> Result<Self, InjectError> {
        Ok(Self {
            sys: sys::Backend::new()?,
            keys: Vec::with_capacity(8),
            buttons: Vec::with_capacity(4),
        })
    }

    pub fn apply(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::MoveAbs { x, y } => self.sys.move_abs(x, y),
            InputEvent::Button { button, down } => {
                if down {
                    if !self.buttons.contains(&button) {
                        self.buttons.push(button);
                    }
                } else {
                    self.buttons.retain(|b| *b != button);
                }
                self.sys.button(button, down);
            }
            InputEvent::Wheel { dx, dy } => self.sys.wheel(dx, dy),
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
                self.sys.key(vk, scan, ext, down);
            }
            InputEvent::Text(ref text) => self.sys.text(text),
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    /// Lets go of everything we are holding. Called on `ReleaseAll`, when the
    /// pointer leaves this device, and when the connection dies.
    pub fn release_all(&mut self) {
        for key in std::mem::take(&mut self.keys) {
            self.sys.key(key.vk, key.scan, key.ext, false);
        }
        for button in std::mem::take(&mut self.buttons) {
            self.sys.button(button, false);
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
