//! Typed object-storage configuration.
//!
//! The media library writes through one of two drivers: the S3-compatible object store the
//! development stack ships (MinIO, `infra/compose/minio.yml`) — which is also what a production
//! deployment points at its own S3 endpoint — or the file-system driver used by local runs and
//! tests that do not want a bucket. Both read their settings from the process environment and
//! validate them once at boot, so a misconfigured deployment fails fast
//! (docs/02-ARCHITECTURE.md, "Reliability").

use std::path::PathBuf;

use crate::error::{Result, StorageError};

/// Object-store endpoint of the development stack (`infra/compose/minio.yml`).
pub const DEFAULT_S3_ENDPOINT: &str = "http://127.0.0.1:9000";

/// Region MinIO and most self-hosted S3 gateways accept (`OMNION_S3_REGION`).
pub const DEFAULT_S3_REGION: &str = "us-east-1";

/// Bucket the media library writes into (`OMNION_S3_BUCKET`).
pub const DEFAULT_S3_BUCKET: &str = "omnion-media";

/// Development access key, matching the compose stack (`OMNION_S3_ACCESS_KEY`).
pub const DEFAULT_S3_ACCESS_KEY: &str = "omnion";

/// Development secret key, matching the compose stack (`OMNION_S3_SECRET_KEY`).
/// A deployment sets its own; the value is never logged.
pub const DEFAULT_S3_SECRET_KEY: &str = "omnion-dev-secret";

/// Root directory of the file-system driver (`OMNION_STORAGE_DIR`).
pub const DEFAULT_STORAGE_DIR: &str = ".omnion-media";

/// Which driver the media library talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageDriver {
    /// An S3-compatible object store (MinIO in development, any S3 endpoint in production).
    S3,
    /// A directory on the local filesystem — development and test convenience.
    Fs,
}

impl StorageDriver {
    /// Canonical lowercase name, used in logs and payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Fs => "fs",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "s3" | "minio" => Ok(Self::S3),
            "fs" | "file" | "filesystem" | "local" => Ok(Self::Fs),
            other => Err(StorageError::Invalid(format!(
                "OMNION_STORAGE_DRIVER: expected `s3` or `fs`, got {other:?}"
            ))),
        }
    }
}

/// Settings one storage driver needs to open.
#[derive(Clone, PartialEq, Eq)]
pub struct StorageConfig {
    /// Which driver is active.
    pub driver: StorageDriver,
    /// S3 endpoint, scheme included (`OMNION_S3_ENDPOINT`).
    pub endpoint: String,
    /// S3 region (`OMNION_S3_REGION`).
    pub region: String,
    /// Bucket the media library owns (`OMNION_S3_BUCKET`).
    pub bucket: String,
    /// S3 access key (`OMNION_S3_ACCESS_KEY`).
    pub access_key: String,
    /// S3 secret key (`OMNION_S3_SECRET_KEY`) — never logged or rendered.
    pub secret_key: String,
    /// Root directory of the file-system driver (`OMNION_STORAGE_DIR`).
    pub root: PathBuf,
}

impl std::fmt::Debug for StorageConfig {
    /// Renders the settings for a boot log without ever printing the secret key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageConfig")
            .field("driver", &self.driver.as_str())
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field("root", &self.root)
            .finish()
    }
}

impl StorageConfig {
    /// Load and validate the configuration from the process environment.
    pub fn from_env() -> Result<Self> {
        Self::from_source(|key| std::env::var(key).ok())
    }

    /// Load and validate the configuration from an arbitrary key/value source.
    ///
    /// Blank values are treated as unset, which is how empty compose variables behave.
    pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let read = |key: &str| -> Option<String> {
            get(key)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };

        let driver = match read("OMNION_STORAGE_DRIVER") {
            Some(raw) => StorageDriver::parse(&raw.to_lowercase())?,
            None => StorageDriver::S3,
        };

