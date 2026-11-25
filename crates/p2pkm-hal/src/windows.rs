use crate::{
    ClipboardContent, ClipboardProvider, DisplayInfo, HalError, HookControl, InputInjector,
    RawInputEvent, INJECTED_MAGIC_TAG,
};
use p2pkm_proto::MouseButton;
use rtrb::Producer;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use windows::core::w;
use windows::Win32::Foundation::{BOOL, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
const MONITORINFOF_PRIMARY: u32 = 1;
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    VK_CONTROL, VK_ESCAPE, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos,
    GetMessageW, GetSystemMetrics, RegisterClassW, SetCursorPos, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLMHF_INJECTED,
    LLKHF_INJECTED, MSG, MSLLHOOKSTRUCT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_CLIPBOARDUPDATE,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
};

struct HookState {
    ctrl: Arc<HookControl>,
    prod: Producer<RawInputEvent>,
    last_pt: Option<(i32, i32)>,
}

thread_local! {
    static HOOK_STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

pub fn enable_per_monitor_dpi() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn get_cursor_pos() -> Option<(i32, i32)> {
    let mut pt = POINT::default();
    unsafe {
        if GetCursorPos(&mut pt).is_ok() {
            Some((pt.x, pt.y))
        } else {
            None
        }
    }
}

pub fn set_cursor_pos(x: i32, y: i32) {
    unsafe {
        let _ = SetCursorPos(x, y);
    }
}

pub fn enumerate_displays() -> Result<Vec<DisplayInfo>, HalError> {
    enable_per_monitor_dpi();
    let mut displays: Vec<DisplayInfo> = Vec::new();
    let ptr = &mut displays as *mut Vec<DisplayInfo>;

    unsafe extern "system" fn enum_proc(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let list = &mut *(lparam.0 as *mut Vec<DisplayInfo>);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmonitor, &mut info).as_bool() {
            let mut dpi_x: u32 = 96;
            let mut dpi_y: u32 = 96;
            let _ = GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            let r = info.rcMonitor;
            list.push(DisplayInfo {
                id: hmonitor.0 as u32,
                is_primary: (info.dwFlags & MONITORINFOF_PRIMARY) != 0,
                x: r.left,
                y: r.top,
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                scale_factor: dpi_x as f32 / 96.0,
            });
        }
        BOOL(1)
    }

    let ok = unsafe { EnumDisplayMonitors(None, None, Some(enum_proc), LPARAM(ptr as isize)) };
    if !ok.as_bool() {
        return Err(HalError::DisplayEnum("EnumDisplayMonitors failed".into()));
    }
    Ok(displays)
}

