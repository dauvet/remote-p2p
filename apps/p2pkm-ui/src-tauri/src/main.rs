#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use p2pkm_engine::{
    AppConfig, CustomKeyMap, KeyMapMode, Language, PeerDisplayNode, PeerPlacement, VirtualDesktop,
};
use p2pkm_hal::{ClipboardContent, ClipboardProvider, HookControl, InputInjector, RawInputEvent};
use p2pkm_net::{FocusState, MdnsDiscovery, MeshNode};
use p2pkm_proto::{OsKind, PeerId, ScreenEdge};
use rtrb::RingBuffer;
use serde::Serialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WindowEvent};
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info};
use uuid::Uuid;

#[cfg(target_os = "windows")]
fn platform_os() -> OsKind {
    OsKind::Windows
}
#[cfg(target_os = "macos")]
fn platform_os() -> OsKind {
    OsKind::MacOS
}

#[cfg(target_os = "windows")]
fn create_injector() -> Arc<dyn InputInjector> {
    p2pkm_hal::windows::WinInputInjector::new()
}
#[cfg(target_os = "macos")]
fn create_injector() -> Arc<dyn InputInjector> {
    p2pkm_hal::macos::MacInputInjector::new()
}

#[cfg(target_os = "windows")]
fn create_clipboard() -> Option<Arc<dyn ClipboardProvider>> {
    Some(p2pkm_hal::windows::WinClipboardProvider::new())
}
#[cfg(target_os = "macos")]
fn create_clipboard() -> Option<Arc<dyn ClipboardProvider>> {
    Some(p2pkm_hal::macos::MacClipboardProvider::new())
}

#[cfg(target_os = "windows")]
fn park_cursor(x: i32, y: i32) {
    p2pkm_hal::windows::park_cursor_at(x, y);
}
#[cfg(target_os = "macos")]
fn park_cursor(x: i32, y: i32) {
    p2pkm_hal::macos::park_cursor_at(x, y);
}

#[cfg(target_os = "windows")]
fn unpark_cursor() {
    p2pkm_hal::windows::unpark_cursor();
}
#[cfg(target_os = "macos")]
fn unpark_cursor() {
    p2pkm_hal::macos::unpark_cursor();
}

#[cfg(target_os = "windows")]
fn enumerate_displays() -> Vec<p2pkm_hal::DisplayInfo> {
    p2pkm_hal::windows::enumerate_displays().unwrap_or_default()
}
#[cfg(target_os = "macos")]
fn enumerate_displays() -> Vec<p2pkm_hal::DisplayInfo> {
    p2pkm_hal::macos::enumerate_displays().unwrap_or_default()
}

#[cfg(target_os = "windows")]
fn can_suppress_outgoing() -> bool {
    true
}
#[cfg(target_os = "macos")]
fn can_suppress_outgoing() -> bool {
    p2pkm_hal::macos::is_accessibility_trusted()
}

fn os_label(os: OsKind) -> &'static str {
    match os {
        OsKind::Windows => "Windows",
        OsKind::MacOS => "macOS",
        OsKind::Linux => "Linux",
    }
}

struct AppState {
    node: Arc<MeshNode>,
    desktop: Arc<RwLock<VirtualDesktop>>,
    local_node: Arc<RwLock<PeerDisplayNode>>,
    local_rects: Arc<RwLock<Vec<p2pkm_proto::RemoteDisplayRect>>>,
    config: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
    overshoot_limit: Arc<AtomicI32>,
    allow_outgoing: Arc<AtomicBool>,
}

#[derive(Serialize)]
struct UiPeer {
    hostname: String,
    os: &'static str,
    addr: String,
    logical_w: u32,
    logical_h: u32,
    physical_w: u32,
    physical_h: u32,
    display_count: usize,
    displays: Vec<p2pkm_proto::RemoteDisplayRect>,
}

