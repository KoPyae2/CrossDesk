//! Native macOS input injection via CoreGraphics.
//!
//! Replaces the portable `enigo` layer on macOS:
//! 1. Posts keyboard events to `kCGSessionEventTap`, which routes properly to the
//!    focused application (whereas `kCGHIDEventTap` silently drops keyboard events
//!    for non-root processes in modern macOS).
//! 2. Maps Windows virtual key codes directly to macOS `CGKeyCode`s without
//!    slow runtime layout queries that fail on non-main threads.
//! 3. Correctly preserves and applies modifier flags (Shift, Command, Option, Control)
//!    and sets Unicode string representation for printable characters.

use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use super::InjectError;
use crate::permission::{self, Access};
use crate::protocol::Button;

// Modifier flag masks for CoreGraphics events
const MASK_SHIFT: u64 = 0x0002_0000;
const MASK_CONTROL: u64 = 0x0004_0000;
const MASK_OPTION: u64 = 0x0008_0000;
const MASK_COMMAND: u64 = 0x0010_0000;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWarpMouseCursorPosition(newCursorPosition: CGPoint) -> i32;
}

pub struct Backend {
    source: CGEventSource,
    cursor: CGPoint,
    active_flags: u64,
    left_down: bool,
    right_down: bool,
    other_down: bool,
    wheel_x: i32,
    wheel_y: i32,
}

impl Backend {
    pub fn new() -> Result<Self, InjectError> {
        if permission::status() == Access::Denied {
            return Err(InjectError::Blocked(
                "macOS has not granted Accessibility permission to this copy of CrossDesk, so it \
                 cannot replay the host's input yet. Use Grant permission on this screen — adding \
                 the app by hand often grants it to a different build."
                    .into(),
            ));
        }

        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| InjectError::Unavailable("failed to create CGEventSource".into()))?;

        let cursor = cursor_location().unwrap_or(CGPoint::new(0.0, 0.0));

        Ok(Self {
            source,
            cursor,
            active_flags: 0,
            left_down: false,
            right_down: false,
            other_down: false,
            wheel_x: 0,
            wheel_y: 0,
        })
    }

    pub fn move_abs(&mut self, x: i32, y: i32) {
        self.cursor = CGPoint::new(x as f64, y as f64);
        let event_type = if self.left_down {
            CGEventType::LeftMouseDragged
        } else if self.right_down {
            CGEventType::RightMouseDragged
        } else if self.other_down {
            CGEventType::OtherMouseDragged
        } else {
            CGEventType::MouseMoved
        };

        let button = if self.right_down {
            CGMouseButton::Right
        } else if self.other_down {
            CGMouseButton::Center
        } else {
            CGMouseButton::Left
        };

        if let Ok(event) = CGEvent::new_mouse_event(self.source.clone(), event_type, self.cursor, button) {
            if self.active_flags != 0 {
                event.set_flags(CGEventFlags::from_bits_truncate(self.active_flags));
            }
            event.post(CGEventTapLocation::HID);
        }
    }

    pub fn button(&mut self, button: Button, down: bool) {
        let (event_type, cg_button, button_num) = match button {
            Button::Left => {
                self.left_down = down;
                (
                    if down { CGEventType::LeftMouseDown } else { CGEventType::LeftMouseUp },
                    CGMouseButton::Left,
                    0,
                )
            }
            Button::Right => {
                self.right_down = down;
                (
                    if down { CGEventType::RightMouseDown } else { CGEventType::RightMouseUp },
                    CGMouseButton::Right,
                    1,
                )
            }
            Button::Middle => {
                self.other_down = down;
                (
                    if down { CGEventType::OtherMouseDown } else { CGEventType::OtherMouseUp },
                    CGMouseButton::Center,
                    2,
                )
            }
            Button::Back => {
                self.other_down = down;
                (
                    if down { CGEventType::OtherMouseDown } else { CGEventType::OtherMouseUp },
                    CGMouseButton::Center,
                    3,
                )
            }
            Button::Forward => {
                self.other_down = down;
                (
                    if down { CGEventType::OtherMouseDown } else { CGEventType::OtherMouseUp },
                    CGMouseButton::Center,
                    4,
                )
            }
        };

        if let Ok(event) = CGEvent::new_mouse_event(self.source.clone(), event_type, self.cursor, cg_button) {
            if button_num >= 2 {
                event.set_integer_value_field(
                    core_graphics::event::EventField::MOUSE_EVENT_BUTTON_NUMBER,
                    button_num,
                );
            }
            if self.active_flags != 0 {
                event.set_flags(CGEventFlags::from_bits_truncate(self.active_flags));
            }
            event.post(CGEventTapLocation::HID);
        }
    }

    pub fn wheel(&mut self, dx: i16, dy: i16) {
        // Windows reports wheel in 1/120 notch units.
        // Scroll with Pixel units for high precision.
        self.wheel_y += dy as i32;
        self.wheel_x += dx as i32;

        let py = self.wheel_y / 10;
        let px = self.wheel_x / 10;

        if py != 0 || px != 0 {
            self.wheel_y -= py * 10;
            self.wheel_x -= px * 10;

            if let Ok(event) = CGEvent::new_scroll_event(
                self.source.clone(),
                ScrollEventUnit::PIXEL,
                2,
                py,
                px,
                0,
            ) {
                if self.active_flags != 0 {
                    event.set_flags(CGEventFlags::from_bits_truncate(self.active_flags));
                }
                event.post(CGEventTapLocation::HID);
            }
        }
    }

    pub fn key(&mut self, vk: u16, scan: u16, ext: bool, down: bool) {
        let Some((keycode, ch)) = vk_to_mac_keycode(vk, scan, ext) else {
            return;
        };

        // Update modifier bitmask
        let mod_mask = match keycode {
            0x38 | 0x3C => Some(MASK_SHIFT),     // Left / Right Shift
            0x3B | 0x3E => Some(MASK_CONTROL),   // Left / Right Control
            0x3A | 0x3D => Some(MASK_OPTION),    // Left / Right Option (Alt)
            0x37 | 0x36 => Some(MASK_COMMAND),   // Left / Right Command (Win)
            _ => None,
        };

        if let Some(mask) = mod_mask {
            if down {
                self.active_flags |= mask;
            } else {
                self.active_flags &= !mask;
            }
        }

        if let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), keycode, down) {
            // Apply current modifiers
            if self.active_flags != 0 {
                event.set_flags(CGEventFlags::from_bits_truncate(self.active_flags));
            }

            // Set Unicode string for printable keys if available
            if down {
                if let Some(c) = ch {
                    event.set_string(&c.to_string());
                }
            }

            // Post to Session tap so it is delivered to the currently focused application
            event.post(CGEventTapLocation::Session);
            event.post(CGEventTapLocation::AnnotatedSession);
        }
    }

    pub fn text(&mut self, text: &str) {
        for ch in text.chars() {
            if let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), 0, true) {
                event.set_string(&ch.to_string());
                event.post(CGEventTapLocation::Session);
            }
            if let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), 0, false) {
                event.post(CGEventTapLocation::Session);
            }
        }
    }
}