#[inline]
unsafe fn is_panic_hotkey_down(vk: u32) -> bool {
    if vk != VK_ESCAPE.0 as u32 {
        return false;
    }
    let ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
    let alt = (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0;
    let shift = (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
    ctrl && alt && shift
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if info.dwExtraInfo == INJECTED_MAGIC_TAG || (info.flags & LLMHF_INJECTED) != 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let mut suppress = false;
        HOOK_STATE.with(|cell| {
            if let Some(st) = cell.borrow_mut().as_mut() {
                suppress = st.ctrl.is_suppressed();
                let msg = wparam.0 as u32;
                match msg {
                    WM_MOUSEMOVE => {
                        let (dx, dy) = if suppress {
                            if let Some((cx, cy)) = get_cursor_pos() {
                                (info.pt.x - cx, info.pt.y - cy)
                            } else {
                                (0, 0)
                            }
                        } else {
                            let d = match st.last_pt {
                                Some((lx, ly)) => (info.pt.x - lx, info.pt.y - ly),
                                None => (0, 0),
                            };
                            st.last_pt = Some((info.pt.x, info.pt.y));
                            d
                        };
                        let _ = st.prod.push(RawInputEvent::MouseMove {
                            dx,
                            dy,
                            abs_x: info.pt.x,
                            abs_y: info.pt.y,
                        });
                    }
                    WM_LBUTTONDOWN | WM_LBUTTONUP => {
                        let _ = st.prod.push(RawInputEvent::MouseButton {
                            button: MouseButton::Left,
                            pressed: msg == WM_LBUTTONDOWN,
                        });
                    }
                    WM_RBUTTONDOWN | WM_RBUTTONUP => {
                        let _ = st.prod.push(RawInputEvent::MouseButton {
                            button: MouseButton::Right,
                            pressed: msg == WM_RBUTTONDOWN,
                        });
                    }
                    WM_MBUTTONDOWN | WM_MBUTTONUP => {
                        let _ = st.prod.push(RawInputEvent::MouseButton {
                            button: MouseButton::Middle,
                            pressed: msg == WM_MBUTTONDOWN,
                        });
                    }
                    WM_XBUTTONDOWN | WM_XBUTTONUP => {
                        let xbtn = ((info.mouseData >> 16) & 0xFFFF) as u16;
                        let button = if xbtn == 1 {
                            MouseButton::X1
                        } else {
                            MouseButton::X2
                        };
                        let _ = st.prod.push(RawInputEvent::MouseButton {
                            button,
                            pressed: msg == WM_XBUTTONDOWN,
                        });
                    }
                    WM_MOUSEWHEEL => {
                        let delta = ((info.mouseData >> 16) as i16) as i32;
                        let _ = st.prod.push(RawInputEvent::MouseScroll { dx: 0, dy: delta });
                    }
                    WM_MOUSEHWHEEL => {
                        let delta = ((info.mouseData >> 16) as i16) as i32;
                        let _ = st.prod.push(RawInputEvent::MouseScroll { dx: delta, dy: 0 });
                    }
                    _ => {}
                }
            }
        });

        if suppress {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if info.dwExtraInfo == INJECTED_MAGIC_TAG || (info.flags.0 & LLKHF_INJECTED.0) != 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let msg = wparam.0 as u32;
        let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
        let released = matches!(msg, WM_KEYUP | WM_SYSKEYUP);

        // Emergency Panic Hotkey checked BEFORE suppression
        if pressed && is_panic_hotkey_down(info.vkCode) {
            HOOK_STATE.with(|cell| {
                if let Some(st) = cell.borrow_mut().as_mut() {
                    st.ctrl.trigger_panic();
                    let _ = st.prod.push(RawInputEvent::PanicHotkey);
                }
            });
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let mut suppress = false;
        let is_modifier_vk = matches!(
            info.vkCode,
            0x10 | 0x11 | 0x12 | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5 | 0x5B | 0x5C
        );
        HOOK_STATE.with(|cell| {
            if let Some(st) = cell.borrow_mut().as_mut() {
                suppress = st.ctrl.is_suppressed();
                if suppress && (pressed || released) {
                    let extended = (info.flags.0 & 0x01) != 0;
                    if let Some(hid) = win_scancode_to_hid(info.scanCode as u16, extended) {
                        let _ = st.prod.push(RawInputEvent::Key {
                            hid_usage: hid,
                            pressed,
                        });
                    }
                }
            }
        });

        // Never swallow modifier KeyUp locally so Alt/Ctrl/Shift/Win can never get stuck
        // if pressed before an edge crossing and released after.
        if suppress && !(released && is_modifier_vk) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Blocking message loop for Win32 Low-Level Hooks. Run on a dedicated thread.
pub fn run_hook_loop(ctrl: Arc<HookControl>, prod: Producer<RawInputEvent>) -> Result<(), HalError> {
    enable_per_monitor_dpi();
    HOOK_STATE.with(|cell| {
        *cell.borrow_mut() = Some(HookState {
            ctrl,
            prod,
            last_pt: get_cursor_pos(),
        });
    });

    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        let h_mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0)
            .map_err(|e| HalError::HookInstall(e.to_string()))?;
        let h_kbd = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0)
            .map_err(|e| HalError::HookInstall(e.to_string()))?;

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(h_mouse);
        let _ = UnhookWindowsHookEx(h_kbd);
    }
    Ok(())
}

#[derive(Default)]
pub struct WinInputInjector {
    held_keys: Mutex<HashSet<u16>>,
    held_buttons: Mutex<HashSet<MouseButton>>,
}

impl WinInputInjector {
    pub fn new() -> Arc<Self> {
        enable_per_monitor_dpi();
        Arc::new(Self::default())
    }
}

impl InputInjector for WinInputInjector {
    fn inject_mouse_move(&self, dx: i32, dy: i32) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: INJECTED_MAGIC_TAG,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    }

    fn inject_mouse_abs(&self, x: i32, y: i32) {
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            let norm_x = (((x - vx) as f64 / vw as f64) * 65535.0).round() as i32;
            let norm_y = (((y - vy) as f64 / vh as f64) * 65535.0).round() as i32;

            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: norm_x,
                        dy: norm_y,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: INJECTED_MAGIC_TAG,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn inject_mouse_button(&self, button: MouseButton, pressed: bool) {
        {
            let mut guard = self.held_buttons.lock().unwrap();
            if pressed {
                guard.insert(button);
            } else {
                guard.remove(&button);
            }
        }
        let (flags, mouse_data) = match (button, pressed) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, 1),
            (MouseButton::X1, false) => (MOUSEEVENTF_XUP, 1),
            (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, 2),
            (MouseButton::X2, false) => (MOUSEEVENTF_XUP, 2),
        };
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: mouse_data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTED_MAGIC_TAG,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    }

    fn inject_mouse_scroll(&self, dx: i32, dy: i32) {
        let mut inputs = Vec::with_capacity(2);
        if dy != 0 {
            inputs.push(INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: dy as u32,
                        dwFlags: MOUSEEVENTF_WHEEL,
                        time: 0,
                        dwExtraInfo: INJECTED_MAGIC_TAG,
                    },
                },
            });
        }
        if dx != 0 {
            inputs.push(INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: dx as u32,
                        dwFlags: MOUSEEVENTF_HWHEEL,
                        time: 0,
                        dwExtraInfo: INJECTED_MAGIC_TAG,
                    },
                },
            });
        }
        if !inputs.is_empty() {
            unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        }
    }

    fn inject_key(&self, hid_usage: u16, pressed: bool) {
        {
            let mut guard = self.held_keys.lock().unwrap();
            if pressed {
                guard.insert(hid_usage);
            } else {
                guard.remove(&hid_usage);
            }
        }
        if let Some((scan_code, extended)) = hid_to_win_scancode(hid_usage) {
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: Default::default(),
                        wScan: scan_code,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: INJECTED_MAGIC_TAG,
                    },
                },
            };
            unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        }
    }

    fn release_all_held(&self) {
        let keys: Vec<u16> = self.held_keys.lock().unwrap().drain().collect();
        for k in keys {
            self.inject_key(k, false);
        }
        let buttons: Vec<MouseButton> = self.held_buttons.lock().unwrap().drain().collect();
        for b in buttons {
            self.inject_mouse_button(b, false);
        }
    }

    fn cursor_pos(&self) -> Option<(i32, i32)> {
        get_cursor_pos()
    }
}