#[derive(Serialize)]
struct UiStateResponse {
    hostname: String,
    os: &'static str,
    actual_port: u16,
    local_resolution: String,
    local_display_count: usize,
    local_displays: Vec<p2pkm_proto::RemoteDisplayRect>,
    focus_state: String,
    accessibility_ok: bool,
    config: AppConfig,
    peers: Vec<UiPeer>,
}

fn compute_local_display_snapshot(
    peer_id: PeerId,
    hostname: &str,
) -> (
    PeerDisplayNode,
    Vec<p2pkm_proto::RemoteDisplayRect>,
    i32,
    i32,
    i32,
    i32,
) {
    let displays = enumerate_displays();
    let primary = displays.iter().find(|d| d.is_primary).or(displays.first());
    let (min_x, min_y, max_x, max_y) = if displays.is_empty() {
        (0, 0, 1920, 1080)
    } else {
        (
            displays.iter().map(|d| d.x).min().unwrap_or(0),
            displays.iter().map(|d| d.y).min().unwrap_or(0),
            displays
                .iter()
                .map(|d| d.x + d.width as i32)
                .max()
                .unwrap_or(1920),
            displays
                .iter()
                .map(|d| d.y + d.height as i32)
                .max()
                .unwrap_or(1080),
        )
    };
    let phys_w = (max_x - min_x).max(1) as u32;
    let phys_h = (max_y - min_y).max(1) as u32;
    let scale = primary.map(|d| d.scale_factor).unwrap_or(1.0);
    let center_x = primary
        .map(|d| d.x + (d.width as i32) / 2)
        .unwrap_or(960);
    let center_y = primary
        .map(|d| d.y + (d.height as i32) / 2)
        .unwrap_or(540);
    let rects: Vec<p2pkm_proto::RemoteDisplayRect> = displays
        .iter()
        .map(|d| p2pkm_proto::RemoteDisplayRect {
            x: d.x,
            y: d.y,
            width: d.width,
            height: d.height,
            scale_factor_x100: (d.scale_factor * 100.0).round() as u16,
            is_primary: d.is_primary,
        })
        .collect();
    let node = PeerDisplayNode {
        peer_id,
        hostname: hostname.to_string(),
        canvas_x: 0.0,
        canvas_y: 0.0,
        logical_w: phys_w as f32 / scale,
        logical_h: phys_h as f32 / scale,
        physical_w: phys_w,
        physical_h: phys_h,
        scale_factor: scale,
        origin_x: min_x,
        origin_y: min_y,
    };
    (node, rects, min_x, min_y, center_x, center_y)
}

async fn sync_desktop_from_peers(
    cfg: &AppConfig,
    local: &PeerDisplayNode,
    node: &MeshNode,
    vd: &mut VirtualDesktop,
) {
    let mut new_nodes = vec![local.clone()];
    for (pid, hostname, _os, _addr, lw, lh, pw, ph, _rects) in node.peer_display_summaries().await {
        let placement = cfg
            .peer_placements
            .get(&hostname)
            .copied()
            .unwrap_or_default();
        let peer_w = (lw as f32).max(1.0);
        let peer_h = match placement {
            // Keep vertical span at least as tall as local for convenient side-edge crossing
            // while preserving actual multi-monitor dimensions when larger.
            PeerPlacement::Left | PeerPlacement::Right => (lh as f32).max(local.logical_h),
            PeerPlacement::Top | PeerPlacement::Bottom => (lh as f32).max(1.0),
        };
        let (cx, cy) = cfg.compute_peer_canvas_pos(local, &hostname, peer_w, peer_h);
        new_nodes.push(PeerDisplayNode {
            peer_id: pid,
            hostname,
            canvas_x: cx,
            canvas_y: cy,
            logical_w: peer_w,
            logical_h: peer_h,
            physical_w: pw.max(1),
            physical_h: ph.max(1),
            scale_factor: 1.0,
            origin_x: 0,
            origin_y: 0,
        });
    }
    vd.nodes = new_nodes;
}

