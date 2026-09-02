//! Windows input capture: low-level hooks for suppression, Raw Input for motion.
//!
//! Both live on one thread with a message-only window. Hook callbacks are on
//! the critical path of every input event in the system and Windows silently
//! unhooks a callback that is too slow, so they only ever do a channel send.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use parking_lot::Mutex;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    PostThreadMessageW, RegisterClassW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK,
    HWND_MESSAGE, KBDLLHOOKSTRUCT, KBDLLHOOKSTRUCT_FLAGS, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSW, WS_OVERLAPPED,
};

use super::{Capture, CaptureError, CaptureEvent};
use crate::protocol::Button;

/// While set, hook callbacks swallow the event instead of passing it on.
static SUPPRESS: AtomicBool = AtomicBool::new(false);
/// Modifier state, tracked in the hook so the escape hotkey works even when
/// every key is being suppressed.
static MODIFIERS: AtomicU32 = AtomicU32::new(0);
static SENDER: Mutex<Option<UnboundedSender<CaptureEvent>>> = Mutex::new(None);

const MOD_CTRL: u32 = 1;
const MOD_ALT: u32 = 2;
const MOD_SHIFT: u32 = 4;

const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_LSHIFT: u16 = 0xA0;
const VK_RSHIFT: u16 = 0xA1;
const VK_LCONTROL: u16 = 0xA2;
const VK_RCONTROL: u16 = 0xA3;
const VK_LMENU: u16 = 0xA4;
const VK_RMENU: u16 = 0xA5;
/// Escape hotkey: Ctrl+Alt+Home brings the pointer back to the host.
const VK_HOME: u16 = 0x24;

/// Marker written into `dwExtraInfo` by our own injected input, so a host that
/// is also acting as a client never captures its own synthetic events.
pub const INJECTED_TAG: usize = 0x5844_534B; // "XDSK"

pub fn set_suppressed(on: bool) {
    SUPPRESS.store(on, Ordering::Relaxed);
}

fn emit(event: CaptureEvent) {
    if let Some(tx) = SENDER.lock().as_ref() {
        let _ = tx.send(event);
    }
}

