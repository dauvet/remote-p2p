use p2pkm_proto::MouseButton;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

pub const INJECTED_MAGIC_TAG: usize = 0x5032_504B; // "P2PK"

#[derive(Debug, Error)]
pub enum HalError {
    #[error("failed to install OS input hook: {0}")]
    HookInstall(String),
    #[error("accessibility or input monitoring permission denied")]
    PermissionDenied,
    #[error("failed to enumerate displays: {0}")]
    DisplayEnum(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawInputEvent {
    MouseMove {
        dx: i32,
        dy: i32,
        abs_x: i32,
        abs_y: i32,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    MouseScroll {
        dx: i32,
        dy: i32,
    },
    Key {
        hid_usage: u16,
        pressed: bool,
    },
    /// Emitted when physical user hits the emergency panic hotkey (Ctrl+Alt+Shift+Esc).
    PanicHotkey,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    pub id: u32,
    pub is_primary: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

pub struct HookControl {
    /// True when this machine is CONTROLLING another peer (local input suppressed).
    pub suppress_local: AtomicBool,
    /// True when emergency panic hotkey was triggered.
    pub panic_triggered: AtomicBool,
}

impl HookControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            suppress_local: AtomicBool::new(false),
            panic_triggered: AtomicBool::new(false),
        })
    }

    #[inline(always)]
    pub fn is_suppressed(&self) -> bool {
        self.suppress_local.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn set_suppressed(&self, suppress: bool) {
        self.suppress_local.store(suppress, Ordering::Release);
    }

    #[inline(always)]
    pub fn trigger_panic(&self) {
        self.suppress_local.store(false, Ordering::Release);
        self.panic_triggered.store(true, Ordering::Release);
    }
}

pub trait InputInjector: Send + Sync {
    fn inject_mouse_move(&self, dx: i32, dy: i32);
    fn inject_mouse_abs(&self, x: i32, y: i32);
    fn inject_mouse_button(&self, button: MouseButton, pressed: bool);
    fn inject_mouse_scroll(&self, dx: i32, dy: i32);
    fn inject_key(&self, hid_usage: u16, pressed: bool);
    fn release_all_held(&self);
    fn cursor_pos(&self) -> Option<(i32, i32)> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    Files(Vec<std::path::PathBuf>),
}

pub trait ClipboardProvider: Send + Sync {
    fn get_text(&self) -> Option<String>;
    fn set_text(&self, text: &str);
    fn get_files(&self) -> Option<Vec<std::path::PathBuf>> {
        None
    }
    fn set_files(&self, _paths: &[std::path::PathBuf]) {}
}

#[derive(Default)]
pub struct MockClipboard {
    pub text: Mutex<Option<String>>,
    pub files: Mutex<Vec<std::path::PathBuf>>,
}

impl MockClipboard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl ClipboardProvider for MockClipboard {
    fn get_text(&self) -> Option<String> {
        self.text.lock().unwrap().clone()
    }

    fn set_text(&self, text: &str) {
        *self.text.lock().unwrap() = Some(text.to_string());
    }

    fn get_files(&self) -> Option<Vec<std::path::PathBuf>> {
        let f = self.files.lock().unwrap();
        if f.is_empty() {
            None
        } else {
            Some(f.clone())
        }
    }

    fn set_files(&self, paths: &[std::path::PathBuf]) {
        *self.files.lock().unwrap() = paths.to_vec();
    }
}

/// Deterministic mock injector for unit & integration tests.
#[derive(Default)]
pub struct MockInputHal {
    pub events: Mutex<Vec<RawInputEvent>>,
    pub release_all_count: Mutex<u32>,
    pub pos: Mutex<(i32, i32)>,
}

impl MockInputHal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn take_events(&self) -> Vec<RawInputEvent> {
        let mut guard = self.events.lock().unwrap();
        std::mem::take(&mut *guard)
    }
}

impl InputInjector for MockInputHal {
    fn inject_mouse_move(&self, dx: i32, dy: i32) {
        {
            let mut p = self.pos.lock().unwrap();
            p.0 = (p.0 + dx).max(0);
            p.1 = (p.1 + dy).max(0);
        }
        self.events.lock().unwrap().push(RawInputEvent::MouseMove {
            dx,
            dy,
            abs_x: 0,
            abs_y: 0,
        });
    }

    fn inject_mouse_abs(&self, x: i32, y: i32) {
        *self.pos.lock().unwrap() = (x, y);
        self.events.lock().unwrap().push(RawInputEvent::MouseMove {
            dx: 0,
            dy: 0,
            abs_x: x,
            abs_y: y,
        });
    }

    fn cursor_pos(&self) -> Option<(i32, i32)> {
        Some(*self.pos.lock().unwrap())
    }

    fn inject_mouse_button(&self, button: MouseButton, pressed: bool) {
        self.events
            .lock()
            .unwrap()
            .push(RawInputEvent::MouseButton { button, pressed });
    }

    fn inject_mouse_scroll(&self, dx: i32, dy: i32) {
        self.events
            .lock()
            .unwrap()
            .push(RawInputEvent::MouseScroll { dx, dy });
    }

    fn inject_key(&self, hid_usage: u16, pressed: bool) {
        self.events
            .lock()
            .unwrap()
            .push(RawInputEvent::Key { hid_usage, pressed });
    }

    fn release_all_held(&self) {
        *self.release_all_count.lock().unwrap() += 1;
    }
}

#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_control_panic_resets_suppression() {
        let ctrl = HookControl::new();
        ctrl.set_suppressed(true);
        assert!(ctrl.is_suppressed());
        ctrl.trigger_panic();
        assert!(!ctrl.is_suppressed());
        assert!(ctrl.panic_triggered.load(Ordering::Acquire));
    }

    #[test]
    fn mock_hal_records_events() {
        let hal = MockInputHal::new();
        hal.inject_mouse_move(10, -5);
        hal.inject_key(0x04, true);
        hal.inject_mouse_button(MouseButton::Left, true);
        hal.release_all_held();

        let evs = hal.take_events();
        assert_eq!(evs.len(), 3);
        assert_eq!(*hal.release_all_count.lock().unwrap(), 1);
    }
}
