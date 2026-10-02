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
}
