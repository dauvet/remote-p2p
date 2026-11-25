use p2pkm_proto::PeerId;
use std::collections::HashMap;

#[derive(Default)]
pub struct TrustStore {
    /// Maps paired PeerId -> pinned 32-byte SHA256/BLAKE3 certificate fingerprint.
    pinned_certs: HashMap<PeerId, [u8; 32]>,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trust_peer(&mut self, peer_id: PeerId, cert_fingerprint: [u8; 32]) {
        self.pinned_certs.insert(peer_id, cert_fingerprint);
    }

    pub fn is_trusted(&self, peer_id: &PeerId, cert_fingerprint: &[u8; 32]) -> bool {
        match self.pinned_certs.get(peer_id) {
            Some(expected) => constant_time_eq(expected, cert_fingerprint),
            None => false,
        }
    }

    pub fn revoke_peer(&mut self, peer_id: &PeerId) {
        self.pinned_certs.remove(peer_id);
    }
}

/// Computes a keyed BLAKE3 pairing proof binding `pin`, `nonce`, and both peers' cert fingerprints
/// so the PIN is never sent over the wire and MITM cannot relay the proof.
pub fn compute_pairing_proof(
    pin: &str,
    nonce: &[u8; 16],
    initiator_fp: &[u8; 32],
    responder_fp: &[u8; 32],
) -> [u8; 32] {
    let key = *blake3::hash(pin.as_bytes()).as_bytes();
    let mut hasher = blake3::Hasher::new_keyed(&key);
    hasher.update(nonce);
    hasher.update(initiator_fp);
    hasher.update(responder_fp);
    *hasher.finalize().as_bytes()
}

pub fn verify_pairing_proof(
    pin: &str,
    nonce: &[u8; 16],
    initiator_fp: &[u8; 32],
    responder_fp: &[u8; 32],
    proof: &[u8; 32],
) -> bool {
    let expected = compute_pairing_proof(pin, nonce, initiator_fp, responder_fp);
    constant_time_eq(&expected, proof)
}

#[inline]
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_pairing_proof_binds_certs_and_rejects_wrong_pin() {
        let nonce = [7u8; 16];
        let fp_a = [0xAAu8; 32];
        let fp_b = [0xBBu8; 32];
        let fp_mitm = [0xCCu8; 32];

        let proof = compute_pairing_proof("482910", &nonce, &fp_a, &fp_b);
        assert!(verify_pairing_proof("482910", &nonce, &fp_a, &fp_b, &proof));
        assert!(!verify_pairing_proof("000000", &nonce, &fp_a, &fp_b, &proof));
        assert!(!verify_pairing_proof("482910", &nonce, &fp_a, &fp_mitm, &proof));
    }

    #[test]
    fn trust_store_pins_and_revokes() {
        let mut store = TrustStore::new();
        let peer = [1u8; 16];
        let fp = [9u8; 32];
        assert!(!store.is_trusted(&peer, &fp));
        store.trust_peer(peer, fp);
        assert!(store.is_trusted(&peer, &fp));
        assert!(!store.is_trusted(&peer, &[8u8; 32]));
        store.revoke_peer(&peer);
        assert!(!store.is_trusted(&peer, &fp));
    }
}
