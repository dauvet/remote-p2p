use p2pkm_proto::OsKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyMapMode {
    /// Automatically swaps Ctrl <-> Command between Windows/Linux and macOS.
    SmartErgonomic,
    /// Sends hardware HID usages unmodified.
    RawHardware,
    /// Uses user-defined CustomKeyMap matrix and custom key rules.
    Custom,
}

// USB HID Usage IDs (Page 0x07)
pub const HID_ESC: u16 = 0x29;
pub const HID_CAPS_LOCK: u16 = 0x39;
pub const HID_L_CTRL: u16 = 0xE0;
pub const HID_L_SHIFT: u16 = 0xE1;
pub const HID_L_ALT: u16 = 0xE2;
pub const HID_L_GUI: u16 = 0xE3; // Win / Cmd
pub const HID_R_CTRL: u16 = 0xE4;
pub const HID_R_SHIFT: u16 = 0xE5;
pub const HID_R_ALT: u16 = 0xE6;
pub const HID_R_GUI: u16 = 0xE7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModifierTarget {
    Ctrl,    // HID 0xE0 / 0xE4
    AltOpt,  // HID 0xE2 / 0xE6
    WinCmd,  // HID 0xE3 / 0xE7
    Shift,   // HID 0xE1 / 0xE5
}

