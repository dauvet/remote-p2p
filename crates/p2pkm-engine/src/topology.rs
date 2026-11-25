use p2pkm_proto::{PeerId, ScreenEdge};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerDisplayNode {
    pub peer_id: PeerId,
    pub hostname: String,
    /// Position in the Virtual Unified Desktop (logical points).
    pub canvas_x: f32,
    pub canvas_y: f32,
    /// Logical width/height on the canvas.
    pub logical_w: f32,
    pub logical_h: f32,
    /// Native OS physical pixel dimensions & DPI scale factor.
    pub physical_w: u32,
    pub physical_h: u32,
    pub scale_factor: f32,
    /// Top-left origin of the machine's virtual desktop in OS coordinates (can be negative on multi-monitor setups).
    #[serde(default)]
    pub origin_x: i32,
    #[serde(default)]
    pub origin_y: i32,
}

impl PeerDisplayNode {
    /// Returns `(min_x, min_y, max_x, max_y)` in native OS coordinates (inclusive bounds).
    pub fn physical_bounds(&self) -> (i32, i32, i32, i32) {
        let min_x = self.origin_x;
        let min_y = self.origin_y;
        let max_x = self.origin_x + (self.physical_w as i32 - 1).max(0);
        let max_y = self.origin_y + (self.physical_h as i32 - 1).max(0);
        (min_x, min_y, max_x, max_y)
    }

    /// Converts a local OS physical coordinate `(phys_x, phys_y)` into canvas logical coordinates.
    pub fn physical_to_canvas(&self, phys_x: i32, phys_y: i32) -> (f32, f32) {
        let lx = phys_x as f32 / self.scale_factor;
        let ly = phys_y as f32 / self.scale_factor;
        (self.canvas_x + lx, self.canvas_y + ly)
    }

