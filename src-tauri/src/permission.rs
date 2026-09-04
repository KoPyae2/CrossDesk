//! Whether the OS will actually let this machine replay the host's input.
//!
//! Only macOS withholds this, and it withholds it in silence: every injected
//! event there ends up in `CGEventPost`, which returns `void`. A refused event is
//! indistinguishable from a delivered one at the call site, so a client without
//! permission connects, reports itself connected, counts the packets it receives
//! and moves nothing. Nothing fails, so nothing can be reported from the
//! injection path — which is why permission is asked about explicitly here rather
//! than inferred from an error that never arrives.

use std::time::Duration;

use serde::Serialize;

use crate::inject;

/// How long [`probe`] waits before reading the pointer back.
///
/// Injected movement is asynchronous, so an immediate read still reports the old
/// position and every probe would report failure. Several frames at 60 Hz.
const SETTLE: Duration = Duration::from_millis(120);

/// How far [`probe`] nudges the pointer: enough to survive rounding between
/// points and backing pixels, little enough to read as a twitch.
const NUDGE: i32 = 12;

/// What the OS claims about replaying input on this machine.
///
/// `Granted` and `Denied` are only ever constructed on macOS — everywhere else
/// there is nothing to gate — but the enum stays whole so the wire shape the UI
/// sees does not change per platform.
#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// This platform does not gate synthetic input, so there is nothing to grant
    /// and nothing to warn about: Windows, and X11 through XTEST.
    NotNeeded,
    Granted,
    Denied,
}

/// The result of actually trying to move this machine's pointer.
#[derive(Serialize, Clone)]
pub struct Probe {
    /// True only when the pointer demonstrably moved — the one fact in here the
    /// OS cannot be wrong about.
    pub moved: bool,
    /// What the OS claims, which `moved` is allowed to contradict.
    pub access: Access,
    /// One sentence naming the outcome and, on failure, what to do about it.
    pub detail: String,
}

/// Reports whether this process may post synthetic events.
///
/// Two separate records are consulted and either one answering yes is enough:
///
/// - `CGPreflightPostEventAccess` is the check CoreGraphics applies to
///   `CGEventPost` itself, so it speaks for the exact operation this app performs.
/// - `AXIsProcessTrusted` reports membership of the Accessibility list, which is
///   the list the user is actually told to tick.
///
/// Asking only the second is the bug behind "I already gave permission and it
/// still says permission required": the answer a *running* process gets from the
/// Accessibility list does not reliably change at the moment the box is ticked, so
/// a UI that polls it keeps insisting the permission is missing while System
/// Settings plainly shows it on. Whichever record has caught up wins.
///
/// Neither call prompts, which is what makes this safe on a 250 ms UI tick.
#[cfg(target_os = "macos")]
pub fn status() -> Access {
    // SAFETY: neither function takes arguments, and both only read process state.
    if unsafe { CGPreflightPostEventAccess() != 0 || AXIsProcessTrusted() != 0 } {
        Access::Granted
    } else {
        Access::Denied
    }
}

#[cfg(not(target_os = "macos"))]
pub fn status() -> Access {
    Access::NotNeeded
}

/// Asks macOS for permission, prompting if the user has not decided yet.
///
/// This is also how the *right* binary reaches the Accessibility list, which is
/// the fix for the other half of the problem. Adding an app by hand with **+**
/// means choosing a path, and choosing the wrong one — the packaged `.app` rather
/// than the binary `tauri dev` is running, or a stale copy of either — grants
/// permission to something that is not running: the list shows CrossDesk enabled
/// while every event posted is still dropped. Letting macOS register the caller
/// removes that failure entirely.
///
/// Called from [`open_settings`], which is the only sensible entry point: asking
/// and then showing the user the list are one action from their side.
#[cfg(target_os = "macos")]
fn request() -> Access {
    // SAFETY: may present a system prompt to register this exact running binary.
    unsafe {
        CGRequestPostEventAccess();
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let dict = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        if !dict.is_null() {
            let _ = AXIsProcessTrustedWithOptions(dict);
            CFRelease(dict);
        }
    }
    status()
}

/// Opens System Settings on the Accessibility pane.
///
/// Asks for access first, because that is the step that can actually fix things:
/// it puts *this* binary in the list the pane then shows, so there is something
/// correct to tick instead of a path to hunt for.
///
/// `open` rather than `NSWorkspace`: it needs no AppKit dependency, and the URL
/// scheme is honoured by System Settings and pre-Ventura System Preferences alike.
#[cfg(target_os = "macos")]
pub fn open_settings() -> Result<(), String> {
    let _ = request();
    std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .status()
        .map_err(|e| format!("could not open System Settings: {e}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err("macOS declined to open System Settings".into())
            }
        })
}

