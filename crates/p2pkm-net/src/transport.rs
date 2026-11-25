use crate::fsm::{FocusDecision, FocusFsm, FocusState};
use p2pkm_engine::{
    map_hid_key_with_custom, sanitize_rel_path, ClipboardLoopGuard, ClipboardPayload, CustomKeyMap,
    KeyMapMode, PeerDisplayNode, DEFAULT_OVERSHOOT_PX, FILE_CHUNK_SIZE,
};
use p2pkm_hal::{ClipboardProvider, HookControl, InputInjector, RawInputEvent};
use p2pkm_proto::{OsKind, Packet, PeerId, RemoteDisplayRect, ScreenEdge};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum NetError {
    #[error("QUIC endpoint error: {0}")]
    Endpoint(String),
    #[error("TLS config error: {0}")]
    Tls(String),
    #[error("peer not connected")]
    PeerNotConnected,
    #[error("connection error: {0}")]
    Connection(String),
}

/// Phase 1: Skip server cert verification (Phase 4 upgrades to Noise / Pinned Ed25519 mTLS).
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

fn make_quic_configs() -> Result<(ServerConfig, ClientConfig), NetError> {
    let cert = rcgen::generate_simple_self_signed(vec!["p2pkm.local".into()])
        .map_err(|e| NetError::Tls(e.to_string()))?;
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .map_err(|e| NetError::Tls(e.to_string()))?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let server_crypto = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| NetError::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| NetError::Tls(e.to_string()))?;

    let mut transport_cfg = quinn::TransportConfig::default();
    transport_cfg.max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()));
    transport_cfg.keep_alive_interval(Some(Duration::from_secs(3)));
    let transport_cfg = Arc::new(transport_cfg);

    let mut server_cfg = ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(server_crypto).map_err(|e| NetError::Tls(e.to_string()))?,
    ));
    server_cfg.transport_config(transport_cfg.clone());

    let client_crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| NetError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    let mut client_cfg = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(client_crypto).map_err(|e| NetError::Tls(e.to_string()))?,
    ));
    client_cfg.transport_config(transport_cfg);

    Ok((server_cfg, client_cfg))
}

#[derive(Clone)]
pub struct PeerSession {
    pub conn: Connection,
    pub os: OsKind,
    pub hostname: String,
    pub logical_w: u32,
    pub logical_h: u32,
    pub physical_w: u32,
    pub physical_h: u32,
    pub displays: Vec<RemoteDisplayRect>,
}

pub struct MeshNode {
    pub peer_id: PeerId,
    pub os: OsKind,
    pub hostname: String,
    pub local_display: RwLock<PeerDisplayNode>,
    local_rects: RwLock<Vec<RemoteDisplayRect>>,
    keymap_mode: Arc<RwLock<KeyMapMode>>,
    custom_keymap: Arc<RwLock<CustomKeyMap>>,
    endpoint: Endpoint,
    peers: Arc<RwLock<HashMap<PeerId, PeerSession>>>,
    known_addrs: Arc<RwLock<HashMap<PeerId, SocketAddr>>>,
    manual_addrs: Arc<RwLock<Vec<SocketAddr>>>,
    fsm: Arc<Mutex<FocusFsm>>,
    hook_ctrl: Arc<HookControl>,
    injector: Arc<dyn InputInjector>,
    clipboard_guard: Arc<Mutex<ClipboardLoopGuard>>,
    clipboard_provider: Option<Arc<dyn ClipboardProvider>>,
    received_files: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
    controlled_entry: Arc<Mutex<Option<(ScreenEdge, bool, i32)>>>,
    last_exit_edge: Arc<Mutex<Option<ScreenEdge>>>,
    return_overshoot_px: AtomicI32,
    seq: AtomicU32,
}

