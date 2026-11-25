use crate::{
    ClipboardContent, ClipboardProvider, DisplayInfo, HalError, HookControl, InputInjector,
    RawInputEvent, INJECTED_MAGIC_TAG,
};
use core_foundation::base::{CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::mach_port::{CFMachPort, CFMachPortRef};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::{CGDisplay, CGGetActiveDisplayList, CGPoint};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use p2pkm_proto::MouseButton;
use rtrb::Producer;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

type RawCGEventRef = *mut std::ffi::c_void;
type RawCGEventTapProxy = *mut std::ffi::c_void;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSMainConnectionID() -> i32;
    fn CGSSetConnectionProperty(
        cid: i32,
        target_cid: i32,
        key: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: extern "C" fn(
            RawCGEventTapProxy,
            u32,
            RawCGEventRef,
            *mut std::ffi::c_void,
        ) -> RawCGEventRef,
        user_info: *mut std::ffi::c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: RawCGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: RawCGEventRef) -> u64;
    fn CGEventGetLocation(event: RawCGEventRef) -> CGPoint;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFPreferencesCurrentApplication: CFStringRef;
    fn CFPreferencesSetAppValue(key: CFStringRef, value: CFTypeRef, application_id: CFStringRef);
    fn CFPreferencesAppSynchronize(application_id: CFStringRef) -> bool;
}

/// Allows macOS WindowServer to hide/park the cursor even when `p2pkm-ui` is not the frontmost app
/// (e.g. when Chrome or another app is focused) and disables macOS App Nap so background event
/// tap & watchdogs are never throttled.
fn enable_background_cursor_control() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let cid = CGSMainConnectionID();
        let key = CFString::new("SetsCursorInBackground");
        let val = CFBoolean::true_value();
        let _ = CGSSetConnectionProperty(
            cid,
            cid,
            key.as_concrete_TypeRef(),
            val.as_CFTypeRef(),
        );

        let sleep_key = CFString::new("NSAppSleepDisabled");
        CFPreferencesSetAppValue(
            sleep_key.as_concrete_TypeRef(),
            val.as_CFTypeRef(),
            kCFPreferencesCurrentApplication,
        );
        let _ = CFPreferencesAppSynchronize(kCFPreferencesCurrentApplication);
    });
}

pub fn is_accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

pub fn enumerate_displays() -> Result<Vec<DisplayInfo>, HalError> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    let err = unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut count) };
    if err != 0 {
        return Err(HalError::DisplayEnum(format!("CGError {err}")));
    }
    let main_id = CGDisplay::main().id;
    let mut out = Vec::with_capacity(count as usize);
    for &id in &ids[..count as usize] {
        let disp = CGDisplay::new(id);
        let bounds = disp.bounds();
        out.push(DisplayInfo {
            id,
            is_primary: id == main_id,
            x: bounds.origin.x as i32,
            y: bounds.origin.y as i32,
            width: bounds.size.width as u32,
            height: bounds.size.height as u32,
            scale_factor: 1.0, // Logical points on macOS
        });
    }
    Ok(out)
}

struct TapContext {
    ctrl: Arc<HookControl>,
    prod: Mutex<Producer<RawInputEvent>>,
}