impl ModifierTarget {
    pub fn to_hid(self, is_right: bool) -> u16 {
        match (self, is_right) {
            (Self::Ctrl, false) => HID_L_CTRL,
            (Self::Ctrl, true) => HID_R_CTRL,
            (Self::AltOpt, false) => HID_L_ALT,
            (Self::AltOpt, true) => HID_R_ALT,
            (Self::WinCmd, false) => HID_L_GUI,
            (Self::WinCmd, true) => HID_R_GUI,
            (Self::Shift, false) => HID_L_SHIFT,
            (Self::Shift, true) => HID_R_SHIFT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMappingRule {
    pub from_hid: u16,
    pub to_hid: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomKeyMap {
    /// Windows/Linux -> macOS modifier targets
    pub win_ctrl_to_mac: ModifierTarget,
    pub win_alt_to_mac: ModifierTarget,
    pub win_key_to_mac: ModifierTarget,
    /// macOS -> Windows/Linux modifier targets
    pub mac_cmd_to_win: ModifierTarget,
    pub mac_opt_to_win: ModifierTarget,
    pub mac_ctrl_to_win: ModifierTarget,
    /// Optional CapsLock remap (e.g. 0x29 = Escape, 0xE0 = Ctrl, 0x39 = CapsLock)
    pub caps_lock_to: u16,
    /// Arbitrary per-key HID overrides (applied before modifier mapping)
    pub custom_rules: Vec<KeyMappingRule>,
}

impl Default for CustomKeyMap {
    fn default() -> Self {
        Self {
            win_ctrl_to_mac: ModifierTarget::WinCmd, // Win Ctrl -> Mac Cmd
            win_alt_to_mac: ModifierTarget::AltOpt,  // Win Alt  -> Mac Option
            win_key_to_mac: ModifierTarget::Ctrl,    // Win Key  -> Mac Control
            mac_cmd_to_win: ModifierTarget::Ctrl,    // Mac Cmd  -> Win Ctrl
            mac_opt_to_win: ModifierTarget::AltOpt,  // Mac Opt  -> Win Alt
            mac_ctrl_to_win: ModifierTarget::WinCmd, // Mac Ctrl -> Win Key
            caps_lock_to: HID_CAPS_LOCK,
            custom_rules: Vec::new(),
        }
    }
}

/// Translates a canonical USB HID Usage ID from `source_os` to `target_os`.
#[inline]
pub fn map_hid_key(
    hid_usage: u16,
    source_os: OsKind,
    target_os: OsKind,
    mode: KeyMapMode,
) -> u16 {
    map_hid_key_with_custom(hid_usage, source_os, target_os, mode, &CustomKeyMap::default())
}

pub fn map_hid_key_with_custom(
    hid_usage: u16,
    source_os: OsKind,
    target_os: OsKind,
    mode: KeyMapMode,
    custom: &CustomKeyMap,
) -> u16 {
    if mode == KeyMapMode::RawHardware {
        return hid_usage;
    }

    if mode == KeyMapMode::Custom {
        // 1. Apply arbitrary custom key rules first
        for rule in &custom.custom_rules {
            if rule.from_hid == hid_usage {
                return rule.to_hid;
            }
        }
        // 2. Apply CapsLock remap if configured
        if hid_usage == HID_CAPS_LOCK && custom.caps_lock_to != HID_CAPS_LOCK {
            return custom.caps_lock_to;
        }
        if source_os == target_os {
            return hid_usage;
        }
        return match (source_os, target_os) {
            (OsKind::Windows | OsKind::Linux, OsKind::MacOS) => match hid_usage {
                HID_L_CTRL => custom.win_ctrl_to_mac.to_hid(false),
                HID_R_CTRL => custom.win_ctrl_to_mac.to_hid(true),
                HID_L_ALT => custom.win_alt_to_mac.to_hid(false),
                HID_R_ALT => custom.win_alt_to_mac.to_hid(true),
                HID_L_GUI => custom.win_key_to_mac.to_hid(false),
                HID_R_GUI => custom.win_key_to_mac.to_hid(true),
                other => other,
            },
            (OsKind::MacOS, OsKind::Windows | OsKind::Linux) => match hid_usage {
                HID_L_GUI => custom.mac_cmd_to_win.to_hid(false),
                HID_R_GUI => custom.mac_cmd_to_win.to_hid(true),
                HID_L_ALT => custom.mac_opt_to_win.to_hid(false),
                HID_R_ALT => custom.mac_opt_to_win.to_hid(true),
                HID_L_CTRL => custom.mac_ctrl_to_win.to_hid(false),
                HID_R_CTRL => custom.mac_ctrl_to_win.to_hid(true),
                other => other,
            },
            _ => hid_usage,
        };
    }

    // SmartErgonomic preset
    if source_os == target_os {
        return hid_usage;
    }

    match (source_os, target_os) {
        (OsKind::Windows | OsKind::Linux, OsKind::MacOS) => match hid_usage {
            HID_L_CTRL => HID_L_GUI, // Win Ctrl -> Mac Cmd
            HID_R_CTRL => HID_R_GUI,
            HID_L_GUI => HID_L_CTRL, // Win Key -> Mac Control
            HID_R_GUI => HID_R_CTRL,
            other => other,
        },
        (OsKind::MacOS, OsKind::Windows | OsKind::Linux) => match hid_usage {
            HID_L_GUI => HID_L_CTRL, // Mac Cmd -> Win Ctrl
            HID_R_GUI => HID_R_CTRL,
            HID_L_CTRL => HID_L_GUI, // Mac Control -> Win Key
            HID_R_CTRL => HID_R_GUI,
            other => other,
        },
        _ => hid_usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_ergonomic_swaps_ctrl_and_cmd_across_win_mac() {
        assert_eq!(
            map_hid_key(HID_L_CTRL, OsKind::Windows, OsKind::MacOS, KeyMapMode::SmartErgonomic),
            HID_L_GUI
        );
        assert_eq!(
            map_hid_key(HID_L_GUI, OsKind::MacOS, OsKind::Windows, KeyMapMode::SmartErgonomic),
            HID_L_CTRL
        );
        assert_eq!(
            map_hid_key(0x06, OsKind::Windows, OsKind::MacOS, KeyMapMode::SmartErgonomic),
            0x06
        );
    }

    #[test]
    fn raw_hardware_and_same_os_do_not_remap() {
        assert_eq!(
            map_hid_key(HID_L_CTRL, OsKind::Windows, OsKind::MacOS, KeyMapMode::RawHardware),
            HID_L_CTRL
        );
        assert_eq!(
            map_hid_key(HID_L_CTRL, OsKind::Windows, OsKind::Windows, KeyMapMode::SmartErgonomic),
            HID_L_CTRL
        );
    }

    #[test]
    fn custom_keymap_supports_modifier_matrix_and_caps_lock_rules() {
        let mut custom = CustomKeyMap::default();
        custom.win_alt_to_mac = ModifierTarget::WinCmd; // Map Win Alt -> Mac Cmd
        custom.win_ctrl_to_mac = ModifierTarget::Ctrl;  // Keep Win Ctrl -> Mac Ctrl
        custom.caps_lock_to = HID_ESC;                  // CapsLock -> Escape
        custom.custom_rules.push(KeyMappingRule {
            from_hid: 0x4C, // Delete
            to_hid: 0x2A,   // Backspace
        });

        assert_eq!(
            map_hid_key_with_custom(HID_L_ALT, OsKind::Windows, OsKind::MacOS, KeyMapMode::Custom, &custom),
            HID_L_GUI
        );
        assert_eq!(
            map_hid_key_with_custom(HID_R_CTRL, OsKind::Windows, OsKind::MacOS, KeyMapMode::Custom, &custom),
            HID_R_CTRL
        );
        assert_eq!(
            map_hid_key_with_custom(HID_CAPS_LOCK, OsKind::Windows, OsKind::MacOS, KeyMapMode::Custom, &custom),
            HID_ESC
        );
        assert_eq!(
            map_hid_key_with_custom(0x4C, OsKind::Windows, OsKind::MacOS, KeyMapMode::Custom, &custom),
            0x2A
        );
    }
}
