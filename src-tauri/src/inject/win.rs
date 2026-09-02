//! `SendInput` injection.
//!
//! Keys go in as **scancodes** rather than virtual-key codes: the host and the
//! client may well have different keyboard layouts, and a scancode means "the
//! physical key in that position", which is what the user actually pressed.
//! Every event is tagged in `dwExtraInfo` so a machine acting as both host and
//! client does not capture its own injected input.

use std::mem::size_of;

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEINPUT,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SetCursorPos, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

use super::InjectError;
use crate::protocol::Button;

/// Same marker the capture side filters on.
use crate::capture::INJECTED_TAG as TAG;

pub struct Backend;

impl Backend {
    pub fn new() -> Result<Self, InjectError> {
        Ok(Self)
    }

    pub fn move_abs(&mut self, x: i32, y: i32) {
        // MOUSEEVENTF_ABSOLUTE takes 0..=65535 across the whole virtual
        // desktop, which is why VIRTUALDESK has to be set as well: without it
        // the range covers only the primary monitor.
        let (ox, oy, w, h) = virtual_screen();
        let nx = normalise(x - ox, w);
        let ny = normalise(y - oy, h);
        send(&[mouse(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )]);
    }

    pub fn button(&mut self, button: Button, down: bool) {
        let (flags, data) = match (button, down) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
            (Button::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
            (Button::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
            (Button::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
        };
        send(&[mouse(0, 0, data, flags)]);
    }

    pub fn wheel(&mut self, dx: i16, dy: i16) {
        // The wire carries 1/120 units; Windows expects the same scale, so this
        // is a straight pass-through and hi-res wheels stay smooth.
        let mut events = Vec::with_capacity(2);
        if dy != 0 {
            events.push(mouse(0, 0, dy as i32 as u32, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            events.push(mouse(0, 0, dx as i32 as u32, MOUSEEVENTF_HWHEEL));
        }
        if !events.is_empty() {
            send(&events);
        }
    }

    pub fn key(&mut self, vk: u16, scan: u16, ext: bool, down: bool) {
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        if ext {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }

        // Prefer the physical key. A scancode of 0 means the host could not
        // supply one (some virtual keys have none), so fall back to the vk.
        let (vk_field, scan_field) = if scan != 0 {
            flags |= KEYEVENTF_SCANCODE;
            (0, scan)
        } else {
            (vk, 0)
        };

        send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk_field),
                    wScan: scan_field,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: TAG,
                },
            },
        }]);
    }

    pub fn text(&mut self, text: &str) {
        // UTF-16 with KEYEVENTF_UNICODE bypasses the layout entirely, which is
        // the only way to reproduce a character the local layout cannot type.
        let mut events = Vec::with_capacity(text.len() * 4);
        for unit in text.encode_utf16() {
            for up in [false, true] {
                let mut flags = KEYEVENTF_UNICODE;
                if up {
                    flags |= KEYEVENTF_KEYUP;
                }
                events.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: unit,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: TAG,
                        },
                    },
                });
            }
        }
        if !events.is_empty() {
            send(&events);
        }
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: TAG,
            },
        },
    }
}

fn send(events: &[INPUT]) {
    unsafe { SendInput(events, size_of::<INPUT>() as i32) };
}

fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Maps a pixel offset into the 0..=65535 range `SendInput` wants, rounding to
/// the nearest pixel centre so the cursor lands where the host said.
fn normalise(offset: i32, extent: i32) -> i32 {
    let extent = extent.max(1) as i64;
    let value = (offset as i64 * 65535 + extent / 2) / extent;
    value.clamp(0, 65535) as i32
}

pub fn warp(x: i32, y: i32) {
    unsafe {
        let _ = SetCursorPos(x, y);
    }
}

pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}