    /// Converts an incoming `entry_edge` and `edge_ratio` in `[0.0, 1.0]` into local OS physical pixels.
    pub fn entry_to_physical(&self, entry_edge: ScreenEdge, ratio: f32) -> (i32, i32) {
        let r = ratio.clamp(0.0, 1.0);
        let max_x = (self.physical_w as i32 - 2).max(0);
        let max_y = (self.physical_h as i32 - 2).max(0);
        let (lx, ly) = match entry_edge {
            ScreenEdge::Left => (1, ((r * max_y as f32).round() as i32).clamp(0, max_y)),
            ScreenEdge::Right => (max_x, ((r * max_y as f32).round() as i32).clamp(0, max_y)),
            ScreenEdge::Top => (((r * max_x as f32).round() as i32).clamp(0, max_x), 1),
            ScreenEdge::Bottom => (((r * max_x as f32).round() as i32).clamp(0, max_x), max_y),
        };
        (self.origin_x + lx, self.origin_y + ly)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeCrossing {
    pub target_peer: PeerId,
    pub entry_edge: ScreenEdge,
    pub edge_ratio: f32,
}

#[derive(Default)]
pub struct VirtualDesktop {
    pub nodes: Vec<PeerDisplayNode>,
}

impl VirtualDesktop {
    pub fn new(nodes: Vec<PeerDisplayNode>) -> Self {
        Self { nodes }
    }

    /// Detects if a cursor at `(phys_x, phys_y)` on `source_peer` has hit an edge adjacent to another peer.
    /// Tolerance in logical pixels (e.g. `2.0`) handles Windows/macOS edge clamping.
    pub fn detect_edge_crossing(
        &self,
        source_peer: PeerId,
        phys_x: i32,
        phys_y: i32,
        tolerance_logical: f32,
    ) -> Option<EdgeCrossing> {
        let src = self.nodes.iter().find(|n| n.peer_id == source_peer)?;
        let lx = phys_x as f32 / src.scale_factor;
        let ly = phys_y as f32 / src.scale_factor;
        let (cx, cy) = src.physical_to_canvas(phys_x, phys_y);

        // Determine which edge of `src` was hit
        let hit_edge = if lx <= tolerance_logical {
            Some(ScreenEdge::Left)
        } else if lx >= src.logical_w - tolerance_logical - 1.0 {
            Some(ScreenEdge::Right)
        } else if ly <= tolerance_logical {
            Some(ScreenEdge::Top)
        } else if ly >= src.logical_h - tolerance_logical - 1.0 {
            Some(ScreenEdge::Bottom)
        } else {
            None
        }?;

        for dst in &self.nodes {
            if dst.peer_id == source_peer {
                continue;
            }
            match hit_edge {
                ScreenEdge::Right => {
                    // dst must be immediately to the right of src
                    if (dst.canvas_x - (src.canvas_x + src.logical_w)).abs() <= 16.0 {
                        let overlap_start = src.canvas_y.max(dst.canvas_y);
                        let overlap_end =
                            (src.canvas_y + src.logical_h).min(dst.canvas_y + dst.logical_h);
                        if cy >= overlap_start && cy <= overlap_end && overlap_end > overlap_start {
                            let ratio = (cy - dst.canvas_y) / dst.logical_h;
                            return Some(EdgeCrossing {
                                target_peer: dst.peer_id,
                                entry_edge: ScreenEdge::Left,
                                edge_ratio: ratio.clamp(0.0, 1.0),
                            });
                        }
                    }
                }
                ScreenEdge::Left => {
                    if ((dst.canvas_x + dst.logical_w) - src.canvas_x).abs() <= 16.0 {
                        let overlap_start = src.canvas_y.max(dst.canvas_y);
                        let overlap_end =
                            (src.canvas_y + src.logical_h).min(dst.canvas_y + dst.logical_h);
                        if cy >= overlap_start && cy <= overlap_end && overlap_end > overlap_start {
                            let ratio = (cy - dst.canvas_y) / dst.logical_h;
                            return Some(EdgeCrossing {
                                target_peer: dst.peer_id,
                                entry_edge: ScreenEdge::Right,
                                edge_ratio: ratio.clamp(0.0, 1.0),
                            });
                        }
                    }
                }
                ScreenEdge::Bottom => {
                    if (dst.canvas_y - (src.canvas_y + src.logical_h)).abs() <= 16.0 {
                        let overlap_start = src.canvas_x.max(dst.canvas_x);
                        let overlap_end =
                            (src.canvas_x + src.logical_w).min(dst.canvas_x + dst.logical_w);
                        if cx >= overlap_start && cx <= overlap_end && overlap_end > overlap_start {
                            let ratio = (cx - dst.canvas_x) / dst.logical_w;
                            return Some(EdgeCrossing {
                                target_peer: dst.peer_id,
                                entry_edge: ScreenEdge::Top,
                                edge_ratio: ratio.clamp(0.0, 1.0),
                            });
                        }
                    }
                }
                ScreenEdge::Top => {
                    if ((dst.canvas_y + dst.logical_h) - src.canvas_y).abs() <= 16.0 {
                        let overlap_start = src.canvas_x.max(dst.canvas_x);
                        let overlap_end =
                            (src.canvas_x + src.logical_w).min(dst.canvas_x + dst.logical_w);
                        if cx >= overlap_start && cx <= overlap_end && overlap_end > overlap_start {
                            let ratio = (cx - dst.canvas_x) / dst.logical_w;
                            return Some(EdgeCrossing {
                                target_peer: dst.peer_id,
                                entry_edge: ScreenEdge::Bottom,
                                edge_ratio: ratio.clamp(0.0, 1.0),
                            });
                        }
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retina_mac_to_4k_windows_dpi_scaling_crossing() {
        let mac_id = [1u8; 16];
        let win_id = [2u8; 16];

        // MacBook 14" Retina: 1512x982 logical (scale 1.0 in CGDisplayBounds)
        let mac = PeerDisplayNode {
            peer_id: mac_id,
            hostname: "MacBook".into(),
            canvas_x: 0.0,
            canvas_y: 0.0,
            logical_w: 1512.0,
            logical_h: 982.0,
            physical_w: 1512,
            physical_h: 982,
            scale_factor: 1.0,
            origin_x: 0,
            origin_y: 0,
        };
        // Windows 4K @ 150% DPI: 3840x2160 physical -> 2560x1440 logical
        let win = PeerDisplayNode {
            peer_id: win_id,
            hostname: "Win4K".into(),
            canvas_x: 1512.0,
            canvas_y: 0.0,
            logical_w: 2560.0,
            logical_h: 1440.0,
            physical_w: 3840,
            physical_h: 2160,
            scale_factor: 1.5,
            origin_x: 0,
            origin_y: 0,
        };

        let desktop = VirtualDesktop::new(vec![mac, win.clone()]);

        // Cursor hits right edge of Mac at y = 491 (50% of Mac height)
        let crossing = desktop
            .detect_edge_crossing(mac_id, 1511, 491, 2.0)
            .expect("must detect crossing to Win4K");

        assert_eq!(crossing.target_peer, win_id);
        assert_eq!(crossing.entry_edge, ScreenEdge::Left);
        // 491 / 1440 ≈ 0.341
        assert!((crossing.edge_ratio - (491.0 / 1440.0)).abs() < 0.01);

        let (wx, wy) = win.entry_to_physical(crossing.entry_edge, crossing.edge_ratio);
        assert_eq!(wx, 1);
        // 0.341 * 2158 ≈ 736 physical pixels on 4K screen
        assert!((wy - 736).abs() <= 2);
    }

    #[test]
    fn return_crossing_from_4k_windows_left_edge_to_mac() {
        let mac_id = [1u8; 16];
        let win_id = [2u8; 16];
        let mac = PeerDisplayNode {
            peer_id: mac_id,
            hostname: "MacBook".into(),
            canvas_x: 0.0,
            canvas_y: 0.0,
            logical_w: 1512.0,
            logical_h: 982.0,
            physical_w: 1512,
            physical_h: 982,
            scale_factor: 1.0,
            origin_x: 0,
            origin_y: 0,
        };
        let win = PeerDisplayNode {
            peer_id: win_id,
            hostname: "Win4K".into(),
            canvas_x: 1512.0,
            canvas_y: 0.0,
            logical_w: 2560.0,
            logical_h: 1440.0,
            physical_w: 3840,
            physical_h: 2160,
            scale_factor: 1.5,
            origin_x: 0,
            origin_y: 0,
        };
        let desktop = VirtualDesktop::new(vec![mac, win]);

        // On Win4K at physical (0, 736) -> logical (0, 490.67) -> hits Left edge back to Mac
        let crossing = desktop
            .detect_edge_crossing(win_id, 0, 736, 2.0)
            .expect("must cross back to Mac");
        assert_eq!(crossing.target_peer, mac_id);
        assert_eq!(crossing.entry_edge, ScreenEdge::Right);
        assert!((crossing.edge_ratio - 0.5).abs() < 0.01);
    }

    #[test]
    fn tall_host_4000x2560_crosses_left_at_lower_half() {
        let host_id = [1u8; 16];
        let mac_id = [2u8; 16];
        let host = PeerDisplayNode {
            peer_id: host_id,
            hostname: "DAUVETTINY".into(),
            canvas_x: 0.0,
            canvas_y: 0.0,
            logical_w: 4000.0,
            logical_h: 2560.0,
            physical_w: 4000,
            physical_h: 2560,
            scale_factor: 1.0,
            origin_x: 0,
            origin_y: 0,
        };
        let mac = PeerDisplayNode {
            peer_id: mac_id,
            hostname: "p2pkm-node".into(),
            canvas_x: -1920.0,
            canvas_y: 0.0,
            logical_w: 1920.0,
            logical_h: 2560.0,
            physical_w: 1920,
            physical_h: 2560,
            scale_factor: 1.0,
            origin_x: 0,
            origin_y: 0,
        };
        let desktop = VirtualDesktop::new(vec![host, mac]);
        let crossing = desktop
            .detect_edge_crossing(host_id, 0, 2000, 2.0)
            .expect("must cross left even in lower half y=2000");
        assert_eq!(crossing.target_peer, mac_id);
        assert_eq!(crossing.entry_edge, ScreenEdge::Right);
        assert!((crossing.edge_ratio - (2000.0 / 2560.0)).abs() < 0.01);
    }

    #[test]
    fn dual_monitor_negative_origin_maps_bounds_and_entry_correctly() {
        // Mac with 2 monitors: external 1920x1080 on the left (-1920..0) + built-in 1440x900 (0..1440)
        let mac = PeerDisplayNode {
            peer_id: [2u8; 16],
            hostname: "MacDual".into(),
            canvas_x: 1920.0,
            canvas_y: 0.0,
            logical_w: 3360.0,
            logical_h: 1080.0,
            physical_w: 3360,
            physical_h: 1080,
            scale_factor: 1.0,
            origin_x: -1920,
            origin_y: 0,
        };
        assert_eq!(mac.physical_bounds(), (-1920, 0, 1439, 1079));
        let (ex, ey) = mac.entry_to_physical(ScreenEdge::Left, 0.5);
        assert_eq!(ex, -1919);
        assert!((ey - 539).abs() <= 1);
        let (rx, ry) = mac.entry_to_physical(ScreenEdge::Right, 0.5);
        assert_eq!(rx, 1438);
        assert!((ry - 539).abs() <= 1);
    }
}
