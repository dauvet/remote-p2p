use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use p2pkm_proto::{OsKind, PeerId};
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

pub const MDNS_SERVICE_TYPE: &str = "_p2pkm._udp.local.";

#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub peer_id: PeerId,
    pub hostname: String,
    pub os: OsKind,
    pub addr: SocketAddr,
}

pub struct MdnsDiscovery {
    daemon: ServiceDaemon,
}

impl MdnsDiscovery {
    pub fn start(
        local_peer_id: PeerId,
        hostname: &str,
        os: OsKind,
        port: u16,
    ) -> Result<(Self, mpsc::Receiver<DiscoveredPeer>), mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let peer_hex = Uuid::from_bytes(local_peer_id).simple().to_string();
        let instance_name = format!("p2pkm-{}", &peer_hex[..8]);
        let host_label = format!("{instance_name}.local.");

        let mut props = HashMap::new();
        props.insert("peer_id".to_string(), peer_hex.clone());
        props.insert("os".to_string(), format!("{}", os as u8));
        props.insert("hostname".to_string(), hostname.to_string());

        let service_info = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &instance_name,
            &host_label,
            "",
            port,
            props,
        )?
        .enable_addr_auto();

        daemon.register(service_info)?;

        let receiver = daemon.browse(MDNS_SERVICE_TYPE)?;
        let (tx, rx) = mpsc::channel(32);

        std::thread::Builder::new()
            .name("p2pkm-mdns-browse".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    if let ServiceEvent::ServiceResolved(info) = event {
                        let Some(pid_str) = info.get_property_val_str("peer_id") else {
                            continue;
                        };
                        let Ok(uuid) = Uuid::parse_str(pid_str) else {
                            continue;
                        };
                        let peer_id = *uuid.as_bytes();
                        if peer_id == local_peer_id {
                            continue;
                        }
                        let Some(ip) = info.get_addresses().iter().find(|a| a.is_ipv4()).copied()
                            .or_else(|| info.get_addresses().iter().next().copied())
                        else {
                            continue;
                        };
                        let os_num = info
                            .get_property_val_str("os")
                            .and_then(|s| s.parse::<u8>().ok())
                            .and_then(|v| OsKind::from_u8(v).ok())
                            .unwrap_or(OsKind::Windows);
                        let remote_host = info
                            .get_property_val_str("hostname")
                            .unwrap_or("unknown")
                            .to_string();

                        let peer = DiscoveredPeer {
                            peer_id,
                            hostname: remote_host,
                            os: os_num,
                            addr: SocketAddr::new(ip, info.get_port()),
                        };
                        debug!(?peer, "mDNS resolved peer");
                        if tx.blocking_send(peer).is_err() {
                            break;
                        }
                    }
                }
                warn!("mDNS browse loop exited");
            })
            .ok();

        Ok((Self { daemon }, rx))
    }

    pub fn shutdown(self) {
        let _ = self.daemon.shutdown();
    }
}