pub fn warp(x: i32, y: i32) {
    let pt = CGPoint::new(x as f64, y as f64);
    unsafe {
        CGWarpMouseCursorPosition(pt);
    }
}

pub fn cursor_position() -> Option<(i32, i32)> {
    cursor_location().map(|p| (p.x as i32, p.y as i32))
}

fn cursor_location() -> Option<CGPoint> {
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    let event = CGEvent::new(source).ok()?;
    Some(event.location())
}

/// Translates Windows virtual-key codes and scancodes to macOS `CGKeyCode`
/// and optional character representation.
pub fn vk_to_mac_keycode(vk: u16, _scan: u16, _ext: bool) -> Option<(CGKeyCode, Option<char>)> {
    Some(match vk {
        // Letters (A - Z)
        0x41 => (0x00, Some('a')),
        0x42 => (0x0B, Some('b')),
        0x43 => (0x08, Some('c')),
        0x44 => (0x02, Some('d')),
        0x45 => (0x0E, Some('e')),
        0x46 => (0x03, Some('f')),
        0x47 => (0x05, Some('g')),
        0x48 => (0x04, Some('h')),
        0x49 => (0x22, Some('i')),
        0x4A => (0x26, Some('j')),
        0x4B => (0x28, Some('k')),
        0x4C => (0x25, Some('l')),
        0x4D => (0x2E, Some('m')),
        0x4E => (0x2D, Some('n')),
        0x4F => (0x1F, Some('o')),
        0x50 => (0x23, Some('p')),
        0x51 => (0x0C, Some('q')),
        0x52 => (0x0F, Some('r')),
        0x53 => (0x01, Some('s')),
        0x54 => (0x11, Some('t')),
        0x55 => (0x20, Some('u')),
        0x56 => (0x09, Some('v')),
        0x57 => (0x0D, Some('w')),
        0x58 => (0x07, Some('x')),
        0x59 => (0x10, Some('y')),
        0x5A => (0x06, Some('z')),

        // Numbers (0 - 9 top row)
        0x30 => (0x1D, Some('0')),
        0x31 => (0x12, Some('1')),
        0x32 => (0x13, Some('2')),
        0x33 => (0x14, Some('3')),
        0x34 => (0x15, Some('4')),
        0x35 => (0x17, Some('5')),
        0x36 => (0x16, Some('6')),
        0x37 => (0x1A, Some('7')),
        0x38 => (0x1C, Some('8')),
        0x39 => (0x19, Some('9')),

        // Punctuation
        0xBA => (0x29, Some(';')),
        0xBB => (0x18, Some('=')),
        0xBC => (0x2B, Some(',')),
        0xBD => (0x1B, Some('-')),
        0xBE => (0x2F, Some('.')),
        0xBF => (0x2C, Some('/')),
        0xC0 => (0x32, Some('`')),
        0xDB => (0x21, Some('[')),
        0xDC => (0x2A, Some('\\')),
        0xDD => (0x1E, Some(']')),
        0xDE => (0x27, Some('\'')),

        // Basic Control & Navigation
        0x08 => (0x33, None), // Backspace (Mac Delete)
        0x09 => (0x30, None), // Tab
        0x0D => (0x24, None), // Return
        0x1B => (0x35, None), // Escape
        0x20 => (0x31, Some(' ')), // Space
        0x21 => (0x74, None), // Page Up
        0x22 => (0x79, None), // Page Down
        0x23 => (0x77, None), // End
        0x24 => (0x73, None), // Home
        0x25 => (0x7B, None), // Left Arrow
        0x26 => (0x7E, None), // Up Arrow
        0x27 => (0x7C, None), // Right Arrow
        0x28 => (0x7D, None), // Down Arrow
        0x2D => (0x72, None), // Insert (Mac Help)
        0x2E => (0x75, None), // Forward Delete
        0x14 => (0x39, None), // Caps Lock

        // Modifiers
        0x10 | 0xA0 => (0x38, None), // Shift / Left Shift
        0xA1 => (0x3C, None),        // Right Shift
        0x11 | 0xA2 => (0x3B, None), // Control / Left Control
        0xA3 => (0x3E, None),        // Right Control
        0x12 | 0xA4 => (0x3A, None), // Alt / Left Alt (Option)
        0xA5 => (0x3D, None),        // Right Alt (Option)
        0x5B => (0x37, None),        // Left Windows (Command)
        0x5C => (0x36, None),        // Right Windows (Command)

        // Numpad
        0x60 => (0x52, Some('0')),
        0x61 => (0x53, Some('1')),
        0x62 => (0x54, Some('2')),
        0x63 => (0x55, Some('3')),
        0x64 => (0x56, Some('4')),
        0x65 => (0x57, Some('5')),
        0x66 => (0x58, Some('6')),
        0x67 => (0x59, Some('7')),
        0x68 => (0x5B, Some('8')),
        0x69 => (0x5C, Some('9')),
        0x6A => (0x43, Some('*')),
        0x6B => (0x45, Some('+')),
        0x6C => (0x4C, None), // Numpad Enter
        0x6D => (0x4E, Some('-')),
        0x6E => (0x41, Some('.')),
        0x6F => (0x4B, Some('/')),

        // Function Keys (F1 - F20)
        0x70 => (0x7A, None),
        0x71 => (0x78, None),
        0x72 => (0x63, None),
        0x73 => (0x76, None),
        0x74 => (0x60, None),
        0x75 => (0x61, None),
        0x76 => (0x62, None),
        0x77 => (0x64, None),
        0x78 => (0x65, None),
        0x79 => (0x6D, None),
        0x7A => (0x67, None),
        0x7B => (0x6F, None),
        0x7C => (0x69, None), // F13
        0x7D => (0x6B, None), // F14
        0x7E => (0x71, None), // F15
        0x7F => (0x6A, None), // F16
        0x80 => (0x40, None), // F17
        0x81 => (0x4F, None), // F18
        0x82 => (0x50, None), // F19
        0x83 => (0x5A, None), // F20

        // Media / Volume
        0xAD => (0x4A, None), // Mute
        0xAE => (0x49, None), // Volume Down
        0xAF => (0x48, None), // Volume Up

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vk_to_mac_keycodes() {
        assert_eq!(vk_to_mac_keycode(0x41, 0, false), Some((0x00, Some('a'))));
        assert_eq!(vk_to_mac_keycode(0x0D, 0, false), Some((0x24, None)));
        assert_eq!(vk_to_mac_keycode(0x10, 0, false), Some((0x38, None)));
        assert_eq!(vk_to_mac_keycode(0x5B, 0, false), Some((0x37, None)));
        assert_eq!(vk_to_mac_keycode(0x20, 0, false), Some((0x31, Some(' '))));
    }
}