#[tauri::command]
async fn get_state(state: tauri::State<'_, AppState>) -> Result<UiStateResponse, String> {
    let cfg = state.config.read().await.clone();
    let (fresh_local, fresh_rects, _, _, _, _) =
        compute_local_display_snapshot(state.node.peer_id, &state.node.hostname);
    let changed = {
        let cur = state.local_node.read().await;
        let cur_rects = state.local_rects.read().await;
        *cur != fresh_local || *cur_rects != fresh_rects
    };
    if changed {
        *state.local_node.write().await = fresh_local.clone();
        *state.local_rects.write().await = fresh_rects.clone();
        state
            .node
            .update_local_display(fresh_local.clone(), fresh_rects.clone())
            .await;
    }
    {
        let mut vd = state.desktop.write().await;
        sync_desktop_from_peers(&cfg, &fresh_local, &state.node, &mut vd).await;
    }
    let peers = state
        .node
        .peer_display_summaries()
        .await
        .into_iter()
        .map(
            |(_, hostname, os, addr, logical_w, logical_h, physical_w, physical_h, displays)| UiPeer {
                hostname,
                os: os_label(os),
                addr: addr.to_string(),
                logical_w,
                logical_h,
                physical_w,
                physical_h,
                display_count: displays.len().max(1),
                displays,
            },
        )
        .collect();
    let fstate = match state.node.focus_state().await {
        FocusState::Local => "Local".to_string(),
        FocusState::Controlling { .. } => "Controlling Remote".to_string(),
        FocusState::Controlled { .. } => "Controlled by Peer".to_string(),
    };
    Ok(UiStateResponse {
        hostname: state.node.hostname.clone(),
        os: os_label(state.node.os),
        actual_port: state.node.local_addr().port(),
        local_resolution: format!("{}x{}", fresh_local.physical_w, fresh_local.physical_h),
        local_display_count: fresh_rects.len().max(1),
        local_displays: fresh_rects,
        focus_state: fstate,
        accessibility_ok: can_suppress_outgoing(),
        config: cfg,
        peers,
    })
}