impl MeshNode {
    pub async fn bind(
        bind_addr: SocketAddr,
        peer_id: PeerId,
        os: OsKind,
        hostname: String,
        hook_ctrl: Arc<HookControl>,
        injector: Arc<dyn InputInjector>,
    ) -> Result<Arc<Self>, NetError> {
        let default_display = PeerDisplayNode {
            peer_id,
            hostname: hostname.clone(),
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
        Self::bind_with_options(
            bind_addr,
            peer_id,
            os,
            hostname,
            default_display,
            KeyMapMode::SmartErgonomic,
            hook_ctrl,
            injector,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn bind_with_options(
        bind_addr: SocketAddr,
        peer_id: PeerId,
        os: OsKind,
        hostname: String,
        local_display: PeerDisplayNode,
        keymap_mode: KeyMapMode,
        hook_ctrl: Arc<HookControl>,
        injector: Arc<dyn InputInjector>,
        clipboard_provider: Option<Arc<dyn ClipboardProvider>>,
    ) -> Result<Arc<Self>, NetError> {
        let (server_cfg, client_cfg) = make_quic_configs()?;
        let mut endpoint = Endpoint::server(server_cfg, bind_addr)
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        endpoint.set_default_client_config(client_cfg);

        let node = Arc::new(Self {
            peer_id,
            os,
            hostname,
            local_display: RwLock::new(local_display),
            local_rects: RwLock::new(Vec::new()),
            keymap_mode: Arc::new(RwLock::new(keymap_mode)),
            custom_keymap: Arc::new(RwLock::new(CustomKeyMap::default())),
            endpoint,
            peers: Arc::new(RwLock::new(HashMap::new())),
            known_addrs: Arc::new(RwLock::new(HashMap::new())),
            manual_addrs: Arc::new(RwLock::new(Vec::new())),
            fsm: Arc::new(Mutex::new(FocusFsm::new(
                peer_id,
                Duration::from_millis(500),
            ))),
            hook_ctrl,
            injector,
            clipboard_guard: Arc::new(Mutex::new(ClipboardLoopGuard::new())),
            clipboard_provider,
            received_files: Arc::new(Mutex::new(Vec::new())),
            controlled_entry: Arc::new(Mutex::new(None)),
            last_exit_edge: Arc::new(Mutex::new(None)),
            return_overshoot_px: AtomicI32::new(DEFAULT_OVERSHOOT_PX),
            seq: AtomicU32::new(1),
        });

        node.spawn_accept_loop();
        node.spawn_lease_watchdog();
        node.spawn_reconnect_watchdog();
        Ok(node)
    }

    pub fn set_return_overshoot_px(&self, px: i32) {
        self.return_overshoot_px.store(px.max(10), Ordering::Relaxed);
    }

    pub async fn set_key_map(&self, mode: KeyMapMode, custom: CustomKeyMap) {
        *self.keymap_mode.write().await = mode;
        *self.custom_keymap.write().await = custom;
    }

    pub async fn update_local_display(&self, disp: PeerDisplayNode, rects: Vec<RemoteDisplayRect>) {
        let pkt = Packet::DisplayLayout {
            peer_id: self.peer_id,
            logical_w: disp.logical_w.round().max(1.0) as u32,
            logical_h: disp.logical_h.round().max(1.0) as u32,
            physical_w: disp.physical_w,
            physical_h: disp.physical_h,
            displays: rects.clone(),
        };
        *self.local_display.write().await = disp;
        *self.local_rects.write().await = rects;
        let conns: Vec<Connection> = self
            .peers
            .read()
            .await
            .values()
            .filter(|s| s.conn.close_reason().is_none())
            .map(|s| s.conn.clone())
            .collect();
        for conn in conns {
            let _ = send_reliable(&conn, &pkt).await;
        }
    }

    pub async fn local_display_packet(&self) -> Packet {
        let ld = self.local_display.read().await;
        let rects = self.local_rects.read().await;
        Packet::DisplayLayout {
            peer_id: self.peer_id,
            logical_w: ld.logical_w.round().max(1.0) as u32,
            logical_h: ld.logical_h.round().max(1.0) as u32,
            physical_w: ld.physical_w,
            physical_h: ld.physical_h,
            displays: rects.clone(),
        }
    }

    pub async fn peer_summaries(&self) -> Vec<(PeerId, String, OsKind, SocketAddr)> {
        self.peers
            .read()
            .await
            .iter()
            .filter(|(_, s)| s.conn.close_reason().is_none())
            .map(|(id, s)| (*id, s.hostname.clone(), s.os, s.conn.remote_address()))
            .collect()
    }

    pub async fn peer_display_summaries(
        &self,
    ) -> Vec<(
        PeerId,
        String,
        OsKind,
        SocketAddr,
        u32,
        u32,
        u32,
        u32,
        Vec<RemoteDisplayRect>,
    )> {
        self.peers
            .read()
            .await
            .iter()
            .filter(|(_, s)| s.conn.close_reason().is_none())
            .map(|(id, s)| {
                (
                    *id,
                    s.hostname.clone(),
                    s.os,
                    s.conn.remote_address(),
                    s.logical_w,
                    s.logical_h,
                    s.physical_w,
                    s.physical_h,
                    s.displays.clone(),
                )
            })
            .collect()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint.local_addr().unwrap()
    }

    pub async fn focus_state(&self) -> FocusState {
        self.fsm.lock().await.state.clone()
    }

    pub async fn connected_peers(&self) -> Vec<PeerId> {
        self.peers
            .read()
            .await
            .iter()
            .filter(|(_, s)| s.conn.close_reason().is_none())
            .map(|(id, _)| *id)
            .collect()
    }

    pub async fn register_manual_addr(&self, addr: SocketAddr) {
        let mut guard = self.manual_addrs.write().await;
        if !guard.contains(&addr) {
            guard.push(addr);
        }
    }

    pub async fn close_all_connections(&self) {
        let sessions: Vec<PeerSession> = self.peers.write().await.drain().map(|(_, v)| v).collect();
        for s in sessions {
            s.conn.close(0u32.into(), b"reset");
        }
    }

    /// Actively probes all connected peers with a short-timeout heartbeat stream,
    /// immediately pruning dead peers and resetting focus if the active peer dropped,
    /// then attempts quick reconnects to known/manual addresses.
    pub async fn refresh_peers(self: &Arc<Self>) {
        let snapshot: Vec<(PeerId, Connection)> = self
            .peers
            .read()
            .await
            .iter()
            .map(|(pid, sess)| (*pid, sess.conn.clone()))
            .collect();

        let hb_bytes = Packet::Heartbeat {
            epoch: 0,
            timestamp_us: 0,
        }
        .to_bytes();

        let mut join_set = tokio::task::JoinSet::new();
        for (pid, conn) in snapshot {
            let bytes = hb_bytes.clone();
            join_set.spawn(async move {
                if conn.close_reason().is_some() {
                    return (pid, conn, false);
                }
                let alive = tokio::time::timeout(Duration::from_millis(500), async {
                    let mut send = conn.open_uni().await.ok()?;
                    send.write_all(&bytes).await.ok()?;
                    send.finish().ok()?;
                    send.stopped().await.ok()?;
                    Some(())
                })
                .await
                .ok()
                .flatten()
                .is_some();
                let ok = alive && conn.close_reason().is_none();
                (pid, conn, ok)
            });
        }

        let mut dead_peers = Vec::new();
        while let Some(res) = join_set.join_next().await {
            if let Ok((pid, conn, alive)) = res {
                if !alive {
                    conn.close(0u32.into(), b"refresh-probe-timeout");
                    dead_peers.push(pid);
                }
            }
        }

        if !dead_peers.is_empty() {
            {
                let mut peers = self.peers.write().await;
                for pid in &dead_peers {
                    peers.remove(pid);
                }
            }
            let mut fsm = self.fsm.lock().await;
            let should_reset = match fsm.state {
                FocusState::Controlling { target_peer, .. } => dead_peers.contains(&target_peer),
                FocusState::Controlled { controller_peer, .. } => {
                    dead_peers.contains(&controller_peer)
                }
                FocusState::Local => false,
            };
            if should_reset {
                warn!(?dead_peers, "Refresh detected dead active peer -> resetting focus to LOCAL");
                fsm.force_local();
                *self.controlled_entry.lock().await = None;
                self.hook_ctrl.set_suppressed(false);
                self.injector.release_all_held();
            }
        }

        // Also attempt quick reconnects for known/manual peers that aren't currently connected.
        let mut addrs_to_try = Vec::new();
        {
            let peers = self.peers.read().await;
            let known = self.known_addrs.read().await;
            for (pid, addr) in known.iter() {
                if !peers.contains_key(pid) {
                    addrs_to_try.push(*addr);
                }
            }
            let manual = self.manual_addrs.read().await;
            for addr in manual.iter() {
                if !addrs_to_try.contains(addr) {
                    addrs_to_try.push(*addr);
                }
            }
        }
        let mut reconn_set = tokio::task::JoinSet::new();
        for addr in addrs_to_try {
            let me = Arc::clone(self);
            reconn_set.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_millis(500), me.connect_peer(addr)).await;
            });
        }
        while reconn_set.join_next().await.is_some() {}
    }

    pub async fn connect_peer(self: &Arc<Self>, addr: SocketAddr) -> Result<PeerId, NetError> {
        let connecting = self
            .endpoint
            .connect(addr, "p2pkm.local")
            .map_err(|e| NetError::Connection(e.to_string()))?;
        let conn = connecting
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;

        // Send Hello
        let hello = Packet::Hello {
            peer_id: self.peer_id,
            os: self.os,
            hostname: self.hostname.clone(),
        };
        send_reliable(&conn, &hello).await?;

        // Wait for peer's Hello on first uni stream
        let mut recv = conn
            .accept_uni()
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        let bytes = recv
            .read_to_end(4096)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        let pkt = Packet::decode(&bytes).map_err(|e| NetError::Connection(e.to_string()))?;
        let (remote_id, remote_os, remote_host) = match pkt {
            Packet::Hello {
                peer_id,
                os,
                hostname,
            } => (peer_id, os, hostname),
            _ => return Err(NetError::Connection("expected Hello".into())),
        };

        self.known_addrs.write().await.insert(remote_id, addr);
        self.peers.write().await.insert(
            remote_id,
            PeerSession {
                conn: conn.clone(),
                os: remote_os,
                hostname: remote_host,
                logical_w: 1920,
                logical_h: 1080,
                physical_w: 1920,
                physical_h: 1080,
                displays: Vec::new(),
            },
        );
        self.spawn_peer_loops(remote_id, conn.clone());
        let layout_pkt = self.local_display_packet().await;
        let _ = send_reliable(&conn, &layout_pkt).await;
        Ok(remote_id)
    }

