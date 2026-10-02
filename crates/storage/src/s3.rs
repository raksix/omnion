//! S3-compatible object storage.
//!
//! The same client talks to MinIO in development (`infra/compose/minio.yml`) and to any
//! S3-compatible endpoint in production (docs/02-ARCHITECTURE.md). Requests are signed with
//! SigV4 ([`crate::signing`]) and addressed path-style — `{endpoint}/{bucket}/{key}` — which is
//! what self-hosted gateways and MinIO serve out of the box.
//!
//! Buckets are created on demand: the first write that meets a missing bucket asks for it once
//! and retries, so a fresh development stack needs no manual `mc mb` step.

use std::time::Duration;

use bytes::Bytes;
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use time::OffsetDateTime;

use crate::StoredObject;
use crate::config::StorageConfig;
use crate::error::{Result, StorageError};
use crate::keys::validate_key;
use crate::signing::{self, Credentials, EMPTY_PAYLOAD_HASH, RequestToSign};

/// Upper bound for one request against the object store.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest provider message kept in an error — enough for an operator, never a whole body.
const MAX_PROVIDER_MESSAGE: usize = 300;

/// Object store behind an S3-compatible HTTP API.
#[derive(Debug, Clone)]
pub struct S3Storage {
    client: reqwest::Client,
    endpoint: String,
    region: String,
    bucket: String,
    credentials: Credentials,
}

impl S3Storage {
    /// Build the client from a validated configuration.
    pub fn new(config: &StorageConfig) -> Result<Self> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| StorageError::Unavailable(err.to_string()))?;

        Ok(Self {
            client,
            endpoint: config.endpoint.trim_end_matches('/').to_owned(),
            region: config.region.clone(),
            bucket: config.bucket.clone(),
            credentials: Credentials {
                access_key: config.access_key.clone(),
                secret_key: config.secret_key.clone(),
            },
        })
    }

    /// Bucket this client writes into.
    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Write an object, replacing whatever was stored under the key before.
    pub async fn put(&self, key: &str, body: &[u8], content_type: &str) -> Result<StoredObject> {
        validate_key(key)?;
        let path = self.object_path(key);

        let mut response = self
            .execute("PUT", &path, Vec::new(), Some((body, content_type)))
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            // A missing bucket is a first-run condition, not a failure: create it and retry once.
            let message = response.text().await.unwrap_or_default();
            if !message.contains("NoSuchBucket") {
                return Err(StorageError::NotFound {
                    key: key.to_owned(),
                });
            }
            self.ensure_bucket().await?;
            response = self
                .execute("PUT", &path, Vec::new(), Some((body, content_type)))
                .await?;
        }

        if !response.status().is_success() {
            return Err(provider_error(response).await);
        }

        Ok(StoredObject {
            size_bytes: body.len() as u64,
            checksum: signing::payload_hash(body),
        })
    }

    /// Read an object back; a missing object is a `NotFound`.
    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        validate_key(key)?;
        let response = self
            .execute("GET", &self.object_path(key), Vec::new(), None)
            .await?;

        match response.status() {
            status if status.is_success() => match response.bytes().await {
                Ok(bytes) => Ok(bytes.to_vec()),
                Err(err) => Err(StorageError::Unavailable(err.to_string())),
            },
            StatusCode::NOT_FOUND => Err(StorageError::NotFound {
                key: key.to_owned(),
            }),
            _ => Err(provider_error(response).await),
        }
    }

    /// Remove an object; `true` when one was there.
    pub async fn delete(&self, key: &str) -> Result<bool> {
        validate_key(key)?;
        let response = self
            .execute("DELETE", &self.object_path(key), Vec::new(), None)
            .await?;

        match response.status() {
            status if status.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(provider_error(response).await),
        }
    }

    /// Create the bucket when it does not exist yet.
    pub async fn ensure_bucket(&self) -> Result<()> {
        let path = self.bucket_path();
        let response = self.execute("PUT", &path, Vec::new(), None).await?;

        match response.status() {
            status if status.is_success() => Ok(()),
            // Both answers mean "the bucket is there": MinIO and AWS answer 409 for a bucket
            // this account already owns, 403 when someone else does.
            StatusCode::CONFLICT | StatusCode::FORBIDDEN => Ok(()),
            _ => Err(provider_error(response).await),
        }
    }

    /// Ask for the bucket without reading or writing an object.
    ///
    /// Answers `NotFound` when the bucket is missing and `Unavailable` when the store cannot be
    /// reached at all — the two answers a doctor or a test needs to tell apart.
    pub async fn probe(&self) -> Result<()> {
        let response = self
            .execute("HEAD", &self.bucket_path(), Vec::new(), None)
            .await?;

        match response.status() {
            status if status.is_success() => Ok(()),
            StatusCode::NOT_FOUND => Err(StorageError::NotFound {
                key: self.bucket.clone(),
            }),
            _ => Err(provider_error(response).await),
        }
    }

    /// Path of one object inside the bucket, percent-encoded for the request line.
    fn object_path(&self, key: &str) -> String {
        format!("/{}/{}", self.bucket, signing::encode_path(key))
    }

    /// Path of the bucket itself.
    fn bucket_path(&self) -> String {
        format!("/{}", self.bucket)
    }

    /// Sign and send one request.
    async fn execute(
        &self,
        method: &str,
        path: &str,
        query: Vec<(&str, &str)>,
        body: Option<(&[u8], &str)>,
    ) -> Result<reqwest::Response> {
        let host = origin_host(&self.endpoint)?;
        let payload = body.map_or_else(
            || EMPTY_PAYLOAD_HASH.to_owned(),
            |(bytes, _)| signing::payload_hash(bytes),
        );
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some((_, content_type)) = body {
            headers.push(("content-type", content_type));
        }

        let (datetime, date) = timestamp(OffsetDateTime::now_utc());
        let signed = signing::sign(
            &RequestToSign {
                method,
                host: &host,
                path,
                query,
                headers,
                payload_hash: &payload,
            },
            &self.credentials,
            &self.region,
            &datetime,
            &date,
        );

        let url = if signed.query.is_empty() {
            format!("{}{path}", self.endpoint)
        } else {
            format!("{}{path}?{}", self.endpoint, signed.query)
        };

        let method = method
            .parse::<reqwest::Method>()
            .map_err(|err| StorageError::Invalid(err.to_string()))?;
        let mut header_map = HeaderMap::with_capacity(signed.headers.len());
        for (name, value) in &signed.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|err| StorageError::Invalid(err.to_string()))?;
            let value = HeaderValue::from_str(value)
                .map_err(|err| StorageError::Invalid(err.to_string()))?;
            header_map.insert(name, value);
        }

        let mut request = self.client.request(method, url).headers(header_map);
        if let Some((bytes, _)) = body {
            request = request.body(Bytes::copy_from_slice(bytes));
        }

        request
            .send()
            .await
            .map_err(|err| StorageError::Unavailable(err.to_string()))
    }
}

