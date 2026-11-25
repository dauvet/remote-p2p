use p2pkm_engine::{
    AppConfig, CustomKeyMap, KeyMapMode, Language, PeerDisplayNode, PeerPlacement, VirtualDesktop,
};
use p2pkm_net::{FocusState, MeshNode};
use p2pkm_proto::OsKind;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{error, info};

const INDEX_HTML: &str = include_str!("../../../apps/p2pkm-ui/index.html");

#[derive(Serialize)]
struct UiPeer {
    hostname: String,
    os: &'static str,
    addr: String,
}

#[derive(Serialize)]
struct UiStateResponse {
    hostname: String,
    os: &'static str,
    local_resolution: String,
    focus_state: String,
    config: AppConfig,
    peers: Vec<UiPeer>,
}

#[derive(Deserialize)]
struct ConfigUpdateReq {
    listen_port: u16,
    overshoot_px: i32,
    keymap_mode: KeyMapMode,
    custom_keymap: Option<CustomKeyMap>,
}

#[derive(Deserialize)]
struct PlacementReq {
    hostname: String,
    placement: PeerPlacement,
}

#[derive(Deserialize)]
struct AddPeerReq {
    addr: String,
}

#[derive(Deserialize)]
struct LanguageReq {
    language: Language,
}

pub fn os_label(os: OsKind) -> &'static str {
    match os {
        OsKind::Windows => "Windows",
        OsKind::MacOS => "macOS",
        OsKind::Linux => "Linux",
    }
}

pub fn rebuild_desktop_from_config(
    cfg: &AppConfig,
    local_node: &PeerDisplayNode,
    vd: &mut VirtualDesktop,
) {
    let mut new_nodes = vec![local_node.clone()];
    for existing in &vd.nodes {
        if existing.peer_id == local_node.peer_id {
            continue;
        }
        let mut updated = existing.clone();
        let placement = cfg
            .peer_placements
            .get(&existing.hostname)
            .copied()
            .unwrap_or_default();
        let (pw, ph) = match placement {
            PeerPlacement::Left | PeerPlacement::Right => (updated.logical_w, local_node.logical_h),
            PeerPlacement::Top | PeerPlacement::Bottom => (local_node.logical_w, updated.logical_h),
        };
        updated.logical_w = pw;
        updated.logical_h = ph;
        let (cx, cy) = cfg.compute_peer_canvas_pos(local_node, &existing.hostname, pw, ph);
        updated.canvas_x = cx;
        updated.canvas_y = cy;
        new_nodes.push(updated);
    }
    vd.nodes = new_nodes;
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_web_ui(
    ui_port: u16,
    node: Arc<MeshNode>,
    desktop: Arc<RwLock<VirtualDesktop>>,
    local_node: PeerDisplayNode,
    config: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
    outgoing_overshoot_limit: Arc<AtomicI32>,
) {
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{ui_port}");
        let Ok(listener) = TcpListener::bind(&addr).await else {
            error!(%addr, "Failed to bind Web Control Center UI");
            return;
        };
        info!("Web Control Center UI listening at http://{addr}");

        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let node = Arc::clone(&node);
            let desktop = Arc::clone(&desktop);
            let local_node = local_node.clone();
            let config = Arc::clone(&config);
            let config_path = config_path.clone();
            let overshoot_limit = Arc::clone(&outgoing_overshoot_limit);

            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let Ok(n) = stream.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("GET");
                let path = parts.next().unwrap_or("/");
                let body = req.split("\r\n\r\n").nth(1).unwrap_or("").trim();

                match (method, path) {
                    ("GET", "/") | ("GET", "/index.html") => {
                        write_http(&mut stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML).await;
                    }
                    ("GET", "/api/state") => {
                        let cfg = config.read().await.clone();
                        let peers = node
                            .peer_summaries()
                            .await
                            .into_iter()
                            .map(|(_, hostname, os, addr)| UiPeer {
                                hostname,
                                os: os_label(os),
                                addr: addr.to_string(),
                            })
                            .collect();
                        let fstate = match node.focus_state().await {
                            FocusState::Local => "Local".to_string(),
                            FocusState::Controlling { .. } => "Controlling Remote".to_string(),
                            FocusState::Controlled { .. } => "Controlled by Peer".to_string(),
                        };
                        let resp = UiStateResponse {
                            hostname: node.hostname.clone(),
                            os: os_label(node.os),
                            local_resolution: format!(
                                "{}x{}",
                                local_node.physical_w, local_node.physical_h
                            ),
                            focus_state: fstate,
                            config: cfg,
                            peers,
                        };
                        let json = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into());
                        write_http(&mut stream, "200 OK", "application/json", &json).await;
                    }
                    ("POST", "/api/config") => {
                        if let Ok(upd) = serde_json::from_str::<ConfigUpdateReq>(body) {
                            let mut cfg = config.write().await;
                            cfg.listen_port = upd.listen_port;
                            cfg.overshoot_px = upd.overshoot_px.clamp(10, 300);
                            cfg.keymap_mode = upd.keymap_mode;
                            if let Some(ck) = upd.custom_keymap {
                                cfg.custom_keymap = ck;
                            }
                            overshoot_limit.store(cfg.overshoot_px, Ordering::Relaxed);
                            node.set_return_overshoot_px(cfg.overshoot_px);
                            node.set_key_map(cfg.keymap_mode, cfg.custom_keymap.clone()).await;
                            let _ = cfg.save_to(&config_path);
                        }
                        write_http(&mut stream, "200 OK", "application/json", r#"{"ok":true}"#).await;
                    }
                    ("POST", "/api/placement") => {
                        if let Ok(req) = serde_json::from_str::<PlacementReq>(body) {
                            let mut cfg = config.write().await;
                            cfg.peer_placements.insert(req.hostname, req.placement);
                            let _ = cfg.save_to(&config_path);
                            let mut vd = desktop.write().await;
                            rebuild_desktop_from_config(&cfg, &local_node, &mut vd);
                        }
                        write_http(&mut stream, "200 OK", "application/json", r#"{"ok":true}"#).await;
                    }
                    ("POST", "/api/peers/add") => {
                        if let Ok(req) = serde_json::from_str::<AddPeerReq>(body) {
                            if let Ok(sock) = req.addr.parse::<SocketAddr>() {
                                node.register_manual_addr(sock).await;
                                let mut cfg = config.write().await;
                                if !cfg.manual_peers.contains(&req.addr) {
                                    cfg.manual_peers.push(req.addr);
                                    let _ = cfg.save_to(&config_path);
                                }
                                let node_c = Arc::clone(&node);
                                tokio::spawn(async move {
                                    let _ = node_c.connect_peer(sock).await;
                                });
                            }
                        }
                        write_http(&mut stream, "200 OK", "application/json", r#"{"ok":true}"#).await;
                    }
                    ("POST", "/api/panic") => {
                        node.revoke_focus(1).await;
                        write_http(&mut stream, "200 OK", "application/json", r#"{"ok":true}"#).await;
                    }
                    ("POST", "/api/language") => {
                        if let Ok(req) = serde_json::from_str::<LanguageReq>(body) {
                            let mut cfg = config.write().await;
                            cfg.language = req.language;
                            let _ = cfg.save_to(&config_path);
                        }
                        write_http(&mut stream, "200 OK", "application/json", r#"{"ok":true}"#).await;
                    }
                    _ => {
                        write_http(&mut stream, "404 Not Found", "text/plain", "Not Found").await;
                    }
                }
            });
        }
    });
}

async fn write_http(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
}