        let config = Self {
            driver,
            endpoint: read("OMNION_S3_ENDPOINT").unwrap_or_else(|| DEFAULT_S3_ENDPOINT.to_owned()),
            region: read("OMNION_S3_REGION").unwrap_or_else(|| DEFAULT_S3_REGION.to_owned()),
            bucket: read("OMNION_S3_BUCKET").unwrap_or_else(|| DEFAULT_S3_BUCKET.to_owned()),
            access_key: read("OMNION_S3_ACCESS_KEY")
                .unwrap_or_else(|| DEFAULT_S3_ACCESS_KEY.to_owned()),
            secret_key: read("OMNION_S3_SECRET_KEY")
                .unwrap_or_else(|| DEFAULT_S3_SECRET_KEY.to_owned()),
            root: PathBuf::from(
                read("OMNION_STORAGE_DIR").unwrap_or_else(|| DEFAULT_STORAGE_DIR.to_owned()),
            ),
        };
        config.validate()?;
        Ok(config)
    }

    /// Re-check the loaded configuration.
    pub fn validate(&self) -> Result<()> {
        match self.driver {
            StorageDriver::S3 => {
                if !(self.endpoint.starts_with("http://") || self.endpoint.starts_with("https://"))
                {
                    return Err(StorageError::Invalid(format!(
                        "OMNION_S3_ENDPOINT: expected an http(s) URL, got {:?}",
                        self.endpoint
                    )));
                }
                if self.endpoint.contains('?') || self.endpoint.contains('#') {
                    return Err(StorageError::Invalid(
                        "OMNION_S3_ENDPOINT: expected a plain origin without a query or fragment"
                            .to_owned(),
                    ));
                }
                if self.bucket.trim().is_empty() {
                    return Err(StorageError::Invalid(
                        "OMNION_S3_BUCKET: the bucket name may not be blank".to_owned(),
                    ));
                }
                if self.region.trim().is_empty() {
                    return Err(StorageError::Invalid(
                        "OMNION_S3_REGION: the region may not be blank".to_owned(),
                    ));
                }
                Ok(())
            }
            StorageDriver::Fs => {
                if self.root.as_os_str().is_empty() {
                    return Err(StorageError::Invalid(
                        "OMNION_STORAGE_DIR: the storage directory may not be blank".to_owned(),
                    ));
                }
                Ok(())
            }
        }
    }

    /// One line for the boot log: what the media library writes into, without any secret.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.driver {
            StorageDriver::S3 => format!("s3://{} at {}", self.bucket, self.endpoint),
            StorageDriver::Fs => format!("fs at {}", self.root.display()),
        }
    }
}

impl Default for StorageConfig {
    /// Development defaults that match the compose stack, without reading the environment.
    fn default() -> Self {
        Self {
            driver: StorageDriver::S3,
            endpoint: DEFAULT_S3_ENDPOINT.to_owned(),
            region: DEFAULT_S3_REGION.to_owned(),
            bucket: DEFAULT_S3_BUCKET.to_owned(),
            access_key: DEFAULT_S3_ACCESS_KEY.to_owned(),
            secret_key: DEFAULT_S3_SECRET_KEY.to_owned(),
            root: PathBuf::from(DEFAULT_STORAGE_DIR),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config_from(pairs: &[(&str, &str)]) -> Result<StorageConfig> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        StorageConfig::from_source(|key| map.get(key).cloned())
    }

    #[test]
    fn defaults_match_the_compose_stack() {
        let config = config_from(&[]).expect("empty source must fall back to defaults");
        assert_eq!(config.driver, StorageDriver::S3);
        assert_eq!(config.endpoint, DEFAULT_S3_ENDPOINT);
        assert_eq!(config.bucket, DEFAULT_S3_BUCKET);
        assert_eq!(config.access_key, DEFAULT_S3_ACCESS_KEY);
        assert_eq!(
            config.describe(),
            "s3://omnion-media at http://127.0.0.1:9000"
        );
    }

    #[test]
    fn a_blank_value_falls_back_to_the_default() {
        let config = config_from(&[("OMNION_S3_BUCKET", "   ")]).expect("blank is unset");
        assert_eq!(config.bucket, DEFAULT_S3_BUCKET);
    }

    #[test]
    fn the_file_system_driver_is_selectable() {
        let config = config_from(&[
            ("OMNION_STORAGE_DRIVER", "fs"),
            ("OMNION_STORAGE_DIR", "/tmp/omnion-media"),
        ])
        .expect("the fs driver is valid");
        assert_eq!(config.driver, StorageDriver::Fs);
        assert_eq!(config.describe(), "fs at /tmp/omnion-media");
    }

    #[test]
    fn an_unknown_driver_is_rejected() {
        let error = config_from(&[("OMNION_STORAGE_DRIVER", "tape")])
            .expect_err("only s3 and fs are supported");
        assert!(
            error.to_string().contains("OMNION_STORAGE_DRIVER"),
            "{error}"
        );
    }

    #[test]
    fn an_endpoint_without_a_scheme_is_rejected() {
        let error = config_from(&[("OMNION_S3_ENDPOINT", "127.0.0.1:9000")])
            .expect_err("the endpoint needs a scheme");
        assert!(error.to_string().contains("http(s) URL"), "{error}");
    }

    #[test]
    fn an_endpoint_with_a_query_is_rejected() {
        let error = config_from(&[("OMNION_S3_ENDPOINT", "http://127.0.0.1:9000/?x=1")])
            .expect_err("the endpoint is an origin");
        assert!(error.to_string().contains("query"), "{error}");
    }

    #[test]
    fn the_secret_key_never_appears_in_debug_output() {
        let config = config_from(&[("OMNION_S3_SECRET_KEY", "super-secret-value")])
            .expect("a custom secret is valid");
        assert_eq!(config.secret_key, "super-secret-value");

        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("super-secret-value"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "rendered: {rendered}");
    }
}