#[tauri::command]
async fn save_config(
    listen_port: u16,
    overshoot_px: i32,
    keymap_mode: KeyMapMode,
    custom_keymap: Option<CustomKeyMap>,
    allow_outgoing: Option<bool>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut cfg = state.config.write().await;
    cfg.listen_port = listen_port;
    cfg.overshoot_px = overshoot_px.clamp(10, 300);
    cfg.keymap_mode = keymap_mode;
    if let Some(ck) = custom_keymap {
        cfg.custom_keymap = ck;
    }
    if let Some(ao) = allow_outgoing {
        cfg.allow_outgoing = ao;
        state.allow_outgoing.store(ao, Ordering::Relaxed);
    }
    state
        .overshoot_limit
        .store(cfg.overshoot_px, Ordering::Relaxed);
    state.node.set_return_overshoot_px(cfg.overshoot_px);
    state
        .node
        .set_key_map(cfg.keymap_mode, cfg.custom_keymap.clone())
        .await;
    cfg.save_to(&state.config_path)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn set_placement(
    hostname: String,
    placement: PeerPlacement,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut cfg = state.config.write().await;
    cfg.peer_placements.insert(hostname, placement);
    let _ = cfg.save_to(&state.config_path);
    let local_r = state.local_node.read().await.clone();
    let mut vd = state.desktop.write().await;
    sync_desktop_from_peers(&cfg, &local_r, &state.node, &mut vd).await;
    Ok(())
}

#[tauri::command]
async fn add_manual_peer(addr: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let sock: SocketAddr = addr.parse().map_err(|e: std::net::AddrParseError| e.to_string())?;
    state.node.register_manual_addr(sock).await;
    {
        let mut cfg = state.config.write().await;
        if !cfg.manual_peers.contains(&addr) {
            cfg.manual_peers.push(addr);
            let _ = cfg.save_to(&state.config_path);
        }
    }
    let node_c = Arc::clone(&state.node);
    tokio::spawn(async move {
        let _ = node_c.connect_peer(sock).await;
    });
    Ok(())
}

#[tauri::command]
async fn trigger_panic(state: tauri::State<'_, AppState>) -> Result<(), String> {
    unpark_cursor();
    state.node.revoke_focus(1).await;
    Ok(())
}

#[tauri::command]
async fn refresh_peers(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.node.refresh_peers().await;
    let cfg = state.config.read().await.clone();
    let local_r = state.local_node.read().await.clone();
    let mut vd = state.desktop.write().await;
    sync_desktop_from_peers(&cfg, &local_r, &state.node, &mut vd).await;
    Ok(())
}

#[tauri::command]
async fn set_language(
    language: Language,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut cfg = state.config.write().await;
    cfg.language = language;
    cfg.save_to(&state.config_path)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn exit_app(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    unpark_cursor();
    state.node.revoke_focus(1).await;
    let _ = std::fs::remove_file(pid_file_path());
    app.exit(0);
    Ok(())
}

fn pid_file_path() -> PathBuf {
    std::env::temp_dir().join("p2pkm-ui.pid")
}

/// Returns `true` if this process should continue starting, or `false` if an existing healthy
/// instance is already running (after signaling it to show its window).
fn ensure_single_instance() -> bool {
    use std::io::Write;
    let addr: SocketAddr = "127.0.0.1:51997".parse().unwrap();
    if let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(250)) {
        let _ = stream.write_all(b"SHOW");
        return false; // Existing healthy instance is running
    }

    // Existing control port didn't answer -> if a stale/hung PID exists, force-kill it
    let pid_path = pid_file_path();
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(old_pid) = pid_str.trim().parse::<u32>() {
            if old_pid != std::process::id() {
                #[cfg(target_os = "windows")]
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/PID", &old_pid.to_string()])
                    .output();
                #[cfg(not(target_os = "windows"))]
                let _ = std::process::Command::new("kill")
                    .args(["-9", &old_pid.to_string()])
                    .output();
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    let _ = std::fs::write(&pid_path, std::process::id().to_string());
    true
}

fn build_tray_icon() -> Image<'static> {
    // 16x16 crisp cyan-blue rounded square icon in RGBA
    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for y in 0..16 {
        for x in 0..16 {
            let border = x == 0 || x == 15 || y == 0 || y == 15;
            if border {
                rgba.extend_from_slice(&[15, 23, 42, 255]);
            } else {
                rgba.extend_from_slice(&[56, 189, 248, 255]);
            }
        }
    }
    Image::new_owned(rgba, 16, 16)
}

struct TeeWriter {
    file: Option<Arc<std::sync::Mutex<std::fs::File>>>,
}

impl std::io::Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        if let Some(f) = &self.file {
            if let Ok(mut g) = f.lock() {
                let _ = g.write_all(buf);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stderr().flush();
        if let Some(f) = &self.file {
            if let Ok(mut g) = f.lock() {
                let _ = g.flush();
            }
        }
        Ok(())
    }
}

fn init_logging(config_path: &std::path::Path) -> PathBuf {
    let log_path = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("p2pkm.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Cap log file at 2 MB on startup so it never grows unboundedly
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > 2 * 1024 * 1024 {
            let _ = std::fs::write(&log_path, b"");
        }
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok()
        .map(|f| Arc::new(std::sync::Mutex::new(f)));

    let make_writer = move || TeeWriter { file: file.clone() };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mdns_sd=off".into()),
        )
        .with_ansi(false)
        .with_writer(make_writer)
        .init();
    log_path
}

fn main() {
    if !ensure_single_instance() {
        return;
    }
    let config_path = AppConfig::default_path();
    let log_path = init_logging(&config_path);
    info!(?log_path, "Initialized persistent logging");

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let cfg = AppConfig::load_from(&config_path);

    let peer_id: PeerId = *Uuid::new_v4().as_bytes();
    let hostname = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "p2pkm-node".into());

    let (local_node_init, local_rects_init, mut min_x, mut min_y, mut center_x, mut center_y) =
        compute_local_display_snapshot(peer_id, &hostname);
    let desktop = Arc::new(RwLock::new(VirtualDesktop::new(vec![local_node_init.clone()])));
    let local_node = Arc::new(RwLock::new(local_node_init.clone()));
    let local_rects = Arc::new(RwLock::new(local_rects_init.clone()));
    let hook_ctrl = HookControl::new();
    let injector = create_injector();
    let clipboard = create_clipboard();
    let overshoot_limit = Arc::new(AtomicI32::new(cfg.overshoot_px));

    let bind_addr: SocketAddr = format!("0.0.0.0:{}", cfg.listen_port).parse().unwrap();
    let node = match rt.block_on(MeshNode::bind_with_options(
        bind_addr,
        peer_id,
        platform_os(),
        hostname.clone(),
        local_node_init.clone(),
        cfg.keymap_mode,
        hook_ctrl.clone(),
        injector.clone(),
        clipboard.clone(),
    )) {
        Ok(n) => n,
        Err(e) => {
            error!(?e, port = cfg.listen_port, "Port busy, falling back to ephemeral port");
            let fallback: SocketAddr = "0.0.0.0:0".parse().unwrap();
            rt.block_on(MeshNode::bind_with_options(
                fallback,
                peer_id,
                platform_os(),
                hostname.clone(),
                local_node_init.clone(),
                cfg.keymap_mode,
                hook_ctrl.clone(),
                injector,
                clipboard,
            ))
            .expect("Failed to bind QUIC MeshNode on fallback port")
        }
    };
    rt.block_on(node.update_local_display(local_node_init.clone(), local_rects_init));
    node.set_return_overshoot_px(cfg.overshoot_px);
    rt.block_on(node.set_key_map(cfg.keymap_mode, cfg.custom_keymap.clone()));

    let config_state = Arc::new(RwLock::new(cfg.clone()));
    let allow_outgoing = Arc::new(AtomicBool::new(cfg.allow_outgoing));
    let port = node.local_addr().port();

    // Start mDNS, manual peers, clipboard, and OS hook inside Tokio runtime
    let node_bg = Arc::clone(&node);
    let desktop_bg = Arc::clone(&desktop);
    let cfg_bg = Arc::clone(&config_state);
    let local_bg = Arc::clone(&local_node);
    let rects_bg = Arc::clone(&local_rects);
    let overshoot_bg = Arc::clone(&overshoot_limit);
    let allow_outgoing_bg = Arc::clone(&allow_outgoing);
    let hook_bg = Arc::clone(&hook_ctrl);
    let hostname_bg = hostname.clone();

    rt.spawn(async move {
        let (_mdns, mut rx_peers) =
            MdnsDiscovery::start(peer_id, &hostname, platform_os(), port)
                .expect("Failed to start mDNS discovery");

        for raw in &cfg.manual_peers {
            if let Ok(addr) = raw.parse::<SocketAddr>() {
                node_bg.register_manual_addr(addr).await;
                let nm = Arc::clone(&node_bg);
                let dm = Arc::clone(&desktop_bg);
                let cm = Arc::clone(&cfg_bg);
                let lm = Arc::clone(&local_bg);
                tokio::spawn(async move {
                    if let Ok(_remote_id) = nm.connect_peer(addr).await {
                        let cr = cm.read().await;
                        let local_r = lm.read().await.clone();
                        let mut vd = dm.write().await;
                        sync_desktop_from_peers(&cr, &local_r, &nm, &mut vd).await;
                    }
                });
            }
        }

        let nc = Arc::clone(&node_bg);
        let dc = Arc::clone(&desktop_bg);
        let cc = Arc::clone(&cfg_bg);
        let lc = Arc::clone(&local_bg);
        tokio::spawn(async move {
            while let Some(peer) = rx_peers.recv().await {
                nc.register_peer_addr(peer.peer_id, peer.addr).await;
                if nc.connect_peer(peer.addr).await.is_err() {
                    continue;
                }
                let cr = cc.read().await;
                let local_r = lc.read().await.clone();
                let mut vd = dc.write().await;
                sync_desktop_from_peers(&cr, &local_r, &nc, &mut vd).await;
            }
        });

        // Clipboard listener (Text + Files/Folders)
        let (clip_tx, mut clip_rx) = mpsc::channel::<ClipboardContent>(32);
        std::thread::Builder::new()
            .name("p2pkm-clipboard".into())
            .spawn(move || {
                #[cfg(target_os = "windows")]
                let _ = p2pkm_hal::windows::WinClipboardProvider::run_listener(move |item| {
                    let _ = clip_tx.blocking_send(item);
                });
                #[cfg(target_os = "macos")]
                p2pkm_hal::macos::MacClipboardProvider::run_listener(move |item| {
                    let _ = clip_tx.blocking_send(item);
                });
            })
            .ok();
        let nclip = Arc::clone(&node_bg);
        tokio::spawn(async move {
            while let Some(item) = clip_rx.recv().await {
                match item {
                    ClipboardContent::Text(txt) => nclip.broadcast_clipboard_text(&txt).await,
                    ClipboardContent::Files(paths) => nclip.broadcast_copied_paths(&paths).await,
                }
            }
        });

        // OS hook loop
        let (prod, mut cons) = RingBuffer::<RawInputEvent>::new(4096);
        let h_thread = Arc::clone(&hook_bg);
        std::thread::Builder::new()
            .name("p2pkm-os-hook".into())
            .spawn(move || {
                #[cfg(target_os = "windows")]
                if let Err(e) = p2pkm_hal::windows::run_hook_loop(h_thread, prod) {
                    error!(?e, "Win32 hook loop failed");
                }
                #[cfg(target_os = "macos")]
                if let Err(e) = p2pkm_hal::macos::run_hook_loop(h_thread, prod) {
                    error!(?e, "macOS CGEventTap failed");
                }
            })
            .expect("Failed to spawn OS hook thread");

        let mut was_suppressed = false;
        let mut last_focus_req = std::time::Instant::now() - Duration::from_secs(1);
        let mut last_display_poll = std::time::Instant::now();
        let mut last_desync_check = std::time::Instant::now();
        let mut outgoing_overshoot: i32 = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(2)).await;
            if last_desync_check.elapsed() >= Duration::from_millis(250) {
                last_desync_check = std::time::Instant::now();
                if hook_bg.is_suppressed()
                    && !matches!(node_bg.focus_state().await, FocusState::Controlling { .. })
                {
                    tracing::warn!("Suppression/FSM desync detected -> forcing unpark & local recovery");
                    hook_bg.set_suppressed(false);
                    unpark_cursor();
                }
            }
            if last_display_poll.elapsed() >= Duration::from_secs(1) {
                last_display_poll = std::time::Instant::now();
                let (fresh_node, fresh_rects, new_min_x, new_min_y, new_cx, new_cy) =
                    compute_local_display_snapshot(peer_id, &hostname_bg);
                min_x = new_min_x;
                min_y = new_min_y;
                center_x = new_cx;
                center_y = new_cy;
                let changed = {
                    let cur = local_bg.read().await;
                    let cur_r = rects_bg.read().await;
                    *cur != fresh_node || *cur_r != fresh_rects
                };
                if changed {
                    *local_bg.write().await = fresh_node.clone();
                    *rects_bg.write().await = fresh_rects.clone();
                    node_bg
                        .update_local_display(fresh_node.clone(), fresh_rects)
                        .await;
                    let cr = cfg_bg.read().await;
                    let mut vd = desktop_bg.write().await;
                    sync_desktop_from_peers(&cr, &fresh_node, &node_bg, &mut vd).await;
                }
            }
            let is_suppressed = hook_bg.is_suppressed();
            if is_suppressed && !was_suppressed {
                outgoing_overshoot = 0;
                park_cursor(center_x, center_y);
            } else if !is_suppressed && was_suppressed {
                outgoing_overshoot = 0;
                unpark_cursor();
            }
            was_suppressed = is_suppressed;

            while let Ok(ev) = cons.pop() {
                match ev {
                    RawInputEvent::PanicHotkey => {
                        outgoing_overshoot = 0;
                        unpark_cursor();
                        node_bg.revoke_focus(1).await;
                    }
                    RawInputEvent::MouseButton { pressed: true, .. } if !is_suppressed => {
                        node_bg.preempt_if_controlled().await;
                    }
                    RawInputEvent::MouseMove { dx, dy, abs_x, abs_y } if !is_suppressed => {
                        if dx.abs() + dy.abs() >= 20 {
                            node_bg.preempt_if_controlled().await;
                        }
                        if allow_outgoing_bg.load(Ordering::Relaxed)
                            && can_suppress_outgoing()
                            && last_focus_req.elapsed() >= Duration::from_millis(200)
                        {
                            let crossing = desktop_bg.read().await.detect_edge_crossing(
                                peer_id,
                                abs_x - min_x,
                                abs_y - min_y,
                                2.0,
                            );
                            if let Some(c) = crossing {
                                let push = match c.entry_edge {
                                    ScreenEdge::Left => dx.max(0),
                                    ScreenEdge::Right => (-dx).max(0),
                                    ScreenEdge::Top => dy.max(0),
                                    ScreenEdge::Bottom => (-dy).max(0),
                                };
                                outgoing_overshoot += push.max(8);
                                if outgoing_overshoot >= overshoot_bg.load(Ordering::Relaxed) {
                                    outgoing_overshoot = 0;
                                    last_focus_req = std::time::Instant::now();
                                    let _ = node_bg
                                        .request_focus(c.target_peer, c.entry_edge, c.edge_ratio)
                                        .await;
                                }
                            } else {
                                outgoing_overshoot = 0;
                            }
                        }
                    }
                    other => {
                        node_bg.send_input_event(other).await;
                    }
                }
            }
        }
    });

    let app_state = AppState {
        node: Arc::clone(&node),
        desktop,
        local_node,
        local_rects,
        config: config_state,
        config_path,
        overshoot_limit,
        allow_outgoing,
    };

    let node_panic = Arc::clone(&node);
    let hook_panic = Arc::clone(&hook_ctrl);
    std::panic::set_hook(Box::new(move |info| {
        error!(%info, "Unhandled panic in p2pkm-ui -> emergency unpark");
        hook_panic.trigger_panic();
        unpark_cursor();
    }));
    info!("Starting Remote P2P");
    tauri::Builder::default()
        .manage(app_state)
        .setup(move |app| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }

            // Single-instance listener on 127.0.0.1:51997
            let app_handle = app.handle().clone();
            std::thread::Builder::new()
                .name("p2pkm-single-instance".into())
                .spawn(move || {
                    if let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:51997") {
                        for stream in listener.incoming().flatten() {
                            drop(stream);
                            if let Some(win) = app_handle.get_webview_window("main") {
                                let _ = win.show();
                                let _ = win.unminimize();
                                let _ = win.set_focus();
                            }
                        }
                    }
                })
                .ok();

            let show = MenuItem::with_id(app, "show", "Show Control Center", true, None::<&str>)?;
            let panic = MenuItem::with_id(app, "panic", "Panic Reset (Local)", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &panic, &quit])?;

            let np = Arc::clone(&node_panic);
            TrayIconBuilder::new()
                .icon(build_tray_icon())
                .tooltip("Remote P2P")
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                    "panic" => {
                        unpark_cursor();
                        let np2 = Arc::clone(&np);
                        tauri::async_runtime::spawn(async move {
                            np2.revoke_focus(1).await;
                        });
                    }
                    "quit" => {
                        let _ = std::fs::remove_file(pid_file_path());
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_config,
            set_placement,
            add_manual_peer,
            trigger_panic,
            refresh_peers,
            set_language,
            exit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