#[cfg(not(target_os = "macos"))]
pub fn open_settings() -> Result<(), String> {
    Err("there is no input permission to grant on this platform".into())
}

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> u8;
    static kAXTrustedCheckOptionPrompt: *const std::ffi::c_void;
    fn CFDictionaryCreate(
        allocator: *const std::ffi::c_void,
        keys: *const *const std::ffi::c_void,
        values: *const *const std::ffi::c_void,
        numValues: isize,
        keyCallBacks: *const std::ffi::c_void,
        valueCallBacks: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    static kCFBooleanTrue: *const std::ffi::c_void;
    static kCFTypeDictionaryKeyCallBacks: std::ffi::c_void;
    static kCFTypeDictionaryValueCallBacks: std::ffi::c_void;
    fn CFRelease(cf: *const std::ffi::c_void);
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Whether this process may post events, without prompting (macOS 10.15+).
    fn CGPreflightPostEventAccess() -> u8;
    /// Requests permission to post events, prompting once if undecided.
    fn CGRequestPostEventAccess() -> u8;
}

/// Moves the pointer and reads it back. The only way to settle a question the OS
/// answers unreliably, and the only check that survives a stale grant.
///
/// Two nudges in opposite directions, because one is not conclusive: a pointer
/// already wedged in the bottom-right corner cannot move further that way, and a
/// clamped move looks exactly like a refused one. The pointer is put back
/// afterwards, so running this is not disruptive.
///
/// Blocking, and it sleeps: call it from a command, never from the UI tick.
pub fn probe() -> Probe {
    let access = status();

    let Some(start) = inject::cursor_position() else {
        return Probe {
            moved: false,
            access,
            // On macOS a refused process cannot read the pointer either, so the
            // permission wording is the useful answer rather than "could not
            // read". Elsewhere the read itself is the news.
            detail: if matches!(access, Access::NotNeeded) {
                "could not read the pointer position, so movement cannot be verified".into()
            } else {
                detail_for(false, access)
            },
        };
    };

    let mut at = start;
    for (dx, dy) in [(NUDGE, NUDGE), (-NUDGE, -NUDGE)] {
        inject::warp(at.0 + dx, at.1 + dy);
        std::thread::sleep(SETTLE);
        match inject::cursor_position() {
            Some(now) => {
                at = now;
                if now != start {
                    break;
                }
            }
            None => break,
        }
    }

    let moved = at != start;
    if moved {
        inject::warp(start.0, start.1);
    }

    Probe {
        moved,
        access,
        detail: detail_for(moved, access),
    }
}

/// The one sentence the user should read, given what happened.
fn detail_for(moved: bool, access: Access) -> String {
    if moved {
        return "the pointer moved: this machine can replay the host's input".into();
    }
    if cfg!(target_os = "macos") {
        // Deliberately blunt about a grant that is lying. This is the case that
        // wastes the most of the user's time, because Settings shows CrossDesk
        // enabled while every event posted is dropped: macOS keys the grant to a
        // code signature and a bundle path, and a rebuilt unsigned binary is a
        // different identity from the one that was ticked.
        return match access {
            Access::Granted => "macOS reports Accessibility as granted, but the pointer did not \
                 move, so the grant belongs to a different build of CrossDesk. Remove CrossDesk \
                 from System Settings → Privacy & Security → Accessibility with the \"−\" button, \
                 then press Grant permission here and restart CrossDesk."
                .into(),
            _ => "macOS has not granted Accessibility to this copy of CrossDesk, so injected \
                 input is being dropped."
                .into(),
        };
    }
    "the pointer did not move, and this platform reported no reason why".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wording exists to stop the user trusting a grant that is lying, so it
    /// has to say so even when the OS claims the permission is fine.
    #[test]
    fn a_granted_permission_that_did_not_move_is_called_out() {
        let text = detail_for(false, Access::Granted);
        if cfg!(target_os = "macos") {
            assert!(text.contains("different build"), "{text}");
            assert!(text.contains("Accessibility"), "{text}");
        }
        assert!(!text.contains("the pointer moved"), "{text}");
    }

    /// Movement is the ground truth; a denied claim cannot override it.
    #[test]
    fn movement_wins_over_whatever_the_os_claims() {
        assert!(detail_for(true, Access::Denied).contains("the pointer moved"));
    }

    /// Nothing to grant off macOS, so nothing may nag about it.
    #[test]
    fn other_platforms_need_no_permission() {
        if !cfg!(target_os = "macos") {
            assert!(status() == Access::NotNeeded);
        }
    }
}
