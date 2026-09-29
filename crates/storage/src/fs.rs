//! File-system backed object storage.
//!
//! The development driver for environments that do not run a bucket — local runs, tests and
//! air-gapped setups. Objects are plain files below a root directory and keys are validated
//! before they are joined onto it (see [`crate::keys`]), so a key can never leave the root.

use std::path::{Path, PathBuf};

use crate::error::{Result, StorageError};
use crate::keys::validate_key;
use crate::signing::payload_hash;
use crate::{StoredObject, config::StorageConfig};

/// Object store on the local filesystem.
#[derive(Debug, Clone)]
pub struct FsStorage {
    root: PathBuf,
}

impl FsStorage {
    /// Open the store, creating the root directory when it does not exist yet.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            return Err(StorageError::Invalid(
                "the storage directory may not be blank".to_owned(),
            ));
        }
        std::fs::create_dir_all(&root)
            .map_err(|err| StorageError::Io(format!("{}: {err}", root.display())))?;
        Ok(Self { root })
    }

    /// Build the driver from a validated configuration.
    pub fn from_config(config: &StorageConfig) -> Result<Self> {
        Self::new(config.root.clone())
    }

    /// Root directory objects are written under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of one key, refused when the key is not shaped like a key.
    fn path_for(&self, key: &str) -> Result<PathBuf> {
        validate_key(key)?;
        Ok(self.root.join(key))
    }

    /// Write an object, replacing whatever was stored under the key before.
    pub async fn put(&self, key: &str, body: &[u8], _content_type: &str) -> Result<StoredObject> {
        let path = self.path_for(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| StorageError::Io(format!("{}: {err}", parent.display())))?;
        }
        std::fs::write(&path, body)
            .map_err(|err| StorageError::Io(format!("{}: {err}", path.display())))?;

        Ok(StoredObject {
            size_bytes: body.len() as u64,
            checksum: payload_hash(body),
        })
    }

    /// Read an object back; a missing file is a `NotFound`.
    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.path_for(key)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(StorageError::NotFound {
                key: key.to_owned(),
            }),
            Err(err) => Err(StorageError::Io(format!("{}: {err}", path.display()))),
        }
    }

    /// Read a window of an object back; a missing file is a `NotFound`.
    ///
    /// The local driver serves ranges out of one in-memory read rather than seeking, because the
    /// file is already mapped into this process — but the *answer* is the window, not the whole
    /// object, so a caller cannot tell from the bytes it got whether the store or the driver
    /// produced them. A window past the end is a `RangeNotSatisfiable`, exactly as S3 answers,
    /// so the route's `416` does not depend on which driver is mounted.
    pub async fn get_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>> {
        let bytes = self.get(key).await?;
        let total = bytes.len() as u64;
        if total == 0 || start >= total || end < start {
            return Err(StorageError::RangeNotSatisfiable {
                key: key.to_owned(),
                requested: format!("bytes={start}-{end}"),
            });
        }
        let start = start as usize;
        let end = end.min(total - 1) as usize;
        Ok(bytes[start..=end].to_vec())
    }

    /// Remove an object; `true` when one was there.
    pub async fn delete(&self, key: &str) -> Result<bool> {
        let path = self.path_for(key)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(StorageError::Io(format!("{}: {err}", path.display()))),
        }
    }

    /// Check that the root exists and is writable.
    pub async fn probe(&self) -> Result<()> {
        let probe = self.root.join(".omnion-probe");
        std::fs::write(&probe, b"probe")
            .map_err(|err| StorageError::Io(format!("{}: {err}", probe.display())))?;
        let _ = std::fs::remove_file(&probe);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("omnion-storage-{label}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn objects_round_trip_through_the_filesystem() {
        let root = temp_root("round-trip");
        let store = FsStorage::new(&root).expect("the root must open");

        let stored = store
            .put("sites/a/one.txt", b"omnion", "text/plain")
            .await
            .expect("the object must be written");
        assert_eq!(stored.size_bytes, 6);
        assert_eq!(stored.checksum, payload_hash(b"omnion"));

        let read = store
            .get("sites/a/one.txt")
            .await
            .expect("the object must read");
        assert_eq!(read, b"omnion");

        assert!(store.delete("sites/a/one.txt").await.expect("delete runs"));
        assert!(
            !store
                .delete("sites/a/one.txt")
                .await
                .expect("a second delete is a no-op")
        );
        assert!(matches!(
            store.get("sites/a/one.txt").await,
            Err(StorageError::NotFound { .. })
        ));

        assert!(store.probe().await.is_ok());
        std::fs::remove_dir_all(&root).expect("the test root must clean up");
    }

    #[tokio::test]
    async fn a_key_that_escapes_the_root_is_refused() {
        let root = temp_root("escape");
        let store = FsStorage::new(&root).expect("the root must open");

        assert!(matches!(
            store.put("../outside.txt", b"no", "text/plain").await,
            Err(StorageError::Invalid(_))
        ));
        assert!(!Path::new(&root).join("..").join("outside.txt").exists());
        std::fs::remove_dir_all(&root).expect("the test root must clean up");
    }

    /// A window is compared **as bytes**, and every edge of it is checked against the object
    /// rather than against a length the test computed: the single-byte case and the last-byte
    /// case are where an off-by-one hides, and a length check passes by accident on both.
    #[tokio::test]
    async fn a_window_answers_exactly_the_bytes_it_names() {
        let root = temp_root("range");
        let store = FsStorage::new(&root).expect("the root must open");
        let body = b"omnion range".to_vec();
        store
            .put("sites/a/one.bin", &body, "application/octet-stream")
            .await
            .expect("the object must be written");

        let head = store
            .get_range("sites/a/one.bin", 0, 5)
            .await
            .expect("a window must read");
        assert_eq!(head, b"omnion".to_vec(), "the first six bytes");

        let tail = store
            .get_range("sites/a/one.bin", 7, 11)
            .await
            .expect("a window must read");
        assert_eq!(tail, b"range".to_vec(), "the last five bytes");

        // The whole object, as one window: a range request that covers everything is the same
        // bytes, which is what a player asks for when it starts playing from the beginning.
        let whole = store
            .get_range("sites/a/one.bin", 0, 11)
            .await
            .expect("a window must read");
        assert_eq!(whole, body);

        // A single byte at each end — the two windows a stream reader asks for first.
        let first = store
            .get_range("sites/a/one.bin", 0, 0)
            .await
            .expect("a one-byte window must read");
        assert_eq!(first, b"o".to_vec());
        let last = store
            .get_range("sites/a/one.bin", 11, 11)
            .await
            .expect("a one-byte window must read");
        assert_eq!(last, b"e".to_vec());

        std::fs::remove_dir_all(&root).expect("the test root must clean up");
    }

    /// A window past the end is refused rather than silently shortened, and an **empty object**
    /// has no window at all — a store that answers `416` there is what the route expects, and a
    /// store that answers an empty slice leaves the caller unable to tell the two apart.
    #[tokio::test]
    async fn a_window_past_the_end_is_refused() {
        let root = temp_root("range-end");
        let store = FsStorage::new(&root).expect("the root must open");
        store
            .put("sites/a/one.bin", b"omnion", "application/octet-stream")
            .await
            .expect("the object must be written");
        store
            .put("sites/a/empty.bin", b"", "application/octet-stream")
            .await
            .expect("the empty object must be written");

        for (start, end) in [(6, 9), (99, 120)] {
            assert!(
                matches!(
                    store.get_range("sites/a/one.bin", start, end).await,
                    Err(StorageError::RangeNotSatisfiable { .. })
                ),
                "bytes={start}-{end} is past the end of a six-byte object"
            );
        }
        assert!(matches!(
            store.get_range("sites/a/empty.bin", 0, 0).await,
            Err(StorageError::RangeNotSatisfiable { .. })
        ));

        // A window whose end runs past the object is clamped, not refused: that is the player
        // asking for "the rest of this file" and the object being shorter than it believed.
        let clamped = store
            .get_range("sites/a/one.bin", 3, 9999)
            .await
            .expect("a clamped window must read");
        assert_eq!(clamped, b"ion".to_vec());

        std::fs::remove_dir_all(&root).expect("the test root must clean up");
    }
}