extern "C" fn event_tap_callback(
    _proxy: RawCGEventTapProxy,
    event_type: u32,
    event: RawCGEventRef,
    user_info: *mut std::ffi::c_void,
) -> RawCGEventRef {
    if user_info.is_null() || event.is_null() {
        return event;
    }
    let ctx = unsafe { &*(user_info as *const TapContext) };

    // kCGEventTapDisabledByTimeout (0xFFFFFFFE) | kCGEventTapDisabledByUserInput (0xFFFFFFFF)
    if event_type == 0xFFFF_FFFE || event_type == 0xFFFF_FFFF {
        tracing::warn!(event_type, "macOS CGEventTap disabled -> triggering panic reset & unpark");
        ctx.ctrl.trigger_panic();
        unpark_cursor();
        if let Ok(mut p) = ctx.prod.try_lock() {
            let _ = p.push(RawInputEvent::PanicHotkey);
        }
        return event;
    }

    let tag = unsafe { CGEventGetIntegerValueField(event, EventField::EVENT_SOURCE_USER_DATA) };
    if tag == INJECTED_MAGIC_TAG as i64 {
        return event;
    }

    // Emergency Panic Hotkey: Cmd+Opt+Ctrl+Esc (kCGEventKeyDown = 10, kVK_Escape = 0x35)
    if event_type == 10 {
        let keycode =
            unsafe { CGEventGetIntegerValueField(event, EventField::KEYBOARD_EVENT_KEYCODE) };
        let flags = CGEventFlags::from_bits_truncate(unsafe { CGEventGetFlags(event) });
        let panic_mods = CGEventFlags::CGEventFlagCommand
            | CGEventFlags::CGEventFlagAlternate
            | CGEventFlags::CGEventFlagControl;
        if keycode == 0x35 && flags.contains(panic_mods) {
            tracing::warn!("macOS emergency panic hotkey pressed -> unparking cursor");
            ctx.ctrl.trigger_panic();
            unpark_cursor();
            if let Ok(mut p) = ctx.prod.try_lock() {
                let _ = p.push(RawInputEvent::PanicHotkey);
            }
            return event;
        }
    }

    let mut pass_through_modifier_up = false;
    let suppressed = ctx.ctrl.is_suppressed();
    if let Ok(mut p) = ctx.prod.try_lock() {
        match event_type {
            // MouseMoved (5), LeftMouseDragged (6), RightMouseDragged (7), OtherMouseDragged (27)
            5 | 6 | 7 | 27 => {
                let dx =
                    unsafe { CGEventGetIntegerValueField(event, EventField::MOUSE_EVENT_DELTA_X) }
                        as i32;
                let dy =
                    unsafe { CGEventGetIntegerValueField(event, EventField::MOUSE_EVENT_DELTA_Y) }
                        as i32;
                let loc = unsafe { CGEventGetLocation(event) };
                let _ = p.push(RawInputEvent::MouseMove {
                    dx,
                    dy,
                    abs_x: loc.x as i32,
                    abs_y: loc.y as i32,
                });
            }
            // LeftMouseDown (1), LeftMouseUp (2)
            1 | 2 => {
                let _ = p.push(RawInputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: event_type == 1,
                });
            }
            // RightMouseDown (3), RightMouseUp (4)
            3 | 4 => {
                let _ = p.push(RawInputEvent::MouseButton {
                    button: MouseButton::Right,
                    pressed: event_type == 3,
                });
            }
            // OtherMouseDown (25), OtherMouseUp (26)
            25 | 26 => {
                let _ = p.push(RawInputEvent::MouseButton {
                    button: MouseButton::Middle,
                    pressed: event_type == 25,
                });
            }
            // ScrollWheel (22)
            22 if suppressed => {
                let dy = unsafe {
                    CGEventGetIntegerValueField(event, EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1)
                } as i32;
                let dx = unsafe {
                    CGEventGetIntegerValueField(event, EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2)
                } as i32;
                let _ = p.push(RawInputEvent::MouseScroll { dx, dy });
            }
            // KeyDown (10), KeyUp (11)
            10 | 11 if suppressed => {
                let kc = unsafe {
                    CGEventGetIntegerValueField(event, EventField::KEYBOARD_EVENT_KEYCODE)
                } as u16;
                if let Some(hid) = mac_keycode_to_hid(kc) {
                    let _ = p.push(RawInputEvent::Key {
                        hid_usage: hid,
                        pressed: event_type == 10,
                    });
                }
            }
            // FlagsChanged (12) — Shift, Control, Option, Command, CapsLock
            12 if suppressed => {
                let kc = unsafe {
                    CGEventGetIntegerValueField(event, EventField::KEYBOARD_EVENT_KEYCODE)
                } as u16;
                let flags = CGEventFlags::from_bits_truncate(unsafe { CGEventGetFlags(event) });
                let pressed_opt = match kc {
                    0x38 | 0x3C => Some(flags.contains(CGEventFlags::CGEventFlagShift)),
                    0x3B | 0x3E => Some(flags.contains(CGEventFlags::CGEventFlagControl)),
                    0x3A | 0x3D => Some(flags.contains(CGEventFlags::CGEventFlagAlternate)),
                    0x37 | 0x36 => Some(flags.contains(CGEventFlags::CGEventFlagCommand)),
                    0x39 => Some(flags.contains(CGEventFlags::CGEventFlagAlphaShift)),
                    _ => None,
                };
                if let (Some(hid), Some(pressed)) = (mac_keycode_to_hid(kc), pressed_opt) {
                    let _ = p.push(RawInputEvent::Key {
                        hid_usage: hid,
                        pressed,
                    });
                    if !pressed {
                        pass_through_modifier_up = true;
                    }
                }
            }
            _ => {}
        }
    }

    // Returning NULL at the C ABI level guarantees CoreGraphics swallows the event
    // for all apps (working around core-graphics 0.24's Option::None passthrough bug).
    // Never swallow modifier releases so Option/Cmd/Ctrl/Shift never get stuck locally.
    if suppressed && !pass_through_modifier_up {
        std::ptr::null_mut()
    } else {
        event
    }
}

