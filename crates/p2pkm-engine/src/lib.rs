pub mod clipboard;
pub mod config;
pub mod file_transfer;
pub mod keymap;
pub mod security;
pub mod topology;

pub use clipboard::{ClipboardLoopGuard, ClipboardPayload, FileOfferItem};
pub use config::{AppConfig, Language, PeerPlacement, DEFAULT_LISTEN_PORT, DEFAULT_OVERSHOOT_PX};
pub use file_transfer::{sanitize_rel_path, ChunkedFileWriter, TransferError, FILE_CHUNK_SIZE};
pub use keymap::{
    map_hid_key, map_hid_key_with_custom, CustomKeyMap, KeyMapMode, KeyMappingRule, ModifierTarget,
};
pub use security::{compute_pairing_proof, verify_pairing_proof, TrustStore};
pub use topology::{EdgeCrossing, PeerDisplayNode, VirtualDesktop};