/// Phase 1 core PS/2 Set 1 Scancode -> USB HID Usage ID (Page 0x07)
pub fn win_scancode_to_hid(scan: u16, extended: bool) -> Option<u16> {
    match (scan, extended) {
        (0x01, false) => Some(0x29), // Esc
        (0x1E, false) => Some(0x04), // A
        (0x30, false) => Some(0x05), // B
        (0x2E, false) => Some(0x06), // C
        (0x20, false) => Some(0x07), // D
        (0x12, false) => Some(0x08), // E
        (0x21, false) => Some(0x09), // F
        (0x22, false) => Some(0x0A), // G
        (0x23, false) => Some(0x0B), // H
        (0x17, false) => Some(0x0C), // I
        (0x24, false) => Some(0x0D), // J
        (0x25, false) => Some(0x0E), // K
        (0x26, false) => Some(0x0F), // L
        (0x32, false) => Some(0x10), // M
        (0x31, false) => Some(0x11), // N
        (0x18, false) => Some(0x12), // O
        (0x19, false) => Some(0x13), // P
        (0x10, false) => Some(0x14), // Q
        (0x13, false) => Some(0x15), // R
        (0x1F, false) => Some(0x16), // S
        (0x14, false) => Some(0x17), // T
        (0x16, false) => Some(0x18), // U
        (0x2F, false) => Some(0x19), // V
        (0x11, false) => Some(0x1A), // W
        (0x2D, false) => Some(0x1B), // X
        (0x15, false) => Some(0x1C), // Y
        (0x2C, false) => Some(0x1D), // Z
        (0x02..=0x0A, false) => Some(0x1E + (scan - 0x02)), // 1..9
        (0x0B, false) => Some(0x27), // 0
        (0x1C, false) => Some(0x28), // Enter
        (0x0E, false) => Some(0x2A), // Backspace
        (0x0F, false) => Some(0x2B), // Tab
        (0x39, false) => Some(0x2C), // Space
        (0x1D, false) => Some(0xE0), // L-Ctrl
        (0x2A, false) => Some(0xE1), // L-Shift
        (0x38, false) => Some(0xE2), // L-Alt
        (0x5B, true) => Some(0xE3),  // L-Win
        (0x1D, true) => Some(0xE4),  // R-Ctrl
        (0x36, false) => Some(0xE5), // R-Shift
        (0x38, true) => Some(0xE6),  // R-Alt
        (0x5C, true) => Some(0xE7),  // R-Win
        (0x48, true) => Some(0x52),  // Up
        (0x50, true) => Some(0x51),  // Down
        (0x4B, true) => Some(0x50),  // Left
        (0x4D, true) => Some(0x4F),  // Right
        (0x3A, false) => Some(0x39), // CapsLock
        (0x3B..=0x44, false) => Some(0x3A + (scan - 0x3B)), // F1..F10
        (0x57, false) => Some(0x44), // F11
        (0x58, false) => Some(0x45), // F12
        (0x52, true) => Some(0x49),  // Insert
        (0x47, true) => Some(0x4A),  // Home
        (0x49, true) => Some(0x4B),  // PageUp
        (0x53, true) => Some(0x4C),  // Delete
        (0x4F, true) => Some(0x4D),  // End
        (0x51, true) => Some(0x4E),  // PageDown
        // Punctuation & special characters
        (0x0C, false) => Some(0x2D), // - _
        (0x0D, false) => Some(0x2E), // = +
        (0x1A, false) => Some(0x2F), // [ {
        (0x1B, false) => Some(0x30), // ] }
        (0x2B, false) => Some(0x31), // \ |
        (0x27, false) => Some(0x33), // ; :
        (0x28, false) => Some(0x34), // ' "
        (0x29, false) => Some(0x35), // ` ~
        (0x33, false) => Some(0x36), // , <
        (0x34, false) => Some(0x37), // . >
        (0x35, false) => Some(0x38), // / ?
        // Numpad
        (0x45, true) | (0x45, false) => Some(0x53), // NumLock
        (0x35, true) => Some(0x54),  // KP /
        (0x37, false) => Some(0x55), // KP *
        (0x4A, false) => Some(0x56), // KP -
        (0x4E, false) => Some(0x57), // KP +
        (0x1C, true) => Some(0x58),  // KP Enter
        (0x4F, false) => Some(0x59), // KP 1
        (0x50, false) => Some(0x5A), // KP 2
        (0x51, false) => Some(0x5B), // KP 3
        (0x4B, false) => Some(0x5C), // KP 4
        (0x4C, false) => Some(0x5D), // KP 5
        (0x4D, false) => Some(0x5E), // KP 6
        (0x47, false) => Some(0x5F), // KP 7
        (0x48, false) => Some(0x60), // KP 8
        (0x49, false) => Some(0x61), // KP 9
        (0x52, false) => Some(0x62), // KP 0
        (0x53, false) => Some(0x63), // KP .
        _ => None,
    }
}