pub fn run_hook_loop(ctrl: Arc<HookControl>, prod: Producer<RawInputEvent>) -> Result<(), HalError> {
    enable_background_cursor_control();
    let mask: u64 = (1 << 1) // LeftMouseDown
        | (1 << 2)           // LeftMouseUp
        | (1 << 3)           // RightMouseDown
        | (1 << 4)           // RightMouseUp
        | (1 << 5)           // MouseMoved
        | (1 << 6)           // LeftMouseDragged
        | (1 << 7)           // RightMouseDragged
        | (1 << 10)          // KeyDown
        | (1 << 11)          // KeyUp
        | (1 << 12)          // FlagsChanged
        | (1 << 22)          // ScrollWheel
        | (1 << 25)          // OtherMouseDown
        | (1 << 26)          // OtherMouseUp
        | (1 << 27);         // OtherMouseDragged

    let ctx_ptr = Box::into_raw(Box::new(TapContext {
        ctrl,
        prod: Mutex::new(prod),
    }));

    // kCGSessionEventTap = 1, kCGHeadInsertEventTap = 0, kCGEventTapOptionDefault = 0
    let tap_ref = unsafe {
        CGEventTapCreate(
            1,
            0,
            0,
            mask,
            event_tap_callback,
            ctx_ptr as *mut std::ffi::c_void,
        )
    };
    if tap_ref.is_null() {
        unsafe {
            drop(Box::from_raw(ctx_ptr));
        }
        return Err(HalError::PermissionDenied);
    }

    unsafe {
        let mach_port = CFMachPort::wrap_under_create_rule(tap_ref);
        let loop_source = mach_port
            .create_runloop_source(0)
            .map_err(|_| HalError::HookInstall("create_runloop_source failed".into()))?;
        CFRunLoop::get_current().add_source(&loop_source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap_ref, true);
        CFRunLoop::run_current();
        drop(Box::from_raw(ctx_ptr));
    }
    Ok(())
}

pub struct MacInputInjector {
    held_keys: Mutex<HashSet<u16>>,
    held_buttons: Mutex<HashSet<MouseButton>>,
    active_flags: Mutex<CGEventFlags>,
    cursor_pos: Mutex<CGPoint>,
    last_click: Mutex<(std::time::Instant, CGPoint, i64, MouseButton)>,
}

impl MacInputInjector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            held_keys: Mutex::new(HashSet::new()),
            held_buttons: Mutex::new(HashSet::new()),
            active_flags: Mutex::new(CGEventFlags::empty()),
            cursor_pos: Mutex::new(CGPoint::new(500.0, 500.0)),
            last_click: Mutex::new((
                std::time::Instant::now() - std::time::Duration::from_secs(5),
                CGPoint::new(0.0, 0.0),
                1,
                MouseButton::Left,
            )),
        })
    }

    fn source() -> CGEventSource {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap()
    }
}

fn clamp_to_displays(x: f64, y: f64) -> CGPoint {
    let displays = enumerate_displays().unwrap_or_default();
    if displays.is_empty() {
        let bounds = CGDisplay::main().bounds();
        let max_x = (bounds.origin.x + bounds.size.width - 1.0).max(bounds.origin.x);
        let max_y = (bounds.origin.y + bounds.size.height - 1.0).max(bounds.origin.y);
        return CGPoint::new(x.clamp(bounds.origin.x, max_x), y.clamp(bounds.origin.y, max_y));
    }
    let mut best = CGPoint::new(x, y);
    let mut best_dist = f64::MAX;
    for d in &displays {
        let min_x = d.x as f64;
        let min_y = d.y as f64;
        let max_x = (d.x + d.width as i32 - 1) as f64;
        let max_y = (d.y + d.height as i32 - 1) as f64;
        let cx = x.clamp(min_x, max_x.max(min_x));
        let cy = y.clamp(min_y, max_y.max(min_y));
        if (cx - x).abs() < 0.5 && (cy - y).abs() < 0.5 {
            return CGPoint::new(x, y);
        }
        let dist = (cx - x).powi(2) + (cy - y).powi(2);
        if dist < best_dist {
            best_dist = dist;
            best = CGPoint::new(cx, cy);
        }
    }
    best
}

