//! Omnion storage — the object-storage abstraction behind the media library.
//!
//! One interface writes objects either into an S3-compatible bucket (`MinIO` in development,
//! any S3 endpoint in production) or into a directory on the local filesystem
//! (docs/02-ARCHITECTURE.md; docs/04-MONOREPO.md places this crate next to `media/`, which owns
//! the metadata of what is stored here). The media library keeps the bytes and the database in
//! step: rows in `media` describe objects, this crate moves them.
//!
//! The trait surface stays deliberately small — put, get, delete, probe — because that is all
//! the platform uses today; presigned URLs, multipart transfers and lifecycle rules arrive
//! when the enterprise file manager needs them (docs/requests/REQ-010).

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod fs;
pub mod keys;
pub mod s3;
pub mod signing;

pub use config::{
    DEFAULT_S3_ACCESS_KEY, DEFAULT_S3_BUCKET, DEFAULT_S3_ENDPOINT, DEFAULT_S3_REGION,
    DEFAULT_S3_SECRET_KEY, DEFAULT_STORAGE_DIR, StorageConfig, StorageDriver,
};
pub use error::{Result, StorageError};
pub use fs::FsStorage;
pub use keys::{MAX_KEY_LENGTH, validate_key};
pub use s3::S3Storage;

/// What a successful write stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    /// Size of the stored object in bytes.
    pub size_bytes: u64,
    /// Hex-encoded SHA-256 of the bytes, recorded on the media row.
    pub checksum: String,
}

/// The object store of a running service: one configured driver behind one interface.
#[derive(Debug, Clone)]
pub enum Storage {
    /// An S3-compatible object store.
    S3(Box<S3Storage>),
    /// A directory on the local filesystem.
    Fs(FsStorage),
}

impl Storage {
    /// Open the driver a configuration names.
    pub fn from_config(config: &StorageConfig) -> Result<Self> {
        config.validate()?;
        match config.driver {
            StorageDriver::S3 => Ok(Self::S3(Box::new(S3Storage::new(config)?))),
            StorageDriver::Fs => Ok(Self::Fs(FsStorage::from_config(config)?)),
        }
    }

    /// Open the driver the process environment names (defaults to the development stack).
    pub fn from_env() -> Result<Self> {
        Self::from_config(&StorageConfig::from_env()?)
    }

    /// Which driver is active.
    #[must_use]
    pub const fn driver(&self) -> StorageDriver {
        match self {
            Self::S3(_) => StorageDriver::S3,
            Self::Fs(_) => StorageDriver::Fs,
        }
    }

    /// One line for a boot log: where objects are written, without any secret.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::S3(storage) => format!("s3://{}", storage.bucket()),
            Self::Fs(storage) => format!("fs at {}", storage.root().display()),
        }
    }

    /// Write an object, replacing whatever was stored under the key before.
    pub async fn put(&self, key: &str, body: &[u8], content_type: &str) -> Result<StoredObject> {
        match self {
            Self::S3(storage) => storage.put(key, body, content_type).await,
            Self::Fs(storage) => storage.put(key, body, content_type).await,
        }
    }

    /// Read an object back; a missing object is a `NotFound`.
    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        match self {
            Self::S3(storage) => storage.get(key).await,
            Self::Fs(storage) => storage.get(key).await,
        }
    }

    /// Remove an object; `true` when one was there.
    pub async fn delete(&self, key: &str) -> Result<bool> {
        match self {
            Self::S3(storage) => storage.delete(key).await,
            Self::Fs(storage) => storage.delete(key).await,
        }
    }

    /// Check that the store answers, without writing an object.
    ///
    /// A bucket that does not exist yet answers `NotFound` — that is a first-run condition, not
    /// an unavailable store; `Unavailable` means the store itself cannot be reached.
    pub async fn probe(&self) -> Result<()> {
        match self {
            Self::S3(storage) => storage.probe().await,
            Self::Fs(storage) => storage.probe().await,
        }
    }

    /// Make the store ready for writes: create the bucket when it is missing.
    pub async fn ensure_ready(&self) -> Result<()> {
        match self {
            Self::S3(storage) => storage.ensure_bucket().await,
            Self::Fs(storage) => storage.probe().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_system_driver_opens_from_a_configuration() {
        let root = std::env::temp_dir().join(format!("omnion-storage-lib-{}", std::process::id()));
        let config = StorageConfig {
            driver: StorageDriver::Fs,
            root: root.clone(),
            ..StorageConfig::default()
        };

        let storage = Storage::from_config(&config).expect("the fs driver must open");
        assert_eq!(storage.driver(), StorageDriver::Fs);
        assert_eq!(storage.describe(), format!("fs at {}", root.display()));

        std::fs::remove_dir_all(&root).expect("the test root must clean up");
    }

    #[test]
    fn the_s3_driver_opens_from_the_development_defaults() {
        let storage = Storage::from_config(&StorageConfig::default()).expect("defaults are valid");
        assert_eq!(storage.driver(), StorageDriver::S3);
        assert_eq!(storage.describe(), "s3://omnion-media");
    }
}