pub fn hid_to_win_scancode(hid: u16) -> Option<(u16, bool)> {
    match hid {
        0x29 => Some((0x01, false)),
        0x04 => Some((0x1E, false)),
        0x05 => Some((0x30, false)),
        0x06 => Some((0x2E, false)),
        0x07 => Some((0x20, false)),
        0x08 => Some((0x12, false)),
        0x09 => Some((0x21, false)),
        0x0A => Some((0x22, false)),
        0x0B => Some((0x23, false)),
        0x0C => Some((0x17, false)),
        0x0D => Some((0x24, false)),
        0x0E => Some((0x25, false)),
        0x0F => Some((0x26, false)),
        0x10 => Some((0x32, false)),
        0x11 => Some((0x31, false)),
        0x12 => Some((0x18, false)),
        0x13 => Some((0x19, false)),
        0x14 => Some((0x10, false)),
        0x15 => Some((0x13, false)),
        0x16 => Some((0x1F, false)),
        0x17 => Some((0x14, false)),
        0x18 => Some((0x16, false)),
        0x19 => Some((0x2F, false)),
        0x1A => Some((0x11, false)),
        0x1B => Some((0x2D, false)),
        0x1C => Some((0x15, false)),
        0x1D => Some((0x2C, false)),
        0x1E..=0x26 => Some((0x02 + (hid - 0x1E), false)),
        0x27 => Some((0x0B, false)),
        0x28 => Some((0x1C, false)),
        0x2A => Some((0x0E, false)),
        0x2B => Some((0x0F, false)),
        0x2C => Some((0x39, false)),
        0xE0 => Some((0x1D, false)),
        0xE1 => Some((0x2A, false)),
        0xE2 => Some((0x38, false)),
        0xE3 => Some((0x5B, true)),
        0xE4 => Some((0x1D, true)),
        0xE5 => Some((0x36, false)),
        0xE6 => Some((0x38, true)),
        0xE7 => Some((0x5C, true)),
        0x52 => Some((0x48, true)),
        0x51 => Some((0x50, true)),
        0x50 => Some((0x4B, true)),
        0x4F => Some((0x4D, true)),
        0x39 => Some((0x3A, false)),
        0x3A..=0x43 => Some((0x3B + (hid - 0x3A), false)),
        0x44 => Some((0x57, false)),
        0x45 => Some((0x58, false)),
        0x49 => Some((0x52, true)),
        0x4A => Some((0x47, true)),
        0x4B => Some((0x49, true)),
        0x4C => Some((0x53, true)),
        0x4D => Some((0x4F, true)),
        0x4E => Some((0x51, true)),
        // Punctuation & special characters
        0x2D => Some((0x0C, false)),
        0x2E => Some((0x0D, false)),
        0x2F => Some((0x1A, false)),
        0x30 => Some((0x1B, false)),
        0x31 => Some((0x2B, false)),
        0x33 => Some((0x27, false)),
        0x34 => Some((0x28, false)),
        0x35 => Some((0x29, false)),
        0x36 => Some((0x33, false)),
        0x37 => Some((0x34, false)),
        0x38 => Some((0x35, false)),
        // Numpad
        0x53 => Some((0x45, true)),
        0x54 => Some((0x35, true)),
        0x55 => Some((0x37, false)),
        0x56 => Some((0x4A, false)),
        0x57 => Some((0x4E, false)),
        0x58 => Some((0x1C, true)),
        0x59 => Some((0x4F, false)),
        0x5A => Some((0x50, false)),
        0x5B => Some((0x51, false)),
        0x5C => Some((0x4B, false)),
        0x5D => Some((0x4C, false)),
        0x5E => Some((0x4D, false)),
        0x5F => Some((0x47, false)),
        0x60 => Some((0x48, false)),
        0x61 => Some((0x49, false)),
        0x62 => Some((0x52, false)),
        0x63 => Some((0x53, false)),
        _ => None,
    }
}

