use std::path::{Component, Path, PathBuf};
use thiserror::Error;

pub const FILE_CHUNK_SIZE: usize = 256 * 1024; // 256 KB

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransferError {
    #[error("unsafe relative path rejected: {0}")]
    UnsafePath(String),
    #[error("checksum mismatch: expected {expected:?}, got {actual:?}")]
    ChecksumMismatch { expected: [u8; 32], actual: [u8; 32] },
}

/// Sanitizes a relative file path received from a peer to prevent directory traversal (`..` / root).
pub fn sanitize_rel_path(rel: &str) -> Result<PathBuf, TransferError> {
    let p = Path::new(rel);
    let mut clean = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(seg) => clean.push(seg),
            _ => return Err(TransferError::UnsafePath(rel.to_string())),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(TransferError::UnsafePath(rel.to_string()));
    }
    Ok(clean)
}

pub struct ChunkedFileWriter {
    hasher: blake3::Hasher,
    bytes_written: u64,
    buffer: Vec<u8>,
}

impl ChunkedFileWriter {
    pub fn new(expected_size: usize) -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
            buffer: Vec::with_capacity(expected_size.min(1024 * 1024)),
        }
    }

    pub fn push_chunk(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        self.buffer.extend_from_slice(chunk);
        self.bytes_written += chunk.len() as u64;
    }

    pub fn finalize(self, expected_blake3: [u8; 32]) -> Result<Vec<u8>, TransferError> {
        let actual = *self.hasher.finalize().as_bytes();
        if actual != expected_blake3 {
            return Err(TransferError::ChecksumMismatch {
                expected: expected_blake3,
                actual,
            });
        }
        Ok(self.buffer)
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        assert!(sanitize_rel_path("../secret.txt").is_err());
        assert!(sanitize_rel_path("/etc/passwd").is_err());
        assert!(sanitize_rel_path("").is_err());
        assert_eq!(
            sanitize_rel_path("folder/report.pdf").unwrap(),
            PathBuf::from("folder").join("report.pdf")
        );
    }

    #[test]
    fn chunked_transfer_verifies_blake3() {
        let data = vec![0x42u8; FILE_CHUNK_SIZE * 2 + 123];
        let expected_hash = *blake3::hash(&data).as_bytes();

        let mut writer = ChunkedFileWriter::new(data.len());
        for chunk in data.chunks(FILE_CHUNK_SIZE) {
            writer.push_chunk(chunk);
        }
        assert_eq!(writer.bytes_written(), data.len() as u64);
        let out = writer.finalize(expected_hash).expect("checksum must match");
        assert_eq!(out, data);
    }

    #[test]
    fn corrupted_chunk_fails_checksum() {
        let data = b"hello world".to_vec();
        let expected_hash = *blake3::hash(&data).as_bytes();

        let mut writer = ChunkedFileWriter::new(data.len());
        writer.push_chunk(b"hello Xorld");
        assert!(matches!(
            writer.finalize(expected_hash),
            Err(TransferError::ChecksumMismatch { .. })
        ));
    }
}
