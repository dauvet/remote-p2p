use crate::keymap::{CustomKeyMap, KeyMapMode};
use crate::topology::PeerDisplayNode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_LISTEN_PORT: u16 = 51999;
pub const DEFAULT_OVERSHOOT_PX: i32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Vi,
    En,
}

impl Default for Language {
    fn default() -> Self {
        Self::Vi
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerPlacement {
    Left,
    Right,
    Top,
    Bottom,
}

impl Default for PeerPlacement {
    fn default() -> Self {
        Self::Right
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub language: Language,
    pub listen_port: u16,
    /// Pixels of cursor push past screen edge before switching (default: 100px).
    pub overshoot_px: i32,
    #[serde(default = "default_true")]
    pub allow_outgoing: bool,
    pub keymap_mode: KeyMapMode,
    #[serde(default)]
    pub custom_keymap: CustomKeyMap,
    /// Manual peer IP:PORT addresses (fallback when mDNS is blocked).
    pub manual_peers: Vec<String>,
    /// Maps peer hostname (or IP:PORT) -> placement relative to local host.
    pub peer_placements: HashMap<String, PeerPlacement>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: Language::Vi,
            listen_port: DEFAULT_LISTEN_PORT,
            overshoot_px: DEFAULT_OVERSHOOT_PX,
            allow_outgoing: true,
            keymap_mode: KeyMapMode::SmartErgonomic,
            custom_keymap: CustomKeyMap::default(),
            manual_peers: Vec::new(),
            peer_placements: HashMap::new(),
        }
    }
}

impl AppConfig {
    pub fn default_path() -> PathBuf {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("p2pkm").join("config.json");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("p2pkm")
                .join("config.json");
        }
        PathBuf::from("p2pkm-config.json")
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Computes `(canvas_x, canvas_y)` for a remote peer relative to `local_node`
    /// based on `peer_placements` (defaults to `Right`).
    pub fn compute_peer_canvas_pos(
        &self,
        local: &PeerDisplayNode,
        peer_key: &str,
        remote_w: f32,
        remote_h: f32,
    ) -> (f32, f32) {
        let placement = self
            .peer_placements
            .get(peer_key)
            .copied()
            .unwrap_or(PeerPlacement::Right);
        match placement {
            PeerPlacement::Right => (local.canvas_x + local.logical_w, local.canvas_y),
            PeerPlacement::Left => (local.canvas_x - remote_w, local.canvas_y),
            PeerPlacement::Top => (local.canvas_x, local.canvas_y - remote_h),
            PeerPlacement::Bottom => (local.canvas_x, local.canvas_y + local.logical_h),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_port_51999_and_overshoot_100() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.listen_port, 51999);
        assert_eq!(cfg.overshoot_px, 100);
        assert_eq!(cfg.keymap_mode, KeyMapMode::SmartErgonomic);
    }

    #[test]
    fn placement_computes_left_right_top_bottom_relative_to_host() {
        let mut cfg = AppConfig::default();
        cfg.peer_placements
            .insert("MacBook".into(), PeerPlacement::Left);
        let host = PeerDisplayNode {
            peer_id: [1u8; 16],
            hostname: "WinHost".into(),
            canvas_x: 0.0,
            canvas_y: 0.0,
            logical_w: 1920.0,
            logical_h: 1080.0,
            physical_w: 1920,
            physical_h: 1080,
            scale_factor: 1.0,
            origin_x: 0,
            origin_y: 0,
        };
        assert_eq!(
            cfg.compute_peer_canvas_pos(&host, "MacBook", 1512.0, 982.0),
            (-1512.0, 0.0)
        );
        assert_eq!(
            cfg.compute_peer_canvas_pos(&host, "Other", 1920.0, 1080.0),
            (1920.0, 0.0)
        );
    }
}