impl InputInjector for MacInputInjector {
    fn inject_mouse_move(&self, dx: i32, dy: i32) {
        let pos = {
            let mut p = self.cursor_pos.lock().unwrap();
            let clamped = clamp_to_displays(p.x + dx as f64, p.y + dy as f64);
            *p = clamped;
            clamped
        };
        let (ev_type, cg_btn) = {
            let held = self.held_buttons.lock().unwrap();
            if held.contains(&MouseButton::Left) {
                (CGEventType::LeftMouseDragged, CGMouseButton::Left)
            } else if held.contains(&MouseButton::Right) {
                (CGEventType::RightMouseDragged, CGMouseButton::Right)
            } else if !held.is_empty() {
                (CGEventType::OtherMouseDragged, CGMouseButton::Center)
            } else {
                (CGEventType::MouseMoved, CGMouseButton::Left)
            }
        };
        if let Ok(ev) = CGEvent::new_mouse_event(Self::source(), ev_type, pos, cg_btn) {
            ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
            ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
            ev.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECTED_MAGIC_TAG as i64);
            ev.set_flags(*self.active_flags.lock().unwrap());
            ev.post(CGEventTapLocation::HID);
        }
    }

    fn inject_mouse_abs(&self, x: i32, y: i32) {
        let clamped = clamp_to_displays(x as f64, y as f64);
        {
            let mut pos = self.cursor_pos.lock().unwrap();
            *pos = clamped;
        }
        if let Ok(ev) = CGEvent::new_mouse_event(
            Self::source(),
            CGEventType::MouseMoved,
            clamped,
            CGMouseButton::Left,
        ) {
            ev.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECTED_MAGIC_TAG as i64);
            ev.post(CGEventTapLocation::HID);
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
        let pos = *self.cursor_pos.lock().unwrap();
        let click_state = {
            let mut lc = self.last_click.lock().unwrap();
            let now = std::time::Instant::now();
            if pressed {
                let dist = ((pos.x - lc.1.x).powi(2) + (pos.y - lc.1.y).powi(2)).sqrt();
                if lc.3 == button
                    && now.duration_since(lc.0) <= std::time::Duration::from_millis(500)
                    && dist <= 6.0
                {
                    lc.2 = (lc.2 % 3) + 1;
                } else {
                    lc.2 = 1;
                }
                lc.0 = now;
                lc.1 = pos;
                lc.3 = button;
            }
            lc.2
        };
        let (ev_type, cg_btn) = match (button, pressed) {
            (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            (_, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            (_, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        };
        if let Ok(ev) = CGEvent::new_mouse_event(Self::source(), ev_type, pos, cg_btn) {
            ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
            ev.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECTED_MAGIC_TAG as i64);
            ev.set_flags(*self.active_flags.lock().unwrap());
            ev.post(CGEventTapLocation::HID);
        }
    }

    fn inject_mouse_scroll(&self, dx: i32, dy: i32) {
        if let Ok(ev) = CGEvent::new(Self::source()) {
            ev.set_type(CGEventType::ScrollWheel);
            ev.set_integer_value_field(
                EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
                (dy / 12) as i64,
            );
            ev.set_integer_value_field(
                EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
                (dx / 12) as i64,
            );
            ev.set_integer_value_field(
                EventField::EVENT_SOURCE_USER_DATA,
                INJECTED_MAGIC_TAG as i64,
            );
            ev.post(CGEventTapLocation::HID);
        }
    }

    fn inject_key(&self, hid_usage: u16, pressed: bool) {
        let computed_flags = {
            let mut guard = self.held_keys.lock().unwrap();
            if pressed {
                guard.insert(hid_usage);
            } else {
                guard.remove(&hid_usage);
            }
            let mut f = CGEventFlags::empty();
            if guard.contains(&0xE0) || guard.contains(&0xE4) {
                f.insert(CGEventFlags::CGEventFlagControl);
            }
            if guard.contains(&0xE1) || guard.contains(&0xE5) {
                f.insert(CGEventFlags::CGEventFlagShift);
            }
            if guard.contains(&0xE2) || guard.contains(&0xE6) {
                f.insert(CGEventFlags::CGEventFlagAlternate);
            }
            if guard.contains(&0xE3) || guard.contains(&0xE7) {
                f.insert(CGEventFlags::CGEventFlagCommand);
            }
            f
        };
        {
            let mut flags = self.active_flags.lock().unwrap();
            *flags = computed_flags;
        }
        if let Some(kc) = hid_to_mac_keycode(hid_usage) {
            if let Ok(ev) = CGEvent::new_keyboard_event(Self::source(), kc, pressed) {
                ev.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECTED_MAGIC_TAG as i64);
                ev.set_flags(computed_flags);
                ev.post(CGEventTapLocation::HID);
            }
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
        *self.active_flags.lock().unwrap() = CGEventFlags::empty();
    }

    fn cursor_pos(&self) -> Option<(i32, i32)> {
        let p = self.cursor_pos.lock().unwrap();
        Some((p.x as i32, p.y as i32))
    }
}

pub fn mac_keycode_to_hid(kc: u16) -> Option<u16> {
    match kc {
        0x35 => Some(0x29), // Esc
        0x00 => Some(0x04), // A
        0x0B => Some(0x05), // B
        0x08 => Some(0x06), // C
        0x02 => Some(0x07), // D
        0x0E => Some(0x08), // E
        0x03 => Some(0x09), // F
        0x05 => Some(0x0A), // G
        0x04 => Some(0x0B), // H
        0x22 => Some(0x0C), // I
        0x26 => Some(0x0D), // J
        0x28 => Some(0x0E), // K
        0x25 => Some(0x0F), // L
        0x2E => Some(0x10), // M
        0x2D => Some(0x11), // N
        0x1F => Some(0x12), // O
        0x23 => Some(0x13), // P
        0x0C => Some(0x14), // Q
        0x0F => Some(0x15), // R
        0x01 => Some(0x16), // S
        0x11 => Some(0x17), // T
        0x20 => Some(0x18), // U
        0x09 => Some(0x19), // V
        0x0D => Some(0x1A), // W
        0x07 => Some(0x1B), // X
        0x10 => Some(0x1C), // Y
        0x06 => Some(0x1D), // Z
        0x24 => Some(0x28), // Return
        0x33 => Some(0x2A), // Delete/Backspace
        0x30 => Some(0x2B), // Tab
        0x31 => Some(0x2C), // Space
        0x3B => Some(0xE0), // L-Control
        0x38 => Some(0xE1), // L-Shift
        0x3A => Some(0xE2), // L-Option
        0x37 => Some(0xE3), // L-Command
        0x7E => Some(0x52), // Up
        0x7D => Some(0x51), // Down
        0x7B => Some(0x50), // Left
        0x7C => Some(0x4F), // Right
        0x39 => Some(0x39), // CapsLock
        0x73 => Some(0x4A), // Home
        0x74 => Some(0x4B), // PageUp
        0x75 => Some(0x4C), // ForwardDelete
        0x77 => Some(0x4D), // End
        0x79 => Some(0x4E), // PageDown
        0x7A => Some(0x3A), // F1
        0x78 => Some(0x3B), // F2
        0x63 => Some(0x3C), // F3
        0x76 => Some(0x3D), // F4
        0x60 => Some(0x3E), // F5
        0x61 => Some(0x3F), // F6
        0x62 => Some(0x40), // F7
        0x64 => Some(0x41), // F8
        0x65 => Some(0x42), // F9
        0x6D => Some(0x43), // F10
        0x67 => Some(0x44), // F11
        0x6F => Some(0x45), // F12
        // Number row 1..9, 0
        0x12 => Some(0x1E), // 1
        0x13 => Some(0x1F), // 2
        0x14 => Some(0x20), // 3
        0x15 => Some(0x21), // 4
        0x17 => Some(0x22), // 5
        0x16 => Some(0x23), // 6
        0x1A => Some(0x24), // 7
        0x1C => Some(0x25), // 8
        0x19 => Some(0x26), // 9
        0x1D => Some(0x27), // 0
        // Punctuation & special characters
        0x1B => Some(0x2D), // - _
        0x18 => Some(0x2E), // = +
        0x21 => Some(0x2F), // [ {
        0x1E => Some(0x30), // ] }
        0x2A => Some(0x31), // \ |
        0x29 => Some(0x33), // ; :
        0x27 => Some(0x34), // ' "
        0x32 => Some(0x35), // ` ~
        0x2B => Some(0x36), // , <
        0x2F => Some(0x37), // . >
        0x2C => Some(0x38), // / ?
        // Right modifiers
        0x3E => Some(0xE4), // R-Control
        0x3C => Some(0xE5), // R-Shift
        0x3D => Some(0xE6), // R-Option
        0x36 => Some(0xE7), // R-Command
        // Numpad
        0x47 => Some(0x53), // Clear / NumLock
        0x4B => Some(0x54), // KP /
        0x43 => Some(0x55), // KP *
        0x4E => Some(0x56), // KP -
        0x45 => Some(0x57), // KP +
        0x4C => Some(0x58), // KP Enter
        0x53 => Some(0x59), // KP 1
        0x54 => Some(0x5A), // KP 2
        0x55 => Some(0x5B), // KP 3
        0x56 => Some(0x5C), // KP 4
        0x57 => Some(0x5D), // KP 5
        0x58 => Some(0x5E), // KP 6
        0x59 => Some(0x5F), // KP 7
        0x5B => Some(0x60), // KP 8
        0x5C => Some(0x61), // KP 9
        0x52 => Some(0x62), // KP 0
        0x41 => Some(0x63), // KP .
        _ => None,
    }
}

pub fn hid_to_mac_keycode(hid: u16) -> Option<u16> {
    match hid {
        0x29 => Some(0x35),
        0x04 => Some(0x00),
        0x05 => Some(0x0B),
        0x06 => Some(0x08),
        0x07 => Some(0x02),
        0x08 => Some(0x0E),
        0x09 => Some(0x03),
        0x0A => Some(0x05),
        0x0B => Some(0x04),
        0x0C => Some(0x22),
        0x0D => Some(0x26),
        0x0E => Some(0x28),
        0x0F => Some(0x25),
        0x10 => Some(0x2E),
        0x11 => Some(0x2D),
        0x12 => Some(0x1F),
        0x13 => Some(0x23),
        0x14 => Some(0x0C),
        0x15 => Some(0x0F),
        0x16 => Some(0x01),
        0x17 => Some(0x11),
        0x18 => Some(0x20),
        0x19 => Some(0x09),
        0x1A => Some(0x0D),
        0x1B => Some(0x07),
        0x1C => Some(0x10),
        0x1D => Some(0x06),
        0x28 => Some(0x24),
        0x2A => Some(0x33),
        0x2B => Some(0x30),
        0x2C => Some(0x31),
        0xE0 => Some(0x3B),
        0xE1 => Some(0x38),
        0xE2 => Some(0x3A),
        0xE3 => Some(0x37),
        0x52 => Some(0x7E),
        0x51 => Some(0x7D),
        0x50 => Some(0x7B),
        0x4F => Some(0x7C),
        0x39 => Some(0x39),
        0x4A => Some(0x73),
        0x4B => Some(0x74),
        0x4C => Some(0x75),
        0x4D => Some(0x77),
        0x4E => Some(0x79),
        0x3A => Some(0x7A),
        0x3B => Some(0x78),
        0x3C => Some(0x63),
        0x3D => Some(0x76),
        0x3E => Some(0x60),
        0x3F => Some(0x61),
        0x40 => Some(0x62),
        0x41 => Some(0x64),
        0x42 => Some(0x65),
        0x43 => Some(0x6D),
        0x44 => Some(0x67),
        0x45 => Some(0x6F),
        // Number row 1..9, 0
        0x1E => Some(0x12),
        0x1F => Some(0x13),
        0x20 => Some(0x14),
        0x21 => Some(0x15),
        0x22 => Some(0x17),
        0x23 => Some(0x16),
        0x24 => Some(0x1A),
        0x25 => Some(0x1C),
        0x26 => Some(0x19),
        0x27 => Some(0x1D),
        // Punctuation & special characters
        0x2D => Some(0x1B),
        0x2E => Some(0x18),
        0x2F => Some(0x21),
        0x30 => Some(0x1E),
        0x31 => Some(0x2A),
        0x33 => Some(0x29),
        0x34 => Some(0x27),
        0x35 => Some(0x32),
        0x36 => Some(0x2B),
        0x37 => Some(0x2F),
        0x38 => Some(0x2C),
        // Right modifiers
        0xE4 => Some(0x3E),
        0xE5 => Some(0x3C),
        0xE6 => Some(0x3D),
        0xE7 => Some(0x36),
        // Numpad
        0x53 => Some(0x47),
        0x54 => Some(0x4B),
        0x55 => Some(0x43),
        0x56 => Some(0x4E),
        0x57 => Some(0x45),
        0x58 => Some(0x4C),
        0x59 => Some(0x53),
        0x5A => Some(0x54),
        0x5B => Some(0x55),
        0x5C => Some(0x56),
        0x5D => Some(0x57),
        0x5E => Some(0x58),
        0x5F => Some(0x59),
        0x60 => Some(0x5B),
        0x61 => Some(0x5C),
        0x62 => Some(0x52),
        0x63 => Some(0x41),
        _ => None,
    }
}

pub fn park_cursor_at(x: i32, y: i32) {
    enable_background_cursor_control();
    let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x as f64, y as f64));
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
    let _ = CGDisplay::main().hide_cursor();
}