    pub async fn register_peer_addr(&self, peer_id: PeerId, addr: SocketAddr) {
        self.known_addrs.write().await.insert(peer_id, addr);
    }

    pub async fn request_focus(
        &self,
        target_peer: PeerId,
        entry_edge: ScreenEdge,
        edge_ratio: f32,
    ) -> Result<(), NetError> {
        let conn = self
            .peers
            .read()
            .await
            .get(&target_peer)
            .map(|s| s.conn.clone())
            .ok_or(NetError::PeerNotConnected)?;

        let epoch = {
            let mut fsm = self.fsm.lock().await;
            fsm.begin_outgoing_request()
        };
        if let Some(epoch) = epoch {
            *self.last_exit_edge.lock().await = Some(entry_edge.opposite());
            let pkt = Packet::FocusRequest {
                epoch,
                from_peer: self.peer_id,
                entry_edge,
                edge_ratio,
            };
            send_reliable(&conn, &pkt).await?;
        }
        Ok(())
    }

    pub async fn revoke_focus(&self, reason: u8) {
        let (target, epoch) = {
            let mut fsm = self.fsm.lock().await;
            let ep = fsm.epoch;
            let prev = fsm.force_local();
            (prev, ep)
        };
        self.hook_ctrl.set_suppressed(false);
        self.injector.release_all_held();
        if let Some(peer) = target {
            if let Some(sess) = self.peers.read().await.get(&peer).cloned() {
                let _ = send_reliable(&sess.conn, &Packet::FocusRevoke { epoch, reason }).await;
            }
        }
    }

    /// Called when the local user physically moves/clicks their mouse or presses a key
    /// while this machine is `Controlled` by a remote peer: immediately reclaims `Local`
    /// and notifies the remote controller to unpark/stop streaming.
    pub async fn preempt_if_controlled(&self) {
        let is_controlled = matches!(self.fsm.lock().await.state, FocusState::Controlled { .. });
        if is_controlled {
            info!("Local physical input detected while CONTROLLED -> preempting to LOCAL");
            *self.controlled_entry.lock().await = None;
            self.revoke_focus(1).await;
        }
    }

    pub async fn broadcast_clipboard_text(&self, text: &str) {
        let payload = ClipboardPayload::Text(text.to_string());
        let is_new = {
            let mut guard = self.clipboard_guard.lock().await;
            guard.on_local_change(&payload).is_some()
        };
        if !is_new {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let pkt = Packet::ClipboardText {
            seq,
            text: text.to_string(),
        };
        let sessions: Vec<PeerSession> = self.peers.read().await.values().cloned().collect();
        for sess in sessions {
            let _ = send_reliable(&sess.conn, &pkt).await;
        }
    }

    pub fn staging_dir() -> std::path::PathBuf {
        #[cfg(target_os = "windows")]
        {
            std::env::temp_dir().join("p2pkm_drop")
        }
        #[cfg(not(target_os = "windows"))]
        {
            if let Ok(home) = std::env::var("HOME") {
                std::path::PathBuf::from(home).join("Downloads").join("P2P_Drop")
            } else {
                std::env::temp_dir().join("p2pkm_drop")
            }
        }
    }

    /// Streams copied files and recursive folders to all connected peers in 256KB chunks
    /// directly from disk, then notifies them to stage the root paths into their OS clipboard.
    pub async fn broadcast_copied_paths(&self, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let mut root_names = Vec::new();
        let mut files_to_send: Vec<(String, std::path::PathBuf)> = Vec::new();
        let mut offer_items = Vec::new();

        for p in paths {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                root_names.push(name.to_string());
                let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                offer_items.push(p2pkm_engine::FileOfferItem {
                    name: name.to_string(),
                    size,
                    is_dir: p.is_dir(),
                });
                collect_files_recursive(p, name, &mut files_to_send);
            }
        }
        if files_to_send.is_empty() {
            return;
        }

        let payload = ClipboardPayload::FileOffer(offer_items);
        let is_new = {
            let mut guard = self.clipboard_guard.lock().await;
            guard.on_local_change(&payload).is_some()
        };
        if !is_new {
            return;
        }

        let peer_ids: Vec<PeerId> = self.connected_peers().await;
        for pid in peer_ids {
            let mut all_ok = true;
            for (rel_path, full_path) in &files_to_send {
                if let Err(e) = self.send_file_from_disk(pid, rel_path, full_path).await {
                    warn!(?pid, %rel_path, ?e, "Failed to stream file to peer");
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                if let Some(sess) = self.peers.read().await.get(&pid).cloned() {
                    let seq = self.seq.fetch_add(1, Ordering::Relaxed);
                    let _ = send_reliable(
                        &sess.conn,
                        &Packet::ClipboardFilesStaged {
                            seq,
                            root_names: root_names.clone(),
                        },
                    )
                    .await;
                }
            }
        }
    }

    pub async fn send_file_from_disk(
        &self,
        target_peer: PeerId,
        rel_path: &str,
        full_path: &std::path::Path,
    ) -> Result<[u8; 32], NetError> {
        use tokio::io::AsyncReadExt;

        let conn = self
            .peers
            .read()
            .await
            .get(&target_peer)
            .map(|s| s.conn.clone())
            .ok_or(NetError::PeerNotConnected)?;

        let mut file = tokio::fs::File::open(full_path)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        let file_size = file
            .metadata()
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?
            .len();

        // Stream pass 1: compute BLAKE3 hash without loading file into RAM
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; FILE_CHUNK_SIZE];
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| NetError::Connection(e.to_string()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let hash = *hasher.finalize().as_bytes();

        // Stream pass 2: rewind and stream 256KB chunks over QUIC
        use tokio::io::AsyncSeekExt;
        file.seek(std::io::SeekFrom::Start(0))
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;

        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;

        let path_bytes = rel_path.as_bytes();
        let path_len = path_bytes.len().min(u16::MAX as usize) as u16;
        let mut header = Vec::with_capacity(2 + path_len as usize + 8 + 32);
        header.extend_from_slice(&path_len.to_le_bytes());
        header.extend_from_slice(&path_bytes[..path_len as usize]);
        header.extend_from_slice(&file_size.to_le_bytes());
        header.extend_from_slice(&hash);

        send.write_all(&header)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;

        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| NetError::Connection(e.to_string()))?;
            if n == 0 {
                break;
            }
            send.write_all(&buf[..n])
                .await
                .map_err(|e| NetError::Connection(e.to_string()))?;
        }
        send.finish()
            .map_err(|e| NetError::Connection(e.to_string()))?;

