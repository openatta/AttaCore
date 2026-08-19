//! Content-addressed blob storage for one recording.
//!
//! A recording references its bulky, highly repetitive parts — system prompt
//! blocks, the tool table, conversation messages — by content hash instead of
//! inlining them into every call record. Identical content stored twice
//! occupies one file, which is what keeps a recording linear in the number of
//! calls rather than quadratic in messages: the 50th call's 49 historical
//! messages are all references to blobs that already exist.
//!
//! Deduplication is scoped to a single recording directory. Sharing blobs
//! across recordings would save more space but would forfeit the property that
//! makes cleanup trivial for the host — deleting one recording is `rm -rf` on
//! its directory, with nothing else referencing what went away.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// A blob's identity: the first 16 hex characters of the SHA-256 of its
/// canonical JSON encoding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobId(pub String);

impl BlobId {
    fn of(json: &str) -> Self {
        let digest = Sha256::digest(json.as_bytes());
        Self(hex::encode(&digest[..8]))
    }

    /// The id `value` would get, without storing it. Replay uses this to line a
    /// live request up against a recorded one without writing anything.
    pub fn of_value<T: Serialize>(value: &T) -> Self {
        match serde_json::to_string(value) {
            Ok(json) => Self::of(&json),
            Err(_) => Self(String::new()),
        }
    }
}

impl std::fmt::Display for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `blobs/` directory of one recording.
///
/// Values are serialized with `serde_json`, whose output for a given Rust type
/// is field-order deterministic — the same value always hashes to the same id.
/// Content is stored verbatim: a recording's worth is that it holds what was
/// actually sent, so nothing is normalized or redacted on the way in.
#[derive(Debug, Clone)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    pub fn new(recording_dir: &Path) -> Self {
        Self {
            dir: recording_dir.join("blobs"),
        }
    }

    pub fn put<T: Serialize>(&self, value: &T) -> Result<BlobId, std::io::Error> {
        let json = serde_json::to_string(value)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.put_raw(&json)
    }

    pub fn put_raw(&self, json: &str) -> Result<BlobId, std::io::Error> {
        let id = BlobId::of(json);
        let path = self.dir.join(&id.0);
        if path.exists() {
            return Ok(id);
        }
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&path, json)?;
        Ok(id)
    }

    pub fn get<T: DeserializeOwned>(&self, id: &BlobId) -> Result<Option<T>, std::io::Error> {
        let Some(json) = self.get_raw(id)? else {
            return Ok(None);
        };
        serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub fn get_raw(&self, id: &BlobId) -> Result<Option<String>, std::io::Error> {
        match std::fs::read_to_string(self.dir.join(&id.0)) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (BlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (BlobStore::new(dir.path()), dir)
    }

    #[test]
    fn same_content_yields_one_file() {
        let (store, dir) = store();
        let a = store.put(&"hello").unwrap();
        let b = store.put(&"hello").unwrap();
        assert_eq!(a, b);
        let count = std::fs::read_dir(dir.path().join("blobs")).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn different_content_yields_different_ids() {
        let (store, _dir) = store();
        assert_ne!(store.put(&"a").unwrap(), store.put(&"b").unwrap());
    }

    #[test]
    fn round_trips_a_typed_value() {
        let (store, _dir) = store();
        let value = vec![1u32, 2, 3];
        let id = store.put(&value).unwrap();
        assert_eq!(store.get::<Vec<u32>>(&id).unwrap(), Some(value));
    }

    #[test]
    fn missing_blob_reads_as_none() {
        let (store, _dir) = store();
        let absent = BlobId("0123456789abcdef".into());
        assert_eq!(store.get_raw(&absent).unwrap(), None);
    }

    #[test]
    fn id_is_sixteen_hex_chars() {
        let (store, _dir) = store();
        let id = store.put(&"anything").unwrap();
        assert_eq!(id.0.len(), 16);
        assert!(id.0.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
