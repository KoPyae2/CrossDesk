//! Injection for everything that is not Windows, via `enigo`.
//!
//! The host is Windows-only for now (see `capture`), so this side is what makes
//! a Mac or a Linux box usable as a client. The host still speaks in Windows
//! virtual-key codes and scancodes because that is what its hooks report, so
//! the translation into `enigo`'s portable key names happens here.

use enigo::{
    Axis, Button as EButton, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, NewConError,
    Settings,
};

use super::InjectError;
use crate::protocol::Button;

/// One notch of a wheel, in the 1/120 units the protocol carries.
const WHEEL_UNIT: i32 = 120;

/// How every `Enigo` in this process is built.
///
/// The one setting that matters is `open_prompt_to_get_permissions: false`.
/// enigo's default is to raise the macOS Accessibility dialog from
/// `Enigo::new`, and this process constructs an `Enigo` for every `warp`, every
/// cursor read and every retry of a refused backend — so the default turns one
/// permission problem into a stream of system dialogs. Asking is a deliberate,
/// user-initiated step instead (`permission::open_settings`), and the UI says
/// what is wrong in the meantime.
fn settings() -> Settings {
    Settings {
        open_prompt_to_get_permissions: false,
        ..Settings::default()
    }
}

pub struct Backend {
    enigo: Enigo,
    /// Sub-notch wheel movement, kept so a hi-res wheel still scrolls smoothly
    /// through an API that only understands whole clicks.
    wheel_x: i32,
    wheel_y: i32,
}

impl Backend {
    pub fn new() -> Result<Self, InjectError> {
        let enigo = Enigo::new(&settings()).map_err(|e| match e {
            // `Blocked`, not `Unavailable`: this is the failure a user hits after
            // ticking the box in System Settings, and on macOS it is lifted
            // *while the app runs*. Treating it as fatal would mean the client
            // role refuses to start on the very machine the user is fixing, so
            // they would have to restart CrossDesk to benefit from the grant they
            // just made. Elsewhere it stays fatal and loud, because nothing in
            // the UI would explain a client that silently moves nothing.
            NewConError::NoPermission if cfg!(target_os = "macos") => InjectError::Blocked(
                "macOS has not granted Accessibility permission to this copy of CrossDesk, so it \
                 cannot replay the host's input yet. Use Grant permission on this screen — adding \
                 the app by hand often grants it to a different build."
                    .into(),
            ),
            other => InjectError::Unavailable(other.to_string()),
        })?;
        Ok(Self {
            enigo,
            wheel_x: 0,
            wheel_y: 0,
        })
    }

    pub fn move_abs(&mut self, x: i32, y: i32) {
        let _ = self.enigo.move_mouse(x, y, Coordinate::Abs);
    }

    pub fn button(&mut self, button: Button, down: bool) {
        let button = match button {
            Button::Left => EButton::Left,
            Button::Right => EButton::Right,
            Button::Middle => EButton::Middle,
            Button::Back => EButton::Back,
            Button::Forward => EButton::Forward,
        };
        let dir = if down {
            Direction::Press
        } else {
            Direction::Release
        };
        let _ = self.enigo.button(button, dir);
    }

    pub fn wheel(&mut self, dx: i16, dy: i16) {
        self.wheel_y += dy as i32;
        self.wheel_x += dx as i32;

        let notches_y = self.wheel_y / WHEEL_UNIT;
        if notches_y != 0 {
            self.wheel_y -= notches_y * WHEEL_UNIT;
            // Windows reports wheel-up as positive; enigo scrolls down on
            // positive, so the sign flips here.
            let _ = self.enigo.scroll(-notches_y, Axis::Vertical);
        }

        let notches_x = self.wheel_x / WHEEL_UNIT;
        if notches_x != 0 {
            self.wheel_x -= notches_x * WHEEL_UNIT;
            let _ = self.enigo.scroll(notches_x, Axis::Horizontal);
        }
    }

    pub fn key(&mut self, vk: u16, _scan: u16, _ext: bool, down: bool) {
        let Some(key) = translate(vk) else {
            return;
        };
        let dir = if down {
            Direction::Press
        } else {
            Direction::Release
        };
        let _ = self.enigo.key(key, dir);
    }

    pub fn text(&mut self, text: &str) {
        let _ = self.enigo.text(text);
    }
}