pub fn unpark_cursor() {
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    let _ = CGDisplay::main().show_cursor();
}

pub struct MacClipboardProvider;

impl MacClipboardProvider {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    /// Lightweight 250ms polling loop for macOS clipboard changes (Files or Text).
    pub fn run_listener<F: Fn(ClipboardContent) + Send + 'static>(on_change: F) {
        let mut last: Option<ClipboardContent> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let current = if let Some(files) = Self.get_files() {
                Some(ClipboardContent::Files(files))
            } else {
                Self.get_text().map(ClipboardContent::Text)
            };
            if let Some(item) = current {
                if last.as_ref() != Some(&item) {
                    last = Some(item.clone());
                    on_change(item);
                }
            }
        }
    }
}

impl ClipboardProvider for MacClipboardProvider {
    fn get_text(&self) -> Option<String> {
        let out = std::process::Command::new("pbpaste").output().ok()?;
        if out.status.success() {
            String::from_utf8(out.stdout).ok()
        } else {
            None
        }
    }

    fn set_text(&self, text: &str) {
        use std::io::Write;
        if let Ok(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }

    fn get_files(&self) -> Option<Vec<std::path::PathBuf>> {
        let script = r#"ObjC.import("Foundation");
ObjC.import("AppKit");
var pb = $.NSPasteboard.generalPasteboard;
var plist = pb.propertyListForType("NSFilenamesPboardType");
if (plist && plist.count > 0) {
  var out = [];
  for (var i = 0; i < plist.count; i++) {
    out.push(ObjC.unwrap(plist.objectAtIndex(i)));
  }
  out.join("\n");
} else {
  var urls = pb.readObjectsForClassesOptions($([$.NSURL]), $());
  if (!urls || urls.count === 0) { ""; } else {
    var out = [];
    for (var i = 0; i < urls.count; i++) {
      var u = urls.objectAtIndex(i);
      if (u.isFileURL) out.push(ObjC.unwrap(u.path));
    }
    out.join("\n");
  }
}"#;
        let out = std::process::Command::new("osascript")
            .args(["-l", "JavaScript", "-e", script])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8(out.stdout).ok()?;
        let paths: Vec<std::path::PathBuf> = s
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(std::path::PathBuf::from)
            .collect();
        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }

    fn set_files(&self, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let arr_items: Vec<String> = paths
            .iter()
            .map(|p| {
                let escaped = p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{escaped}\"")
            })
            .collect();
        let first_escaped = paths[0]
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let script = format!(
            r#"ObjC.import("Foundation");
ObjC.import("AppKit");
var pb = $.NSPasteboard.generalPasteboard;
pb.clearContents;
var arr = $.NSMutableArray.alloc.init;
var paths = [{}];
for (var i = 0; i < paths.length; i++) {{
  arr.addObject($.NSURL.fileURLWithPath(paths[i]));
}}
pb.writeObjects(arr);
pb.setPropertyListForType($([{}]), "NSFilenamesPboardType");
pb.setStringForType($.NSURL.fileURLWithPath("{}").absoluteString, "public.file-url");"#,
            arr_items.join(", "),
            arr_items.join(", "),
            first_escaped
        );
        let res = std::process::Command::new("osascript")
            .args(["-l", "JavaScript", "-e", &script])
            .output();
        if let Ok(out) = res {
            if !out.status.success() {
                tracing::warn!(
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "MacClipboardProvider::set_files osascript failed"
                );
            }
        }
    }
}