        let mut ack = [0u8; 1];
        recv.read_exact(&mut ack)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        if ack[0] != 0x01 {
            return Err(NetError::Connection("file transfer rejected".into()));
        }
        Ok(hash)
    }

    pub async fn take_received_files(&self) -> Vec<(String, Vec<u8>)> {
        let mut guard = self.received_files.lock().await;
        std::mem::take(&mut *guard)
    }

    /// Streams a file over a dedicated QUIC bidirectional stream with 256KB chunks and BLAKE3 verification.
    pub async fn send_file(
        &self,
        target_peer: PeerId,
        rel_path: &str,
        data: &[u8],
    ) -> Result<[u8; 32], NetError> {
        let conn = self
            .peers
            .read()
            .await
            .get(&target_peer)
            .map(|s| s.conn.clone())
            .ok_or(NetError::PeerNotConnected)?;

        let hash = *blake3::hash(data).as_bytes();
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;

        let path_bytes = rel_path.as_bytes();
        let path_len = path_bytes.len().min(u16::MAX as usize) as u16;
        let mut header = Vec::with_capacity(2 + path_len as usize + 8 + 32);
        header.extend_from_slice(&path_len.to_le_bytes());
        header.extend_from_slice(&path_bytes[..path_len as usize]);
        header.extend_from_slice(&(data.len() as u64).to_le_bytes());
        header.extend_from_slice(&hash);

        send.write_all(&header)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;

        for chunk in data.chunks(FILE_CHUNK_SIZE) {
            send.write_all(chunk)
                .await
                .map_err(|e| NetError::Connection(e.to_string()))?;
        }
        send.finish()
            .map_err(|e| NetError::Connection(e.to_string()))?;

        let mut ack = [0u8; 1];
        recv.read_exact(&mut ack)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        if ack[0] != 0x01 {
            return Err(NetError::Connection("file transfer rejected or checksum failed".into()));
        }
        Ok(hash)
    }

    pub async fn send_input_event(&self, ev: RawInputEvent) {
        if matches!(ev, RawInputEvent::PanicHotkey) {
            self.revoke_focus(1).await;
            return;
        }
        let target = {
            let fsm = self.fsm.lock().await;
            match fsm.state {
                FocusState::Controlling { target_peer, .. } => Some(target_peer),
                _ => None,
            }
        };
        let Some(target) = target else { return };
        let Some(sess) = self.peers.read().await.get(&target).cloned() else {
            return;
        };
        let conn = sess.conn;

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        match ev {
            RawInputEvent::MouseMove { dx, dy, .. } => {
                let pkt = Packet::MouseMove {
                    seq,
                    dx: dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    dy: dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    timestamp_us: 0,
                };
                let _ = conn.send_datagram(pkt.to_bytes());
            }
            RawInputEvent::MouseButton { button, pressed } => {
                let _ = send_reliable(&conn, &Packet::MouseButton { seq, button, pressed }).await;
            }
            RawInputEvent::MouseScroll { dx, dy } => {
                let pkt = Packet::MouseScroll {
                    seq,
                    dx: dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    dy: dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                };
                let _ = send_reliable(&conn, &pkt).await;
            }
            RawInputEvent::Key { hid_usage, pressed } => {
                let _ = send_reliable(
                    &conn,
                    &Packet::KeyEvent {
                        seq,
                        hid_usage,
                        pressed,
                    },
                )
                .await;
            }
            RawInputEvent::PanicHotkey => {}
        }
    }

    fn spawn_accept_loop(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(incoming) = me.endpoint.accept().await {
                let node = Arc::clone(&me);
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    // Read Hello
                    let Ok(mut recv) = conn.accept_uni().await else { return };
                    let Ok(bytes) = recv.read_to_end(4096).await else { return };
                    let Ok(Packet::Hello { peer_id, os, hostname }) = Packet::decode(&bytes) else {
                        return;
                    };
                    info!(?peer_id, %hostname, "Accepted QUIC peer");
                    // Reply with our Hello
                    let hello = Packet::Hello {
                        peer_id: node.peer_id,
                        os: node.os,
                        hostname: node.hostname.clone(),
                    };
                    if send_reliable(&conn, &hello).await.is_err() {
                        return;
                    }
                    node.peers.write().await.insert(
                        peer_id,
                        PeerSession {
                            conn: conn.clone(),
                            os,
                            hostname,
                            logical_w: 1920,
                            logical_h: 1080,
                            physical_w: 1920,
                            physical_h: 1080,
                            displays: Vec::new(),
                        },
                    );
                    node.spawn_peer_loops(peer_id, conn.clone());
                    let layout_pkt = node.local_display_packet().await;
                    let _ = send_reliable(&conn, &layout_pkt).await;
                });
            }
        });
    }

    fn spawn_peer_loops(self: &Arc<Self>, remote_id: PeerId, conn: Connection) {
        // Datagram loop (MouseMove, Heartbeat)
        let me_dgram = Arc::clone(self);
        let conn_dgram = conn.clone();
        tokio::spawn(async move {
            while let Ok(bytes) = conn_dgram.read_datagram().await {
                if let Ok(pkt) = Packet::decode(&bytes) {
                    me_dgram.handle_packet(remote_id, &conn_dgram, pkt).await;
                }
            }
        });

        // Reliable uni-stream loop (Focus, Keys, Buttons, Scroll, Clipboard)
        let me_stream = Arc::clone(self);
        let conn_stream = conn.clone();
        tokio::spawn(async move {
            while let Ok(mut recv) = conn_stream.accept_uni().await {
                if let Ok(bytes) = recv.read_to_end(1024 * 1024).await {
                    if let Ok(pkt) = Packet::decode(&bytes) {
                        me_stream.handle_packet(remote_id, &conn_stream, pkt).await;
                    }
                }
            }
            debug!(?remote_id, "Peer stream loop closed");
            me_stream.peers.write().await.remove(&remote_id);
        });

        // Bidirectional stream loop (Chunked File Transfers)
        let me_bi = Arc::clone(self);
        let conn_bi = conn;
        tokio::spawn(async move {
            while let Ok((mut send, mut recv)) = conn_bi.accept_bi().await {
                let node = Arc::clone(&me_bi);
                tokio::spawn(async move {
                    let mut len_buf = [0u8; 2];
                    if recv.read_exact(&mut len_buf).await.is_err() {
                        return;
                    }
                    let path_len = u16::from_le_bytes(len_buf) as usize;
                    let mut path_buf = vec![0u8; path_len];
                    if recv.read_exact(&mut path_buf).await.is_err() {
                        return;
                    }
                    let Ok(rel_str) = std::str::from_utf8(&path_buf) else {
                        let _ = send.write_all(&[0x00]).await;
                        return;
                    };
                    let Ok(clean_path) = sanitize_rel_path(rel_str) else {
                        let _ = send.write_all(&[0x00]).await;
                        return;
                    };
                    let mut size_buf = [0u8; 8];
                    let mut hash_buf = [0u8; 32];
                    if recv.read_exact(&mut size_buf).await.is_err()
                        || recv.read_exact(&mut hash_buf).await.is_err()
                    {
                        return;
                    }
                    let file_size = u64::from_le_bytes(size_buf) as usize;
                    let dest_path = Self::staging_dir().join(&clean_path);
                    if let Some(parent) = dest_path.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    let Ok(mut out_file) = tokio::fs::File::create(&dest_path).await else {
                        let _ = send.write_all(&[0x00]).await;
                        return;
                    };

                    use tokio::io::AsyncWriteExt;
                    let mut hasher = blake3::Hasher::new();
                    let mut small_buf = if file_size <= 4 * 1024 * 1024 {
                        Some(Vec::with_capacity(file_size))
                    } else {
                        None
                    };
                    let mut buf = vec![0u8; FILE_CHUNK_SIZE];
                    loop {
                        match recv.read(&mut buf).await {
                            Ok(Some(n)) => {
                                hasher.update(&buf[..n]);
                                if out_file.write_all(&buf[..n]).await.is_err() {
                                    let _ = send.write_all(&[0x00]).await;
                                    return;
                                }
                                if let Some(sb) = small_buf.as_mut() {
                                    sb.extend_from_slice(&buf[..n]);
                                }
                            }
                            Ok(None) => break,
                            Err(_) => return,
                        }
                    }
                    let _ = out_file.flush().await;
                    let actual_hash = *hasher.finalize().as_bytes();
                    if actual_hash == hash_buf {
                        if let Some(data) = small_buf {
                            node.received_files
                                .lock()
                                .await
                                .push((clean_path.to_string_lossy().into_owned(), data));
                        }
                        let _ = send.write_all(&[0x01]).await;
                        let _ = send.finish();
                    } else {
                        let _ = tokio::fs::remove_file(&dest_path).await;
                        let _ = send.write_all(&[0x00]).await;
                        let _ = send.finish();
                    }
                });
            }
        });
    }

    fn spawn_lease_watchdog(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                let (is_controlling, target, epoch) = {
                    let mut fsm = me.fsm.lock().await;
                    if fsm.check_lease_timeout(Instant::now()) {
                        warn!("Lease/Heartbeat timed out -> reclaiming LOCAL control & releasing held keys");
                        me.hook_ctrl.set_suppressed(false);
                        me.injector.release_all_held();
                        if let Ok(mut guard) = me.controlled_entry.try_lock() {
                            *guard = None;
                        }
                    }
                    match fsm.state {
                        FocusState::Controlling {
                            target_peer, epoch, ..
                        } => (true, Some(target_peer), epoch),
                        _ => (false, None, fsm.epoch),
                    }
                };
                if is_controlling {
                    if let Some(peer) = target {
                        if let Some(sess) = me.peers.read().await.get(&peer).cloned() {
                            let hb = Packet::Heartbeat {
                                epoch,
                                timestamp_us: 0,
                            };
                            let _ = sess.conn.send_datagram(hb.to_bytes());
                        }
                    }
                }
            }
        });
    }

    /// Periodically checks connection health every 2 seconds, cleans up dead sessions,
    /// resets focus if the active peer dropped, and auto-reconnects to known peers.
    fn spawn_reconnect_watchdog(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                interval.tick().await;

                // 1. Prune closed connections
                let mut dead_peers = Vec::new();
                {
                    let mut peers = me.peers.write().await;
                    peers.retain(|pid, sess| {
                        if sess.conn.close_reason().is_some() {
                            dead_peers.push(*pid);
                            false
                        } else {
                            true
                        }
                    });
                }

                if !dead_peers.is_empty() {
                    let mut fsm = me.fsm.lock().await;
                    let should_reset = match fsm.state {
                        FocusState::Controlling { target_peer, .. } => {
                            dead_peers.contains(&target_peer)
                        }
                        FocusState::Controlled {
                            controller_peer, ..
                        } => dead_peers.contains(&controller_peer),
                        FocusState::Local => false,
                    };
                    if should_reset {
                        warn!(?dead_peers, "Active peer disconnected -> resetting focus to LOCAL");
                        fsm.force_local();
                        *me.controlled_entry.lock().await = None;
                        me.hook_ctrl.set_suppressed(false);
                        me.injector.release_all_held();
                    }
                }

                // 2. Reconnect to any known peer that is currently disconnected
                let to_reconnect: Vec<(PeerId, SocketAddr)> = {
                    let peers = me.peers.read().await;
                    let known = me.known_addrs.read().await;
                    known
                        .iter()
                        .filter(|(pid, _)| !peers.contains_key(*pid))
                        .map(|(pid, addr)| (*pid, *addr))
                        .collect()
                };

                for (pid, addr) in to_reconnect {
                    let node = Arc::clone(&me);
                    tokio::spawn(async move {
                        debug!(?pid, %addr, "Attempting auto-reconnect to peer");
                        match tokio::time::timeout(Duration::from_secs(2), node.connect_peer(addr))
                            .await
                        {
                            Ok(Ok(id)) => info!(?id, %addr, "Auto-reconnected to peer"),
                            Ok(Err(e)) => debug!(?pid, %addr, ?e, "Auto-reconnect failed"),
                            Err(_) => debug!(?pid, %addr, "Auto-reconnect timed out"),
                        }
                    });
                }

                // 3. Retry manual --peer addresses if not yet connected
                let manual: Vec<SocketAddr> = me.manual_addrs.read().await.clone();
                if !manual.is_empty() && me.peers.read().await.is_empty() {
                    for addr in manual {
                        let node = Arc::clone(&me);
                        tokio::spawn(async move {
                            let _ = tokio::time::timeout(
                                Duration::from_secs(2),
                                node.connect_peer(addr),
                            )
                            .await;
                        });
                    }
                }
            }
        });
    }

    async fn handle_packet(&self, remote_id: PeerId, conn: &Connection, pkt: Packet) {
        match pkt {
            Packet::FocusRequest {
                epoch,
                from_peer,
                entry_edge,
                edge_ratio,
            } => {
                let decision = {
                    let mut fsm = self.fsm.lock().await;
                    fsm.on_incoming_focus_request(from_peer, epoch, Instant::now())
                };
                match decision {
                    FocusDecision::Granted { epoch } => {
                        info!(?from_peer, ?entry_edge, edge_ratio, "Focus granted -> CONTROLLED");
                        self.hook_ctrl.set_suppressed(false);
                        *self.controlled_entry.lock().await = Some((entry_edge, false, 0));
                        let (wx, wy) = self
                            .local_display
                            .read()
                            .await
                            .entry_to_physical(entry_edge, edge_ratio);
                        self.injector.inject_mouse_abs(wx, wy);
                        let _ = send_reliable(
                            conn,
                            &Packet::FocusAck {
                                epoch,
                                target_peer: self.peer_id,
                            },
                        )
                        .await;
                    }
                    FocusDecision::Rejected {
                        current_controller,
                        epoch,
                    } => {
                        let _ = send_reliable(
                            conn,
                            &Packet::FocusReject {
                                epoch,
                                current_controller,
                            },
                        )
                        .await;
                    }
                }
            }
            Packet::FocusAck { epoch, target_peer } => {
                let granted = self.fsm.lock().await.on_focus_ack(target_peer, epoch);
                if granted {
                    info!(?target_peer, "Focus ack -> CONTROLLING");
                    self.injector.release_all_held();
                    self.hook_ctrl.set_suppressed(true);
                }
            }
            Packet::FocusReject { .. } | Packet::FocusRevoke { .. } => {
                info!("Focus revoked/rejected -> LOCAL");
                self.fsm.lock().await.force_local();
                *self.controlled_entry.lock().await = None;
                self.hook_ctrl.set_suppressed(false);
                self.injector.release_all_held();
                if let Some(exit_edge) = self.last_exit_edge.lock().await.take() {
                    let (min_x, min_y, max_x, max_y) =
                        self.local_display.read().await.physical_bounds();
                    let mid_x = (min_x + max_x) / 2;
                    let mid_y = (min_y + max_y) / 2;
                    let (rx, ry) = match exit_edge {
                        ScreenEdge::Right => ((max_x - 23).max(min_x), mid_y),
                        ScreenEdge::Left => ((min_x + 24).min(max_x), mid_y),
                        ScreenEdge::Bottom => (mid_x, (max_y - 23).max(min_y)),
                        ScreenEdge::Top => (mid_x, (min_y + 24).min(max_y)),
                    };
                    self.injector.inject_mouse_abs(rx, ry);
                }
            }
            Packet::DisplayLayout {
                peer_id: _,
                logical_w,
                logical_h,
                physical_w,
                physical_h,
                displays,
            } => {
                if let Some(sess) = self.peers.write().await.get_mut(&remote_id) {
                    sess.logical_w = logical_w.max(1);
                    sess.logical_h = logical_h.max(1);
                    sess.physical_w = physical_w.max(1);
                    sess.physical_h = physical_h.max(1);
                    sess.displays = displays;
                }
            }
            Packet::Heartbeat { epoch, timestamp_us } => {
                let now = Instant::now();
                let is_controlled = {
                    let mut fsm = self.fsm.lock().await;
                    if fsm.refresh_controlled_activity(remote_id, now) {
                        true
                    } else {
                        fsm.refresh_controlling_ack(remote_id, now);
                        false
                    }
                };
                if is_controlled {
                    let ack = Packet::Heartbeat {
                        epoch,
                        timestamp_us,
                    };
                    let _ = conn.send_datagram(ack.to_bytes());
                }
            }
            Packet::MouseMove { dx, dy, .. } => {
                let active = self
                    .fsm
                    .lock()
                    .await
                    .refresh_controlled_activity(remote_id, Instant::now());
                if active {
                    let prev_pos = self.injector.cursor_pos();
                    self.injector.inject_mouse_move(dx as i32, dy as i32);
                    if let Some((cx, cy)) = self.injector.cursor_pos() {
                        let return_overshoot_threshold =
                            self.return_overshoot_px.load(Ordering::Relaxed);
                        let (px, py) = prev_pos.unwrap_or((cx, cy));
                        let (min_x, min_y, max_x, max_y) =
                            self.local_display.read().await.physical_bounds();
                        let mut should_return = false;
                        {
                            let mut entry_guard = self.controlled_entry.lock().await;
                            if let Some((entry_edge, left_zone, overshoot)) = entry_guard.as_mut() {
                                if !*left_zone {
                                    *left_zone = match entry_edge {
                                        ScreenEdge::Left => cx > min_x + 24,
                                        ScreenEdge::Right => cx < max_x - 24,
                                        ScreenEdge::Top => cy > min_y + 24,
                                        ScreenEdge::Bottom => cy < max_y - 24,
                                    };
                                } else {
                                    match entry_edge {
                                        ScreenEdge::Left if cx <= min_x + 1 && dx < 0 => {
                                            let excess = (min_x - (px + dx as i32)).max(0);
                                            *overshoot += excess;
                                        }
                                        ScreenEdge::Right if cx >= max_x - 1 && dx > 0 => {
                                            let excess = ((px + dx as i32) - max_x).max(0);
                                            *overshoot += excess;
                                        }
                                        ScreenEdge::Top if cy <= min_y + 1 && dy < 0 => {
                                            let excess = (min_y - (py + dy as i32)).max(0);
                                            *overshoot += excess;
                                        }
                                        ScreenEdge::Bottom if cy >= max_y - 1 && dy > 0 => {
                                            let excess = ((py + dy as i32) - max_y).max(0);
                                            *overshoot += excess;
                                        }
                                        _ => {
                                            let away = match entry_edge {
                                                ScreenEdge::Left => cx > min_x + 4,
                                                ScreenEdge::Right => cx < max_x - 4,
                                                ScreenEdge::Top => cy > min_y + 4,
                                                ScreenEdge::Bottom => cy < max_y - 4,
                                            };
                                            if away {
                                                *overshoot = 0;
                                            }
                                        }
                                    }
                                    should_return = *overshoot >= return_overshoot_threshold;
                                }
                                if should_return {
                                    *entry_guard = None;
                                }
                            }
                        }
                        if should_return {
                            info!(?remote_id, "Cursor hit return edge -> auto-returning focus to controller");
                            let epoch = {
                                let mut fsm = self.fsm.lock().await;
                                let ep = fsm.epoch;
                                fsm.force_local();
                                ep
                            };
                            self.injector.release_all_held();
                            let _ = send_reliable(conn, &Packet::FocusRevoke { epoch, reason: 0 }).await;
                        }
                    }
                }
            }
            Packet::MouseButton {
                button, pressed, ..
            } => {
                let active = self
                    .fsm
                    .lock()
                    .await
                    .refresh_controlled_activity(remote_id, Instant::now());
                if active {
                    self.injector.inject_mouse_button(button, pressed);
                }
            }
            Packet::MouseScroll { dx, dy, .. } => {
                let active = self
                    .fsm
                    .lock()
                    .await
                    .refresh_controlled_activity(remote_id, Instant::now());
                if active {
                    self.injector.inject_mouse_scroll(dx as i32, dy as i32);
                }
            }
            Packet::KeyEvent {
                hid_usage, pressed, ..
            } => {
                let active = self
                    .fsm
                    .lock()
                    .await
                    .refresh_controlled_activity(remote_id, Instant::now());
                if active {
                    let source_os = self
                        .peers
                        .read()
                        .await
                        .get(&remote_id)
                        .map(|s| s.os)
                        .unwrap_or(self.os);
                    let mode = *self.keymap_mode.read().await;
                    let custom = self.custom_keymap.read().await;
                    let mapped =
                        map_hid_key_with_custom(hid_usage, source_os, self.os, mode, &custom);
                    self.injector.inject_key(mapped, pressed);
                }
            }
            Packet::ClipboardText { text, .. } => {
                let payload = ClipboardPayload::Text(text.clone());
                let apply = {
                    let mut guard = self.clipboard_guard.lock().await;
                    guard.on_remote_receive(remote_id, &payload)
                };
                if apply {
                    if let Some(cp) = &self.clipboard_provider {
                        cp.set_text(&text);
                    }
                }
            }
            Packet::ClipboardFilesStaged { root_names, .. } => {
                let mut staged_paths = Vec::new();
                let mut offer_items = Vec::new();
                let base = Self::staging_dir();
                for name in root_names {
                    if let Ok(clean) = sanitize_rel_path(&name) {
                        let p = base.join(clean);
                        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                        offer_items.push(p2pkm_engine::FileOfferItem {
                            name,
                            size,
                            is_dir: p.is_dir(),
                        });
                        staged_paths.push(p);
                    }
                }
                let payload = ClipboardPayload::FileOffer(offer_items);
                let apply = {
                    let mut guard = self.clipboard_guard.lock().await;
                    guard.on_remote_receive(remote_id, &payload)
                };
                if apply && !staged_paths.is_empty() {
                    info!(count = staged_paths.len(), "Staged remote files into OS clipboard");
                    if let Some(cp) = &self.clipboard_provider {
                        cp.set_files(&staged_paths);
                    }
                }
            }
            Packet::Hello { .. } => {}
        }
    }
}