/// Windows virtual-key code to a portable key.
///
/// Anything printable goes through `Key::Unicode`, which lets the client's own
/// layout decide what the key produces — the same behaviour as sitting at it.
fn translate(vk: u16) -> Option<Key> {
    Some(match vk {
        0x08 => Key::Backspace,
        0x09 => Key::Tab,
        0x0D => Key::Return,
        0x10 => Key::Shift,
        0x11 => Key::Control,
        0x12 => Key::Alt,
        0x14 => Key::CapsLock,
        0x1B => Key::Escape,
        0x20 => Key::Space,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x23 => Key::End,
        0x24 => Key::Home,
        0x25 => Key::LeftArrow,
        0x26 => Key::UpArrow,
        0x27 => Key::RightArrow,
        0x28 => Key::DownArrow,
        #[cfg(all(unix, not(target_os = "macos")))]
        0x2D => Key::Insert,
        0x2E => Key::Delete,
        0x2F => Key::Help,
        0x30..=0x39 => Key::Unicode((b'0' + (vk - 0x30) as u8) as char),
        0x41..=0x5A => Key::Unicode((b'a' + (vk - 0x41) as u8) as char),
        0x5B | 0x5C => Key::Meta,
        0x60..=0x69 => numpad(vk - 0x60)?,
        0x6A => Key::Multiply,
        0x6B => Key::Add,
        0x6C => Key::Return,
        0x6D => Key::Subtract,
        0x6E => Key::Decimal,
        0x6F => Key::Divide,
        0x70..=0x83 => function_key(vk - 0x70 + 1)?,
        0xA0 => Key::LShift,
        0xA1 => Key::RShift,
        0xA2 => Key::LControl,
        0xA3 => Key::RControl,
        // Right Alt is AltGr on many layouts; enigo has no portable name for
        // it, and plain Alt is the closest honest answer.
        0xA4 | 0xA5 => Key::Alt,
        0xAD => Key::VolumeMute,
        0xAE => Key::VolumeDown,
        0xAF => Key::VolumeUp,
        0xB0 => Key::MediaNextTrack,
        0xB1 => Key::MediaPrevTrack,
        0xB3 => Key::MediaPlayPause,
        0xBA => Key::Unicode(';'),
        0xBB => Key::Unicode('='),
        0xBC => Key::Unicode(','),
        0xBD => Key::Unicode('-'),
        0xBE => Key::Unicode('.'),
        0xBF => Key::Unicode('/'),
        0xC0 => Key::Unicode('`'),
        0xDB => Key::Unicode('['),
        0xDC => Key::Unicode('\\'),
        0xDD => Key::Unicode(']'),
        0xDE => Key::Unicode('\''),
        _ => return None,
    })
}

fn numpad(n: u16) -> Option<Key> {
    Some(match n {
        0 => Key::Numpad0,
        1 => Key::Numpad1,
        2 => Key::Numpad2,
        3 => Key::Numpad3,
        4 => Key::Numpad4,
        5 => Key::Numpad5,
        6 => Key::Numpad6,
        7 => Key::Numpad7,
        8 => Key::Numpad8,
        9 => Key::Numpad9,
        _ => return None,
    })
}

fn function_key(n: u16) -> Option<Key> {
    Some(match n {
        1 => Key::F1,
        2 => Key::F2,
        3 => Key::F3,
        4 => Key::F4,
        5 => Key::F5,
        6 => Key::F6,
        7 => Key::F7,
        8 => Key::F8,
        9 => Key::F9,
        10 => Key::F10,
        11 => Key::F11,
        12 => Key::F12,
        13 => Key::F13,
        14 => Key::F14,
        15 => Key::F15,
        16 => Key::F16,
        17 => Key::F17,
        18 => Key::F18,
        19 => Key::F19,
        20 => Key::F20,
        _ => return None,
    })
}

/// Only reachable if this platform ever gains a host role, plus the permission
/// probe. A fresh connection per call is fine at the rate either happens.
pub fn warp(x: i32, y: i32) {
    if let Ok(mut enigo) = Enigo::new(&settings()) {
        let _ = enigo.move_mouse(x, y, Coordinate::Abs);
    }
}

pub fn cursor_position() -> Option<(i32, i32)> {
    Enigo::new(&settings()).ok()?.location().ok()
}