/// Moves the Windows cursor once to `(x, y)` (away from the screen edge) without locking it
/// via ClipCursor, so WH_MOUSE_LL can read unclamped relative deltas while suppressing local UI.
pub fn park_cursor_at(x: i32, y: i32) {
    unsafe {
        let _ = SetCursorPos(x, y);
    }
}

/// No-op since we do not lock the cursor with ClipCursor.
pub fn unpark_cursor() {}

const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;

pub struct WinClipboardProvider;

impl WinClipboardProvider {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    /// Runs a blocking Win32 message-only window (`HWND_MESSAGE`) listening for `WM_CLIPBOARDUPDATE`.
    /// Emits `ClipboardContent::Files` (if CF_HDROP) or `ClipboardContent::Text`.
    pub fn run_listener<F: Fn(ClipboardContent) + Send + 'static>(
        on_change: F,
    ) -> Result<(), HalError> {
        thread_local! {
            static CB_HANDLER: RefCell<Option<Box<dyn Fn(ClipboardContent)>>> = const { RefCell::new(None) };
        }
        CB_HANDLER.with(|c| *c.borrow_mut() = Some(Box::new(on_change)));

        unsafe extern "system" fn wnd_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if msg == WM_CLIPBOARDUPDATE {
                let content = if IsClipboardFormatAvailable(CF_HDROP).is_ok() {
                    WinClipboardProvider
                        .get_files()
                        .map(ClipboardContent::Files)
                } else {
                    WinClipboardProvider.get_text().map(ClipboardContent::Text)
                };
                if let Some(item) = content {
                    CB_HANDLER.with(|c| {
                        if let Some(cb) = c.borrow().as_ref() {
                            cb(item);
                        }
                    });
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        unsafe {
            let class_name = w!("P2PKMClipboardListener");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                Default::default(),
                class_name,
                w!(""),
                Default::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                None,
            )
            .map_err(|e| HalError::HookInstall(e.to_string()))?;

            AddClipboardFormatListener(hwnd)
                .map_err(|e| HalError::HookInstall(e.to_string()))?;

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }
}

impl ClipboardProvider for WinClipboardProvider {
    fn get_text(&self) -> Option<String> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }
            let result = (|| {
                let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
                let ptr = GlobalLock(HGLOBAL(handle.0)) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr, len);
                let s = String::from_utf16(slice).ok();
                let _ = GlobalUnlock(HGLOBAL(handle.0));
                s
            })();
            let _ = CloseClipboard();
            result
        }
    }

    fn set_text(&self, text: &str) {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes_len = wide.len() * std::mem::size_of::<u16>();
        unsafe {
            if OpenClipboard(None).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            if let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes_len) {
                let ptr = GlobalLock(hmem) as *mut u16;
                if !ptr.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                    let _ = GlobalUnlock(hmem);
                    let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0));
                }
            }
            let _ = CloseClipboard();
        }
    }

    fn get_files(&self) -> Option<Vec<PathBuf>> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }
            let result = (|| {
                let handle = GetClipboardData(CF_HDROP).ok()?;
                let hdrop = HDROP(handle.0);
                let count = DragQueryFileW(hdrop, u32::MAX, None);
                if count == 0 {
                    return None;
                }
                let mut paths = Vec::with_capacity(count as usize);
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, None);
                    if len > 0 {
                        let mut buf = vec![0u16; (len + 1) as usize];
                        let copied = DragQueryFileW(hdrop, i, Some(&mut buf));
                        if copied > 0 {
                            let s = String::from_utf16_lossy(&buf[..copied as usize]);
                            paths.push(PathBuf::from(s));
                        }
                    }
                }
                if paths.is_empty() {
                    None
                } else {
                    Some(paths)
                }
            })();
            let _ = CloseClipboard();
            result
        }
    }

    fn set_files(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let mut wide_paths: Vec<u16> = Vec::new();
        for p in paths {
            wide_paths.extend(p.to_string_lossy().encode_utf16());
            wide_paths.push(0);
        }
        wide_paths.push(0); // double null terminator

        let header_size = std::mem::size_of::<DROPFILES>();
        let total_bytes = header_size + wide_paths.len() * std::mem::size_of::<u16>();

        unsafe {
            if OpenClipboard(None).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            if let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, total_bytes) {
                let ptr = GlobalLock(hmem) as *mut u8;
                if !ptr.is_null() {
                    let df = DROPFILES {
                        pFiles: header_size as u32,
                        pt: POINT { x: 0, y: 0 },
                        fNC: BOOL(0),
                        fWide: BOOL(1),
                    };
                    std::ptr::write(ptr as *mut DROPFILES, df);
                    std::ptr::copy_nonoverlapping(
                        wide_paths.as_ptr() as *const u8,
                        ptr.add(header_size),
                        wide_paths.len() * std::mem::size_of::<u16>(),
                    );
                    let _ = GlobalUnlock(hmem);
                    let _ = SetClipboardData(CF_HDROP, HANDLE(hmem.0));
                }
            }
            let _ = CloseClipboard();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_enumerate_displays_works() {
        let displays = enumerate_displays().expect("should enumerate displays on Windows");
        assert!(!displays.is_empty(), "at least one display must exist");
        assert!(displays.iter().any(|d| d.is_primary));
        for d in &displays {
            assert!(d.width > 0 && d.height > 0);
            assert!(d.scale_factor >= 1.0);
        }
    }

    #[test]
    fn scancode_hid_roundtrip() {
        for hid in [
            0x04u16, 0x1E, 0x27, 0x28, 0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x33, 0x34, 0x35, 0x36,
            0x37, 0x38, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5E, 0x62, 0x63, 0xE0, 0xE3,
            0xE4, 0xE5, 0xE6, 0xE7,
        ] {
            let (scan, ext) = hid_to_win_scancode(hid).unwrap();
            assert_eq!(win_scancode_to_hid(scan, ext), Some(hid));
        }
    }
}