fn collect_files_recursive(
    path: &std::path::Path,
    rel_prefix: &str,
    out: &mut Vec<(String, std::path::PathBuf)>,
) {
    if path.is_file() {
        out.push((rel_prefix.replace('\\', "/"), path.to_path_buf()));
    } else if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let ep = entry.path();
                if let Some(name) = ep.file_name().and_then(|n| n.to_str()) {
                    let child_rel = format!("{rel_prefix}/{name}");
                    collect_files_recursive(&ep, &child_rel, out);
                }
            }
        }
    }
}

async fn send_reliable(conn: &Connection, pkt: &Packet) -> Result<(), NetError> {
    let bytes = pkt.to_bytes();
    tokio::time::timeout(Duration::from_millis(400), async {
        let mut send = conn
            .open_uni()
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        send.write_all(&bytes)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        send.finish()
            .map_err(|e| NetError::Connection(e.to_string()))?;
        Ok(())
    })
    .await
    .map_err(|_| NetError::Connection("send_reliable timed out".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2pkm_hal::MockInputHal;
    use p2pkm_proto::MouseButton;

    #[tokio::test]
    async fn mesh_nodes_transfer_focus_and_stream_input_over_quic() {
        let peer_a = [0xAAu8; 16];
        let peer_b = [0xBBu8; 16];

        let ctrl_a = HookControl::new();
        let inj_a = MockInputHal::new();
        let ctrl_b = HookControl::new();
        let inj_b = MockInputHal::new();

        let node_a = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_a,
            OsKind::Windows,
            "NODE-A".into(),
            ctrl_a.clone(),
            inj_a,
        )
        .await
        .unwrap();

        let node_b = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_b,
            OsKind::MacOS,
            "NODE-B".into(),
            ctrl_b.clone(),
            inj_b.clone(),
        )
        .await
        .unwrap();

        let connected_id = node_a.connect_peer(node_b.local_addr()).await.unwrap();
        assert_eq!(connected_id, peer_b);

        // Give accept loop a moment to register peer_a on node_b
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(node_b.connected_peers().await, vec![peer_a]);

        // Node A requests focus on Node B
        node_a
            .request_focus(peer_b, ScreenEdge::Left, 0.5)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(matches!(
            node_a.focus_state().await,
            FocusState::Controlling { target_peer, .. } if target_peer == peer_b
        ));
        assert!(ctrl_a.is_suppressed());
        assert!(matches!(
            node_b.focus_state().await,
            FocusState::Controlled { controller_peer, .. } if controller_peer == peer_a
        ));

        // Stream MouseMove (datagram) + KeyEvent + MouseButton (reliable stream)
        node_a
            .send_input_event(RawInputEvent::MouseMove {
                dx: 12,
                dy: -8,
                abs_x: 0,
                abs_y: 0,
            })
            .await;
        node_a
            .send_input_event(RawInputEvent::Key {
                hid_usage: 0x04,
                pressed: true,
            })
            .await;
        node_a
            .send_input_event(RawInputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        let received = inj_b.take_events();
        assert!(
            received.contains(&RawInputEvent::MouseMove {
                dx: 12,
                dy: -8,
                abs_x: 0,
                abs_y: 0,
            }),
            "MouseMove datagram must arrive on Node B: {received:?}"
        );
        assert!(received.contains(&RawInputEvent::Key {
            hid_usage: 0x04,
            pressed: true,
        }));
        assert!(received.contains(&RawInputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        }));

        // Revoke focus -> Node B releases held keys & returns to Local
        node_a.revoke_focus(0).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(node_a.focus_state().await, FocusState::Local);
        assert_eq!(node_b.focus_state().await, FocusState::Local);
        assert!(!ctrl_a.is_suppressed());
        assert_eq!(*inj_b.release_all_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn cross_os_key_mapping_and_entry_warp_over_quic() {
        use p2pkm_hal::MockClipboard;
        let peer_a = [0x11u8; 16];
        let peer_b = [0x22u8; 16];

        let disp_a = PeerDisplayNode {
            peer_id: peer_a,
            hostname: "WIN".into(),
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
        let disp_b = PeerDisplayNode {
            peer_id: peer_b,
            hostname: "MAC".into(),
            canvas_x: 1920.0,
            canvas_y: 0.0,
            logical_w: 1512.0,
            logical_h: 982.0,
            physical_w: 1512,
            physical_h: 982,
            scale_factor: 1.0,
            origin_x: 0,
            origin_y: 0,
        };

        let inj_b = MockInputHal::new();
        let clip_b = MockClipboard::new();

        let node_a = MeshNode::bind_with_options(
            "127.0.0.1:0".parse().unwrap(),
            peer_a,
            OsKind::Windows,
            "WIN".into(),
            disp_a,
            KeyMapMode::SmartErgonomic,
            HookControl::new(),
            MockInputHal::new(),
            Some(MockClipboard::new()),
        )
        .await
        .unwrap();

        let node_b = MeshNode::bind_with_options(
            "127.0.0.1:0".parse().unwrap(),
            peer_b,
            OsKind::MacOS,
            "MAC".into(),
            disp_b,
            KeyMapMode::SmartErgonomic,
            HookControl::new(),
            inj_b.clone(),
            Some(clip_b.clone()),
        )
        .await
        .unwrap();

        node_a.connect_peer(node_b.local_addr()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Focus transfer at 50% height -> Node B should warp cursor to (1, 490)
        node_a
            .request_focus(peer_b, ScreenEdge::Left, 0.5)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send Windows L-Ctrl (0xE0) -> Node B (macOS) should inject L-Cmd (0xE3)
        node_a
            .send_input_event(RawInputEvent::Key {
                hid_usage: 0xE0,
                pressed: true,
            })
            .await;

        // Clipboard sync test
        node_a.broadcast_clipboard_text("P2P Clipboard Sync").await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let evs = inj_b.take_events();
        assert!(evs.contains(&RawInputEvent::MouseMove {
            dx: 0,
            dy: 0,
            abs_x: 1,
            abs_y: 490,
        }));
        assert!(evs.contains(&RawInputEvent::Key {
            hid_usage: 0xE3,
            pressed: true,
        }));
        assert_eq!(clip_b.get_text().as_deref(), Some("P2P Clipboard Sync"));
    }

    #[tokio::test]
    async fn chunked_file_transfer_over_quic_stream() {
        let peer_a = [0x33u8; 16];
        let peer_b = [0x44u8; 16];

        let node_a = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_a,
            OsKind::Windows,
            "WIN".into(),
            HookControl::new(),
            MockInputHal::new(),
        )
        .await
        .unwrap();

        let node_b = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_b,
            OsKind::MacOS,
            "MAC".into(),
            HookControl::new(),
            MockInputHal::new(),
        )
        .await
        .unwrap();

        node_a.connect_peer(node_b.local_addr()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let payload = vec![0x7Au8; 600 * 1024]; // 600 KB -> multiple 256KB chunks
        let sent_hash = node_a
            .send_file(peer_b, "docs/design.bin", &payload)
            .await
            .expect("chunked file transfer must succeed");
        assert_eq!(sent_hash, *blake3::hash(&payload).as_bytes());

        // Path traversal must be rejected by receiver
        assert!(node_a
            .send_file(peer_b, "../evil.txt", b"bad")
            .await
            .is_err());

        let files = node_b.take_received_files().await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, std::path::Path::new("docs").join("design.bin").to_string_lossy());
        assert_eq!(files[0].1, payload);
    }

    #[tokio::test]
    async fn bidirectional_return_edge_crossing_over_quic() {
        let peer_a = [0x55u8; 16];
        let peer_b = [0x66u8; 16];
        let ctrl_a = HookControl::new();

        let node_a = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_a,
            OsKind::Windows,
            "WIN".into(),
            ctrl_a.clone(),
            MockInputHal::new(),
        )
        .await
        .unwrap();

        let node_b = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_b,
            OsKind::MacOS,
            "MAC".into(),
            HookControl::new(),
            MockInputHal::new(),
        )
        .await
        .unwrap();

        node_a.connect_peer(node_b.local_addr()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 1. Enter Node B's Left edge
        node_a
            .request_focus(peer_b, ScreenEdge::Left, 0.5)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(ctrl_a.is_suppressed());

        // 2. Move right (+60px) so Node B leaves the 24px entry zone
        node_a
            .send_input_event(RawInputEvent::MouseMove {
                dx: 60,
                dy: 0,
                abs_x: 0,
                abs_y: 0,
            })
            .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(ctrl_a.is_suppressed());

        // 3. Move left (-70px) to hit x=0 (only 9px overshoot < 100px default -> stays on B)
        node_a
            .send_input_event(RawInputEvent::MouseMove {
                dx: -70,
                dy: 0,
                abs_x: 0,
                abs_y: 0,
            })
            .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(ctrl_a.is_suppressed());

        // 4. Push further negative past 0 (-100px) -> exceeds 100px overshoot -> auto-return to Node A!
        node_a
            .send_input_event(RawInputEvent::MouseMove {
                dx: -100,
                dy: 0,
                abs_x: 0,
                abs_y: 0,
            })
            .await;
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(node_b.focus_state().await, FocusState::Local);
        assert_eq!(node_a.focus_state().await, FocusState::Local);
        assert!(!ctrl_a.is_suppressed());
    }

    #[tokio::test]
    async fn auto_reconnects_after_connection_drop() {
        let peer_a = [0x77u8; 16];
        let peer_b = [0x88u8; 16];

        let node_a = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_a,
            OsKind::Windows,
            "WIN".into(),
            HookControl::new(),
            MockInputHal::new(),
        )
        .await
        .unwrap();

        let node_b = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_b,
            OsKind::MacOS,
            "MAC".into(),
            HookControl::new(),
            MockInputHal::new(),
        )
        .await
        .unwrap();

        node_a.connect_peer(node_b.local_addr()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(node_a.connected_peers().await, vec![peer_b]);

        // Simulate abrupt connection drop / sleep
        node_a.close_all_connections().await;
        assert!(node_a.connected_peers().await.is_empty());

        // Watchdog runs every 2s -> should auto-reconnect within 3s
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(node_a.connected_peers().await, vec![peer_b]);
    }

    #[tokio::test]
    async fn controller_reclaims_local_control_when_target_dies() {
        let peer_a = [0x91u8; 16];
        let peer_b = [0x92u8; 16];
        let ctrl_a = HookControl::new();

        let node_a = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_a,
            OsKind::Windows,
            "WIN".into(),
            ctrl_a.clone(),
            MockInputHal::new(),
        )
        .await
        .unwrap();

        let node_b = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_b,
            OsKind::MacOS,
            "MAC".into(),
            HookControl::new(),
            MockInputHal::new(),
        )
        .await
        .unwrap();

        node_a.connect_peer(node_b.local_addr()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        node_a
            .request_focus(peer_b, ScreenEdge::Left, 0.5)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(ctrl_a.is_suppressed());

        // Simulate client (Node B) dying / hanging abruptly without sending FocusRevoke
        node_b.close_all_connections().await;
        node_b.endpoint.close(0u32.into(), b"crash");

        // Controller (Node A) must reclaim local control within ~1s (< 800ms lease + tick)
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(node_a.focus_state().await, FocusState::Local);
        assert!(!ctrl_a.is_suppressed());
    }

    #[tokio::test]
    async fn copied_files_and_folders_sync_to_target_clipboard_over_quic() {
        use p2pkm_hal::MockClipboard;
        let peer_a = [0x93u8; 16];
        let peer_b = [0x94u8; 16];

        let clip_b = MockClipboard::new();
        let node_a = MeshNode::bind(
            "127.0.0.1:0".parse().unwrap(),
            peer_a,
            OsKind::Windows,
            "WIN".into(),
            HookControl::new(),
            MockInputHal::new(),
        )
        .await
        .unwrap();

        let disp_b = PeerDisplayNode {
            peer_id: peer_b,
            hostname: "MAC".into(),
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
        let node_b = MeshNode::bind_with_options(
            "127.0.0.1:0".parse().unwrap(),
            peer_b,
            OsKind::MacOS,
            "MAC".into(),
            disp_b,
            KeyMapMode::SmartErgonomic,
            HookControl::new(),
            MockInputHal::new(),
            Some(clip_b.clone()),
        )
        .await
        .unwrap();

        node_a.connect_peer(node_b.local_addr()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Create a temporary source folder with a nested file
        let src_root = std::env::temp_dir().join("p2pkm_test_src_folder");
        let _ = std::fs::remove_dir_all(&src_root);
        std::fs::create_dir_all(src_root.join("sub")).unwrap();
        std::fs::write(src_root.join("sub").join("hello.txt"), b"hello cross-os!").unwrap();

        node_a.broadcast_copied_paths(&[src_root.clone()]).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let staged = clip_b.get_files().expect("target clipboard must have staged folder");
        assert_eq!(staged.len(), 1);
        let nested = staged[0].join("sub").join("hello.txt");
        assert_eq!(std::fs::read(&nested).unwrap(), b"hello cross-os!");

        let _ = std::fs::remove_dir_all(&src_root);
    }
}
