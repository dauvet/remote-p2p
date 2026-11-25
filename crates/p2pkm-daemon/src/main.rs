mod web_ui;

use p2pkm_engine::{AppConfig, PeerDisplayNode, VirtualDesktop};
use p2pkm_hal::{ClipboardContent, ClipboardProvider, HookControl, InputInjector, RawInputEvent};
use p2pkm_net::{FocusState, MdnsDiscovery, MeshNode};
use p2pkm_proto::{OsKind, PeerId};
use rtrb::RingBuffer;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mdns_sd=off".into()),
        )
        .init();

    let config_path = AppConfig::default_path();
    let mut cfg = AppConfig::load_from(&config_path);
    let mut ui_port: u16 = 51998;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                if let Some(p) = args.next().and_then(|s| s.parse::<u16>().ok()) {
                    cfg.listen_port = p;
                }
            }
            "--ui-port" => {
                if let Some(p) = args.next().and_then(|s| s.parse::<u16>().ok()) {
                    ui_port = p;
                }
            }
            "--overshoot" => {
                if let Some(px) = args.next().and_then(|s| s.parse::<i32>().ok()) {
                    cfg.overshoot_px = px.clamp(10, 300);
                }
            }
            "--peer" => {
                if let Some(raw) = args.next() {
                    if raw.parse::<SocketAddr>().is_ok() && !cfg.manual_peers.contains(&raw) {
                        cfg.manual_peers.push(raw);
                    }
                }
            }
            _ => {}
        }
    }

    let peer_id: PeerId = *Uuid::new_v4().as_bytes();
    let hostname = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "p2pkm-node".into());

    let displays = enumerate_displays();
    info!(?displays, "Detected displays");
    let primary = displays.iter().find(|d| d.is_primary).or(displays.first());
    let (min_x, min_y, max_x, max_y) = if displays.is_empty() {
        (0, 0, 1920, 1080)
    } else {
        let min_x = displays.iter().map(|d| d.x).min().unwrap_or(0);
        let min_y = displays.iter().map(|d| d.y).min().unwrap_or(0);
        let max_x = displays
            .iter()
            .map(|d| d.x + d.width as i32)
            .max()
            .unwrap_or(1920);
        let max_y = displays
            .iter()
            .map(|d| d.y + d.height as i32)
            .max()
            .unwrap_or(1080);
        (min_x, min_y, max_x, max_y)
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

    let local_node = PeerDisplayNode {
        peer_id,
        hostname: hostname.clone(),
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
    let desktop = Arc::new(RwLock::new(VirtualDesktop::new(vec![local_node.clone()])));

    let hook_ctrl = HookControl::new();
    let injector = create_injector();
    let clipboard = create_clipboard();

    let listen_port = cfg.listen_port;
    let bind_addr: SocketAddr = format!("0.0.0.0:{listen_port}").parse().unwrap();
    let node = MeshNode::bind_with_options(
        bind_addr,
        peer_id,
        platform_os(),
        hostname.clone(),
        local_node.clone(),
        cfg.keymap_mode,
        hook_ctrl.clone(),
        injector,
        clipboard,
    )
    .await
    .expect("Failed to bind QUIC MeshNode");

    node.set_return_overshoot_px(cfg.overshoot_px);
    let outgoing_overshoot_limit = Arc::new(AtomicI32::new(cfg.overshoot_px));
    let manual_peers: Vec<SocketAddr> = cfg
        .manual_peers
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    let config_state = Arc::new(RwLock::new(cfg));

    let port = node.local_addr().port();
    info!(%hostname, port, overshoot_px = outgoing_overshoot_limit.load(Ordering::Relaxed), "MeshNode listening");

    web_ui::spawn_web_ui(
        ui_port,
        Arc::clone(&node),
        Arc::clone(&desktop),
        local_node.clone(),
        Arc::clone(&config_state),
        config_path,
        Arc::clone(&outgoing_overshoot_limit),
    );

    let (_mdns, mut rx_peers) =
        MdnsDiscovery::start(peer_id, &hostname, platform_os(), port)
            .expect("Failed to start mDNS discovery");

    // Connect to any manually specified --peer <IP:PORT>
    for addr in manual_peers {
        node.register_manual_addr(addr).await;
        let node_m = Arc::clone(&node);
        let desktop_m = Arc::clone(&desktop);
        let cfg_m = Arc::clone(&config_state);
        let local_m = local_node.clone();
        tokio::spawn(async move {
            info!(%addr, "Connecting to manual peer");
            match node_m.connect_peer(addr).await {
                Ok(remote_id) => {
                    info!(%addr, ?remote_id, "Connected to manual peer");
                    let cfg_r = cfg_m.read().await;
                    let mut vd = desktop_m.write().await;
                    if !vd.nodes.iter().any(|n| n.peer_id == remote_id) {
                        let key = addr.to_string();
                        let (cx, cy) = cfg_r.compute_peer_canvas_pos(&local_m, &key, 1920.0, 1080.0);
                        vd.nodes.push(PeerDisplayNode {
                            peer_id: remote_id,
                            hostname: key,
                            canvas_x: cx,
                            canvas_y: cy,
                            logical_w: 1920.0,
                            logical_h: 1080.0,
                            physical_w: 1920,
                            physical_h: 1080,
                            scale_factor: 1.0,
                            origin_x: 0,
                            origin_y: 0,
                        });
                    }
                }
                Err(e) => error!(%addr, ?e, "Failed to connect to manual peer (will auto-retry)"),
            }
        });
    }

    // Auto-connect to discovered peers and position them according to AppConfig
    let node_conn = Arc::clone(&node);
    let desktop_conn = Arc::clone(&desktop);
    let cfg_conn = Arc::clone(&config_state);
    let local_conn = local_node.clone();
    tokio::spawn(async move {
        while let Some(peer) = rx_peers.recv().await {
            info!(hostname = %peer.hostname, addr = %peer.addr, "Discovered peer via mDNS");
            node_conn.register_peer_addr(peer.peer_id, peer.addr).await;
            if let Err(e) = node_conn.connect_peer(peer.addr).await {
                error!(?e, "Failed to connect to peer (will auto-retry)");
                continue;
            }
            let cfg_r = cfg_conn.read().await;
            let mut vd = desktop_conn.write().await;
            if !vd.nodes.iter().any(|n| n.peer_id == peer.peer_id) {
                let (cx, cy) =
                    cfg_r.compute_peer_canvas_pos(&local_conn, &peer.hostname, 1920.0, 1080.0);
                vd.nodes.push(PeerDisplayNode {
                    peer_id: peer.peer_id,
                    hostname: peer.hostname,
                    canvas_x: cx,
                    canvas_y: cy,
                    logical_w: 1920.0,
                    logical_h: 1080.0,
                    physical_w: 1920,
                    physical_h: 1080,
                    scale_factor: 1.0,
                    origin_x: 0,
                    origin_y: 0,
                });
            }
        }
    });

    // Native Clipboard Listener -> broadcast Text or Files/Folders to peers
    {
        let (clip_tx, mut clip_rx) = mpsc::channel::<ClipboardContent>(32);
        std::thread::Builder::new()
            .name("p2pkm-clipboard".into())
            .spawn(move || {
                #[cfg(target_os = "windows")]
                if let Err(e) = p2pkm_hal::windows::WinClipboardProvider::run_listener(move |item| {
                    let _ = clip_tx.blocking_send(item);
                }) {
                    error!(?e, "Clipboard listener failed");
                }
                #[cfg(target_os = "macos")]
                p2pkm_hal::macos::MacClipboardProvider::run_listener(move |item| {
                    let _ = clip_tx.blocking_send(item);
                });
            })
            .ok();
        let node_clip = Arc::clone(&node);
        tokio::spawn(async move {
            while let Some(item) = clip_rx.recv().await {
                match item {
                    ClipboardContent::Text(txt) => node_clip.broadcast_clipboard_text(&txt).await,
                    ClipboardContent::Files(paths) => node_clip.broadcast_copied_paths(&paths).await,
                }
            }
        });
    }

    // Lock-free SPSC ring buffer between OS hook thread and async runtime
    let (prod, mut cons) = RingBuffer::<RawInputEvent>::new(4096);
    let hook_ctrl_thread = Arc::clone(&hook_ctrl);
    std::thread::Builder::new()
        .name("p2pkm-os-hook".into())
        .spawn(move || {
            #[cfg(target_os = "windows")]
            if let Err(e) = p2pkm_hal::windows::run_hook_loop(hook_ctrl_thread, prod) {
                error!(?e, "Win32 hook loop failed");
            }
            #[cfg(target_os = "macos")]
            if let Err(e) = p2pkm_hal::macos::run_hook_loop(hook_ctrl_thread, prod) {
                error!(?e, "macOS CGEventTap failed");
            }
        })
        .expect("Failed to spawn OS hook thread");

    let mut was_suppressed = false;
    let mut last_focus_req = std::time::Instant::now() - Duration::from_secs(1);
    let mut outgoing_overshoot: i32 = 0;

    loop {
        tokio::time::sleep(Duration::from_millis(2)).await;
        let is_suppressed = hook_ctrl.is_suppressed();
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
                    info!("Panic hotkey pressed! Returning to LOCAL");
                    outgoing_overshoot = 0;
                    unpark_cursor();
                    node.revoke_focus(1).await;
                }
                RawInputEvent::MouseButton { pressed: true, .. } if !is_suppressed => {
                    node.preempt_if_controlled().await;
                }
                RawInputEvent::MouseMove { dx, dy, abs_x, abs_y } if !is_suppressed => {
                    if dx.abs() + dy.abs() >= 20 {
                        node.preempt_if_controlled().await;
                    }
                    if last_focus_req.elapsed() >= Duration::from_millis(200) {
                        let crossing = desktop.read().await.detect_edge_crossing(
                            peer_id,
                            abs_x - min_x,
                            abs_y - min_y,
                            2.0,
                        );
                        if let Some(c) = crossing {
                            let push = match c.entry_edge {
                                p2pkm_proto::ScreenEdge::Left => dx.max(0),
                                p2pkm_proto::ScreenEdge::Right => (-dx).max(0),
                                p2pkm_proto::ScreenEdge::Top => dy.max(0),
                                p2pkm_proto::ScreenEdge::Bottom => (-dy).max(0),
                            };
                            outgoing_overshoot += push.max(1);
                            let limit = outgoing_overshoot_limit.load(Ordering::Relaxed);
                            if outgoing_overshoot >= limit {
                                outgoing_overshoot = 0;
                                last_focus_req = std::time::Instant::now();
                                let _ = node
                                    .request_focus(c.target_peer, c.entry_edge, c.edge_ratio)
                                    .await;
                            }
                        } else {
                            outgoing_overshoot = 0;
                        }
                    }
                }
                other => {
                    node.send_input_event(other).await;
                }
            }
        }
    }
}
