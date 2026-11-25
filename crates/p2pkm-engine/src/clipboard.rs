use p2pkm_proto::PeerId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardPayload {
    Text(String),
    PngImage(Vec<u8>),
    FileOffer(Vec<FileOfferItem>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOfferItem {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

impl ClipboardPayload {
    pub fn blake3_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        match self {
            ClipboardPayload::Text(t) => {
                hasher.update(b"TEXT:");
                hasher.update(t.as_bytes());
            }
            ClipboardPayload::PngImage(bytes) => {
                hasher.update(b"PNG:");
                hasher.update(bytes);
            }
            ClipboardPayload::FileOffer(items) => {
                hasher.update(b"FILES:");
                for item in items {
                    hasher.update(item.name.as_bytes());
                    hasher.update(&item.size.to_le_bytes());
                }
            }
        }
        *hasher.finalize().as_bytes()
    }
}

/// Prevents infinite clipboard sync loops across peers using BLAKE3 content hashing.
#[derive(Default)]
pub struct ClipboardLoopGuard {
    last_hash: Option<[u8; 32]>,
}

impl ClipboardLoopGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called when the local OS reports a clipboard change.
    /// Returns `Some((hash, payload))` if this is genuinely new content to broadcast,
    /// or `None` if it matches the content we just applied from a remote peer.
    pub fn on_local_change(&mut self, payload: &ClipboardPayload) -> Option<[u8; 32]> {
        let hash = payload.blake3_hash();
        if self.last_hash == Some(hash) {
            return None;
        }
        self.last_hash = Some(hash);
        Some(hash)
    }

    /// Called right BEFORE writing a remote peer's clipboard payload into the local OS.
    /// Returns `true` if the payload is new and should be applied locally.
    pub fn on_remote_receive(&mut self, _from: PeerId, payload: &ClipboardPayload) -> bool {
        let hash = payload.blake3_hash();
        if self.last_hash == Some(hash) {
            return false;
        }
        self.last_hash = Some(hash);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prevents_infinite_clipboard_ping_pong() {
        let peer_a = [1u8; 16];
        let peer_b = [2u8; 16];
        let mut guard_a = ClipboardLoopGuard::new();
        let mut guard_b = ClipboardLoopGuard::new();

        let clip = ClipboardPayload::Text("Hello P2P Mesh!".into());

        // 1. User copies on A -> broadcasts to B
        assert!(guard_a.on_local_change(&clip).is_some());
        // 2. B receives from A -> applies to B's OS clipboard
        assert!(guard_b.on_remote_receive(peer_a, &clip));
        // 3. B's OS fires WM_CLIPBOARDUPDATE / changeCount -> guard_b MUST suppress echo!
        assert!(guard_b.on_local_change(&clip).is_none());
        // 4. Duplicate receive on B is also ignored
        assert!(!guard_b.on_remote_receive(peer_a, &clip));

        // 5. User copies NEW image on B -> broadcasts to A
        let img = ClipboardPayload::PngImage(vec![0x89, 0x50, 0x4E, 0x47]);
        assert!(guard_b.on_local_change(&img).is_some());
        assert!(guard_a.on_remote_receive(peer_b, &img));
        assert!(guard_a.on_local_change(&img).is_none());
    }
}