/// The host of an endpoint, without the scheme and without a path — the `Host` header value.
fn origin_host(endpoint: &str) -> Result<String> {
    let (_, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| StorageError::Invalid(format!("{endpoint:?} does not carry a scheme")))?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(StorageError::Invalid(format!(
            "{endpoint:?} does not name a host"
        )));
    }
    Ok(host.to_owned())
}

/// The SigV4 timestamp pair: basic ISO 8601 and its day part.
fn timestamp(now: OffsetDateTime) -> (String, String) {
    let datetime = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let date = datetime[..8].to_owned();
    (datetime, date)
}

/// Turn a refused response into a provider error, keeping the status and a trimmed message.
async fn provider_error(response: reqwest::Response) -> StorageError {
    let status = response.status().as_u16();
    let message = response
        .text()
        .await
        .unwrap_or_default()
        .trim()
        .chars()
        .take(MAX_PROVIDER_MESSAGE)
        .collect::<String>();
    StorageError::Provider { status, message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;

    fn dev_config() -> StorageConfig {
        StorageConfig::default()
    }

    #[test]
    fn the_client_reads_the_host_out_of_the_endpoint() {
        assert_eq!(
            origin_host("http://127.0.0.1:9000").expect("a plain origin"),
            "127.0.0.1:9000"
        );
        assert_eq!(
            origin_host("https://s3.example.org/").expect("a trailing slash is fine"),
            "s3.example.org"
        );
        assert!(origin_host("127.0.0.1:9000").is_err());
    }

    #[test]
    fn timestamps_are_basic_iso8601_pairs() {
        let (datetime, date) = timestamp(OffsetDateTime::UNIX_EPOCH);
        assert_eq!(datetime, "19700101T000000Z");
        assert_eq!(date, "19700101");
    }

    #[test]
    fn object_paths_carry_the_bucket_and_the_encoded_key() {
        let storage = S3Storage::new(&dev_config()).expect("the dev configuration is valid");
        assert_eq!(storage.bucket(), "omnion-media");
        assert_eq!(
            storage.object_path("sites/a/one.png"),
            "/omnion-media/sites/a/one.png"
        );
        assert_eq!(storage.bucket_path(), "/omnion-media");
    }

    #[test]
    fn keys_are_checked_before_a_request_is_built() {
        let storage = S3Storage::new(&dev_config()).expect("the dev configuration is valid");
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        assert!(matches!(
            runtime.block_on(storage.put("../escape", b"no", "text/plain")),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            runtime.block_on(storage.get("../escape")),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            runtime.block_on(storage.delete("../escape")),
            Err(StorageError::Invalid(_))
        ));
    }
}