fn ours(extra: usize) -> bool {
    extra == INJECTED_TAG
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if !ours(data.dwExtraInfo) {
            let suppress = SUPPRESS.load(Ordering::Relaxed);
            let hi = |v: u32| ((v >> 16) & 0xFFFF) as u16 as i16;

            let event = match wparam.0 as u32 {
                WM_LBUTTONDOWN => Some(CaptureEvent::Button {
                    button: Button::Left,
                    down: true,
                }),
                WM_LBUTTONUP => Some(CaptureEvent::Button {
                    button: Button::Left,
                    down: false,
                }),
                WM_RBUTTONDOWN => Some(CaptureEvent::Button {
                    button: Button::Right,
                    down: true,
                }),
                WM_RBUTTONUP => Some(CaptureEvent::Button {
                    button: Button::Right,
                    down: false,
                }),
                WM_MBUTTONDOWN => Some(CaptureEvent::Button {
                    button: Button::Middle,
                    down: true,
                }),
                WM_MBUTTONUP => Some(CaptureEvent::Button {
                    button: Button::Middle,
                    down: false,
                }),
                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    let down = wparam.0 as u32 == WM_XBUTTONDOWN;
                    let button = if hi(data.mouseData) == 2 {
                        Button::Forward
                    } else {
                        Button::Back
                    };
                    Some(CaptureEvent::Button { button, down })
                }
                WM_MOUSEWHEEL => Some(CaptureEvent::Wheel {
                    dx: 0,
                    dy: hi(data.mouseData),
                }),
                WM_MOUSEHWHEEL => Some(CaptureEvent::Wheel {
                    dx: hi(data.mouseData),
                    dy: 0,
                }),
                // Absolute position, only acted on while control is local:
                // it keeps the wall in step with anything else moving the cursor.
                _ => Some(CaptureEvent::MotionAbs {
                    x: data.pt.x,
                    y: data.pt.y,
                }),
            };

            if let Some(event) = event {
                emit(event);
            }
            if suppress {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let data = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if !ours(data.dwExtraInfo) {
            let msg = wparam.0 as u32;
            let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            let up = matches!(msg, WM_KEYUP | WM_SYSKEYUP);
            let vk = data.vkCode as u16;

            if down || up {
                if let Some(bit) = modifier_bit(vk) {
                    let mut mods = MODIFIERS.load(Ordering::Relaxed);
                    mods = if down { mods | bit } else { mods & !bit };
                    MODIFIERS.store(mods, Ordering::Relaxed);
                }

                let mods = MODIFIERS.load(Ordering::Relaxed);
                if down && vk == VK_HOME && mods & MOD_CTRL != 0 && mods & MOD_ALT != 0 {
                    emit(CaptureEvent::Escape);
                    return LRESULT(1);
                }

                emit(CaptureEvent::Key {
                    vk,
                    scan: data.scanCode as u16,
                    ext: data.flags & LLKHF_EXTENDED != KBDLLHOOKSTRUCT_FLAGS(0),
                    down,
                });

                if SUPPRESS.load(Ordering::Relaxed) {
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn modifier_bit(vk: u16) -> Option<u32> {
    Some(match vk {
        VK_CONTROL | VK_LCONTROL | VK_RCONTROL => MOD_CTRL,
        VK_MENU | VK_LMENU | VK_RMENU => MOD_ALT,
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT => MOD_SHIFT,
        _ => return None,
    })
}

/// Raw Input is the only source of true device deltas: it keeps reporting
/// "moved right" after the cursor has already been pinned to the screen edge,
/// which is exactly the moment the wall needs to know about.
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        let mut raw = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        let read = unsafe {
            GetRawInputData(
                HRAWINPUT(lparam.0 as *mut c_void),
                RID_INPUT,
                Some(&mut raw as *mut RAWINPUT as *mut c_void),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };

        if read != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE.0 {
            let mouse = unsafe { &raw.data.mouse };
            let absolute = mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0;
            // Absolute-mode devices (tablets, RDP) carry no usable delta; the
            // hook's absolute position covers those instead.
            if !absolute
                && !ours(mouse.ulExtraInformation as usize)
                && (mouse.lLastX != 0 || mouse.lLastY != 0)
            {
                emit(CaptureEvent::Motion {
                    dx: mouse.lLastX,
                    dy: mouse.lLastY,
                });
            }
        }
    }
    // WM_INPUT still has to reach DefWindowProc so the system can clean up.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Everything the capture thread owns, so `Drop` order is explicit.
struct Hooks {
    mouse: HHOOK,
    keyboard: HHOOK,
}

impl Drop for Hooks {
    fn drop(&mut self) {
        // Must happen on the installing thread, which is why this lives here.
        unsafe {
            let _ = UnhookWindowsHookEx(self.mouse);
            let _ = UnhookWindowsHookEx(self.keyboard);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Creates the message-only window and subscribes it to raw mouse input.
fn setup_window(instance: HINSTANCE) -> Result<HWND, CaptureError> {
    let class = wide("CrossDeskCapture");

    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance,
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    // A second start would fail here because the class already exists; the real
    // error, if any, surfaces from CreateWindowExW.
    unsafe { RegisterClassW(&wc) };

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(class.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|e| CaptureError::HookFailed(e.code().0 as u32))?;

    // Usage page 1 ("generic desktop"), usage 2 ("mouse"). INPUTSINK keeps the
    // events coming while the window is in the background, which it always is.
    let devices = [RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    }];
    unsafe { RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32) }
        .map_err(|e| CaptureError::HookFailed(e.code().0 as u32))?;

    Ok(hwnd)
}

fn setup_hooks(instance: HINSTANCE) -> Result<Hooks, CaptureError> {
    // Low-level hooks are global (thread id 0) and ignore the module handle,
    // but it is passed anyway for correctness.
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(instance), 0) }
        .map_err(|e| CaptureError::HookFailed(e.code().0 as u32))?;
    let keyboard =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(instance), 0) } {
            Ok(h) => h,
            Err(e) => {
                unsafe { UnhookWindowsHookEx(mouse) }.ok();
                return Err(CaptureError::HookFailed(e.code().0 as u32));
            }
        };
    Ok(Hooks { mouse, keyboard })
}

/// Starts the capture thread.
///
/// Only one `Capture` may exist at a time: the hooks and the event sender are
/// process-wide state, which is what Windows gives us to work with.
pub fn start() -> Result<Capture, CaptureError> {
    let (tx, rx) = unbounded_channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, CaptureError>>();

    std::thread::Builder::new()
        .name("crossdesk-capture".into())
        .spawn(move || {
            // Input is worthless late, so this thread outranks ordinary work.
            unsafe {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            }

            let instance: HINSTANCE = match unsafe { GetModuleHandleW(PCWSTR::null()) } {
                Ok(m) => m.into(),
                Err(e) => {
                    let _ = ready_tx.send(Err(CaptureError::HookFailed(e.code().0 as u32)));
                    return;
                }
            };

            let hwnd = match setup_window(instance) {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = hwnd;

            *SENDER.lock() = Some(tx);

            let hooks = match setup_hooks(instance) {
                Ok(h) => h,
                Err(e) => {
                    *SENDER.lock() = None;
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };

            let thread_id = unsafe { GetCurrentThreadId() };
            if ready_tx.send(Ok(thread_id)).is_err() {
                drop(hooks);
                *SENDER.lock() = None;
                return;
            }

            let mut msg = MSG::default();
            // Returns 0 on WM_QUIT, -1 on error: either way we are done.
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                unsafe { DispatchMessageW(&msg) };
            }

            drop(hooks);
            *SENDER.lock() = None;
            SUPPRESS.store(false, Ordering::Relaxed);
            MODIFIERS.store(0, Ordering::Relaxed);
        })
        .map_err(|_| CaptureError::Unsupported)?;

    let thread_id = ready_rx.recv().map_err(|_| CaptureError::Unsupported)??;

    Ok(Capture {
        events: rx,
        thread_id,
    })
}

/// Asks the capture thread to unwind. The thread does the unhooking itself.
pub fn stop(thread_id: u32) {
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
    }
}

