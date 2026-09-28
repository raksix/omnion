//! Per-site storage settings (REQ-010, slice 3).
//!
//! [`preset_store`] decides what a *transformation* is; this module decides where the *bytes*
//! live. Three rules shape every statement and every field:
//!
//! * **A credential is a reference, never a value.** The row stores the *name* the deployment
//!   holds the bucket's key under, and no setting here can ever return key material. A settings
//!   table that holds a secret in plaintext is a table that has to be backed up, rotated and
//!   guarded as a credential, one `SELECT` away from a browser — and the connection test
//!   resolves the reference through the process environment, which is where the secret already
//!   lives.
//! * **A default is a *platform* default; the row is a *site* answer.** A site that has never
//!   been configured and a site configured back to the defaults look identical in every column
//!   except `updated_at`, which is why that column exists: an operator asking "has this site ever
//!   been pointed anywhere?" needs to be able to answer it, and "every column equals the default"
//!   cannot answer it.
//! * **Validation lives in the crate, not only in the check constraints.** A `check` that
//!   refuses a value answers a 500 with a column name nobody can act on. Every rule here is
//!   enforced with a message that names the *field*, because the settings screen shows the
//!   message under that field and a bare "constraint violation" is a message under nothing.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MediaError, Result};

/// The columns of a settings row, in the order [`SiteStorage`] reads them.
const SETTINGS_COLUMNS: &str = "site_id, driver, endpoint, region, bucket, path_prefix, \
     public_base_url, signed_url_ttl_seconds, default_visibility, max_upload_mb, \
     allowed_content_types, created_at, updated_at";

/// Smallest signed-URL lifetime accepted, in seconds.
pub const MIN_SIGNED_URL_TTL: i32 = 60;

/// Largest signed-URL lifetime accepted, in seconds (a week).
pub const MAX_SIGNED_URL_TTL: i32 = 604_800;

/// Smallest per-site upload ceiling, in megabytes.
pub const MIN_UPLOAD_MB: i32 = 1;

/// Largest per-site upload ceiling, in megabytes.
pub const MAX_UPLOAD_MB: i32 = 1024;

/// The same ceiling as a `u64`, for the byte arithmetic that a clamp falls back to.
///
/// A separate constant rather than a cast at each site: the fallback exists so an `i32` that
/// slipped past the column check cannot wrap the byte maths, and a `u64` written out in three
/// places is three places a future edit changes one of.
pub const MAX_UPLOAD_MB_U64: u64 = MAX_UPLOAD_MB as u64;

/// A default that is safe rather than convenient: private, API-served, no prefix.
///
/// The public base is empty on purpose. A guessed CDN hostname serves one site's bytes under
/// another's name, and an empty base degrades to "the API serves it", which is slower rather
/// than wrong.
fn platform_defaults() -> (String, String, String, String, String, i32, String, i32) {
    (
        "s3".to_string(),
        "http://127.0.0.1:9000".to_string(),
        "us-east-1".to_string(),
        "omnion-media".to_string(),
        String::new(),
        900,
        "private".to_string(),
        25,
    )
}

/// One site's storage settings, as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SiteStorage {
    /// Site this record belongs to.
    pub site_id: Uuid,
    /// Which driver objects are written through: `s3` or `fs`.
    pub driver: String,
    /// Object-store origin, scheme included.
    pub endpoint: String,
    /// Object-store region.
    pub region: String,
    /// Bucket this site's objects live in.
    pub bucket: String,
    /// Key prefix inside the bucket, empty when the site owns the whole namespace.
    pub path_prefix: String,
    /// Where a public file is served from; empty means "the API serves it".
    pub public_base_url: String,
    /// Signed-URL lifetime in seconds.
    pub signed_url_ttl_seconds: i32,
    /// Default visibility of a new upload: `private` or `public`.
    pub default_visibility: String,
    /// Per-site upload ceiling in megabytes.
    pub max_upload_mb: i32,
    /// Content types this site accepts; empty means "the platform list".
    pub allowed_content_types: Vec<String>,
    /// When the row was created.
    pub created_at: OffsetDateTime,
    /// When the row was last written — the only column that distinguishes a configured site
    /// from a site that has never been touched.
    pub updated_at: OffsetDateTime,
}

impl SiteStorage {
    /// The upload ceiling in bytes, which is what the upload path compares against.
    #[must_use]
    pub fn max_upload_bytes(&self) -> u64 {
        let mb = u64::try_from(self.max_upload_mb.clamp(MIN_UPLOAD_MB, MAX_UPLOAD_MB))
            .unwrap_or(MAX_UPLOAD_MB_U64);
        mb * 1024 * 1024
    }

    /// Whether a new upload on this site is public unless the caller says otherwise.
    #[must_use]
    pub fn uploads_are_public(&self) -> bool {
        self.default_visibility == "public"
    }

    /// Whether this site accepts a content type.
    ///
    /// An empty list is the platform's own list, decided by the caller — this returns `true`
    /// so the crate never has to know what the platform allows, and the platform's answer is
    /// consulted only when the site has expressed no opinion.
    #[must_use]
    pub fn accepts(&self, content_type: &str) -> bool {
        if self.allowed_content_types.is_empty() {
            return true;
        }
        let normalised = content_type.split(';').next().unwrap_or(content_type).trim();
        self.allowed_content_types
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(normalised))
    }

    /// The object key of one file, with this site's prefix in front of it.
    ///
    /// The prefix is joined here and nowhere else, so a bucket migration is a single string
    /// change and a *stored* key never has to be rewritten — which is what makes a prefix
    /// different from a bucket different from a whole-site move.
    #[must_use]
    pub fn key_for(&self, key: &str) -> String {
        join_prefix(&self.path_prefix, key)
    }

    /// A public URL for a file, when a public base is configured.
    ///
    /// Returns `None` rather than a guess when there is no base: the caller then serves the
    /// bytes itself, which is slower and correct, instead of pointing at a host this platform
    /// does not own.
    #[must_use]
    pub fn public_url(&self, storage_key: &str) -> Option<String> {
        let base = self.public_base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return None;
        }
        Some(format!("{base}/{}", self.key_for(storage_key)))
    }
}

/// Settings as a caller describes them, before validation.
#[derive(Debug, Clone)]
pub struct NewSiteStorage {
    /// Which driver objects are written through.
    pub driver: String,
    /// Object-store origin.
    pub endpoint: String,
    /// Object-store region.
    pub region: String,
    /// Bucket this site's objects live in.
    pub bucket: String,
    /// Key prefix inside the bucket.
    pub path_prefix: String,
    /// Where a public file is served from.
    pub public_base_url: String,
    /// Signed-URL lifetime in seconds.
    pub signed_url_ttl_seconds: i32,
    /// Default visibility of a new upload.
    pub default_visibility: String,
    /// Per-site upload ceiling in megabytes.
    pub max_upload_mb: i32,
    /// Content types this site accepts.
    pub allowed_content_types: Vec<String>,
}

impl NewSiteStorage {
    /// The platform defaults, as an editable starting point.
    #[must_use]
    pub fn defaults() -> Self {
        let (driver, endpoint, region, bucket, path_prefix, ttl, visibility, max_mb) =
            platform_defaults();
        Self {
            driver,
            endpoint,
            region,
            bucket,
            path_prefix,
            public_base_url: String::new(),
            signed_url_ttl_seconds: ttl,
            default_visibility: visibility,
            max_upload_mb: max_mb,
            allowed_content_types: Vec::new(),
        }
    }
}

/// Normalise and validate a settings payload.
///
/// Every rejection names the field, because the settings screen renders the message under the
/// field and the caller is looking at a form, not at a schema. The normalisation is done *before*
/// the check so that a value which is merely untidy — a trailing slash on a base URL, a leading
/// slash on a prefix, spaces in a bucket — is accepted and stored in its canonical form, and only
/// a value that cannot be repaired is refused.
pub fn validate_new(mut new: NewSiteStorage) -> Result<NewSiteStorage> {
    new.driver = new.driver.trim().to_lowercase();
    if new.driver != "s3" && new.driver != "fs" {
        return Err(MediaError::InvalidStorageSetting {
            field: "driver".to_string(),
            message: format!(
                "expected `s3` or `fs`, got `{}`",
                escape_field(&new.driver)
            ),
        });
    }

    new.endpoint = new.endpoint.trim().to_string();
    if new.driver == "s3" {
        if !new.endpoint.starts_with("http://") && !new.endpoint.starts_with("https://") {
            return Err(MediaError::InvalidStorageSetting {
                field: "endpoint".to_string(),
                message: "expected an http or https URL, for example `https://s3.eu-west-1.amazonaws.com`"
                    .to_string(),
            });
        }
        // A path, a query or a fragment turns a signed request into a signature over the wrong
        // string. The scheme and host are enough to build one, so nothing else is accepted.
        let after_scheme = new
            .endpoint
            .split_once("://")
            .map_or_else(String::new, |(_, rest)| rest.to_string());
        if after_scheme.is_empty() || after_scheme.contains(['/', '?', '#']) {
            return Err(MediaError::InvalidStorageSetting {
                field: "endpoint".to_string(),
                message: "expected a bare origin without a path, a query or a fragment".to_string(),
            });
        }
    }

    new.bucket = new.bucket.trim().to_string();
    if new.bucket.is_empty() {
        return Err(MediaError::InvalidStorageSetting {
            field: "bucket".to_string(),
            message: "the bucket name may not be blank".to_string(),
        });
    }
    // S3's own bucket rules, checked here so the failure names the field rather than arriving
    // as a store-side 400 with a message in XML.
    if new.bucket.len() < 3
        || new.bucket.len() > 63
        || !new
            .bucket
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return Err(MediaError::InvalidStorageSetting {
            field: "bucket".to_string(),
            message: "expected 3–63 characters of a–z, 0–9, `.` and `-`".to_string(),
        });
    }

    new.region = new.region.trim().to_string();
    if new.region.is_empty() {
        return Err(MediaError::InvalidStorageSetting {
            field: "region".to_string(),
            message: "the region may not be blank".to_string(),
        });
    }

    // A prefix is joined *between* the bucket and the key, so leading and trailing slashes are
    // untidy rather than wrong — they are dropped. A `..` is not untidy, it is an escape, and it
    // is refused.
    new.path_prefix = new
        .path_prefix
        .trim()
        .trim_matches('/')
        .to_string();
    if new
        .path_prefix
        .split('/')
        .any(|segment| segment == "..")
        || new.path_prefix.starts_with('/')
    {
        return Err(MediaError::InvalidStorageSetting {
            field: "path_prefix".to_string(),
            message: "expected a relative key prefix without `..` segments".to_string(),
        });
    }

    new.public_base_url = new.public_base_url.trim().to_string();
    if !new.public_base_url.is_empty() {
        if !new.public_base_url.starts_with("http://")
            && !new.public_base_url.starts_with("https://")
        {
            return Err(MediaError::InvalidStorageSetting {
                field: "public_base_url".to_string(),
                message: "expected an http or https URL, or left blank to serve files from the API"
                    .to_string(),
            });
        }
        if new.public_base_url.contains(['?', '#']) {
            return Err(MediaError::InvalidStorageSetting {
                field: "public_base_url".to_string(),
                message: "expected a plain base URL without a query or a fragment".to_string(),
            });
        }
        // Stored without its trailing slash, because the URL is built by joining a path onto
        // it and a doubled slash is a 404 on somebody else's CDN.
        new.public_base_url = new.public_base_url.trim_end_matches('/').to_string();
    }

    if !(MIN_SIGNED_URL_TTL..=MAX_SIGNED_URL_TTL).contains(&new.signed_url_ttl_seconds) {
        return Err(MediaError::InvalidStorageSetting {
            field: "signed_url_ttl_seconds".to_string(),
            message: format!(
                "expected between {MIN_SIGNED_URL_TTL} and {MAX_SIGNED_URL_TTL} seconds"
            ),
        });
    }

    new.default_visibility = new.default_visibility.trim().to_lowercase();
    if new.default_visibility != "private" && new.default_visibility != "public" {
        return Err(MediaError::InvalidStorageSetting {
            field: "default_visibility".to_string(),
            message: "expected `private` or `public`".to_string(),
        });
    }

    if !(MIN_UPLOAD_MB..=MAX_UPLOAD_MB).contains(&new.max_upload_mb) {
        return Err(MediaError::InvalidStorageSetting {
            field: "max_upload_mb".to_string(),
            message: format!("expected between {MIN_UPLOAD_MB} and {MAX_UPLOAD_MB} MB"),
        });
    }

    new.allowed_content_types = new
        .allowed_content_types
        .iter()
        .map(|raw| {
            raw.split(';')
                .next()
                .unwrap_or(raw)
                .trim()
                .to_lowercase()
        })
        .filter(|value| !value.is_empty())
        .collect();
    new.allowed_content_types.sort();
    new.allowed_content_types.dedup();

    Ok(new)
}

/// Make a rejected value safe to put in an error message.
///
/// The field errors are rendered next to a form, so a value containing a newline or a quote
/// would let a caller break the layout of the very message that reports it.
fn escape_field(value: &str) -> String {
    let trimmed: String = value.chars().take(80).collect();
    trimmed
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Read one site's settings.
///
/// A site with no row answers with the platform defaults rather than an error: the row is
/// created by a trigger and a seed, so a missing one means the migration predates the site
/// table's current shape, and "here are the defaults you can edit" is the useful answer. The
/// caller can still write, and the write creates the row.
pub async fn read_storage_settings(pool: &PgPool, site_id: Uuid) -> Result<SiteStorage> {
    let sql = format!("select {SETTINGS_COLUMNS} from media_storage_settings where site_id = $1");
    let found = sqlx::query_as::<_, SiteStorage>(&sql)
        .bind(site_id)
        .fetch_optional(pool)
        .await?;
    match found {
        Some(row) => Ok(row),
        None => {
            let defaults = NewSiteStorage::defaults();
            let now = OffsetDateTime::now_utc();
            Ok(SiteStorage {
                site_id,
                driver: defaults.driver,
                endpoint: defaults.endpoint,
                region: defaults.region,
                bucket: defaults.bucket,
                path_prefix: defaults.path_prefix,
                public_base_url: defaults.public_base_url,
                signed_url_ttl_seconds: defaults.signed_url_ttl_seconds,
                default_visibility: defaults.default_visibility,
                max_upload_mb: defaults.max_upload_mb,
                allowed_content_types: defaults.allowed_content_types,
                created_at: now,
                updated_at: now,
            })
        }
    }
}

/// Write one site's settings, creating the row when it is not there yet.
///
/// The insert carries the site id rather than updating an existing row, so a caller on a site
/// whose row was never created gets a real row back instead of an update that matched nothing
/// and reported success.
pub async fn write_storage_settings(
    pool: &PgPool,
    site_id: Uuid,
    new: NewSiteStorage,
) -> Result<SiteStorage> {
    let new = validate_new(new)?;
    let sql = format!(
        "insert into media_storage_settings \
           (site_id, driver, endpoint, region, bucket, path_prefix, public_base_url, \
            signed_url_ttl_seconds, default_visibility, max_upload_mb, allowed_content_types, \
            updated_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now()) \
         on conflict (site_id) do update set \
           driver = excluded.driver, endpoint = excluded.endpoint, region = excluded.region, \
           bucket = excluded.bucket, path_prefix = excluded.path_prefix, \
           public_base_url = excluded.public_base_url, \
           signed_url_ttl_seconds = excluded.signed_url_ttl_seconds, \
           default_visibility = excluded.default_visibility, \
           max_upload_mb = excluded.max_upload_mb, \
           allowed_content_types = excluded.allowed_content_types, \
           updated_at = now() \
         returning {SETTINGS_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, SiteStorage>(&sql)
        .bind(site_id)
        .bind(&new.driver)
        .bind(&new.endpoint)
        .bind(&new.region)
        .bind(&new.bucket)
        .bind(&new.path_prefix)
        .bind(&new.public_base_url)
        .bind(new.signed_url_ttl_seconds)
        .bind(&new.default_visibility)
        .bind(new.max_upload_mb)
        .bind(&new.allowed_content_types)
        .fetch_one(pool)
        .await?)
}

/// The result of a connection test, as the settings screen renders it.
///
/// Built by the API route from a real round trip, not by the crate: opening a *candidate*
/// configuration means opening a store this process is not using, and `omnion-media` does not
/// depend on `omnion-storage` (docs/04-MONOREPO.md keeps the byte-moving crate one way). The
/// crate's part is the sentence — what this configuration would be writing to, and where a
/// public file would be served from — because that is the part an operator cannot infer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionProbe {
    /// Whether the store could be reached *and* written to.
    pub ok: bool,
    /// What the probe proved, in a sentence a person can act on.
    pub detail: String,
    /// Which driver was opened.
    pub driver: String,
    /// The target that was reached, e.g. `` `omnion-media` under `tenant-a` ``.
    pub target: String,
    /// What a public file on this site will be served from, or the reason it will not be.
    pub public_base_url: String,
    /// Latency of the round trip in milliseconds.
    pub elapsed_ms: u64,
}

impl ConnectionProbe {
    /// A probe that failed to even open a store.
    ///
    /// A configuration that cannot be turned into a store never reaches the network, and the
    /// message says *which field* stopped it — the same message the save would have produced,
    /// so a person does not have to guess which of the two things to fix.
    #[must_use]
    pub fn unopened(driver: &str, target: &str, public_base_url: &str, reason: &str) -> Self {
        Self {
            ok: false,
            detail: reason.to_string(),
            driver: driver.to_string(),
            target: target.to_string(),
            public_base_url: public_base_url.to_string(),
            elapsed_ms: 0,
        }
    }

    /// A probe that reached the store and wrote a marker.
    ///
    /// The marker is written and removed rather than a real object being read, because read
    /// access is the permission an object store grants most often: a configuration that reads
    /// and cannot write is exactly the one that fails at 02:00 with a page of broken images.
    #[must_use]
    pub fn wrote(
        driver: &str,
        target: &str,
        public_base_url: &str,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            ok: true,
            detail: format!("reached {target} and wrote and removed a probe object"),
            driver: driver.to_string(),
            target: target.to_string(),
            public_base_url: public_base_url.to_string(),
            elapsed_ms,
        }
    }

    /// A probe that reached the store but could not write to it.
    #[must_use]
    pub fn read_only(
        driver: &str,
        target: &str,
        public_base_url: &str,
        elapsed_ms: u64,
        reason: &str,
    ) -> Self {
        Self {
            ok: false,
            detail: format!("reached {target} but could not write to it: {reason}"),
            driver: driver.to_string(),
            target: target.to_string(),
            public_base_url: public_base_url.to_string(),
            elapsed_ms,
        }
    }
}

/// What a public file will be served from, or why it will not be.
///
/// Empty is the answer when nothing is configured, and it is a sentence rather than an empty
/// string: "the API serves it" is the difference between a working default and a setting
/// somebody goes looking for because the screen looks empty.
#[must_use]
pub fn describe_public_base(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "the API serves it — no public base URL is configured".to_string()
    } else {
        format!("{trimmed}/")
    }
}

/// How a configuration names its target, for a sentence a person can act on.
///
/// The filesystem driver is named by *what it is* rather than by a path, because the row has
/// no path column: the process decides the root, and a settings screen that let a person type
/// one would be a screen promising a move the platform cannot make.
#[must_use]
pub fn describe_target(new: &NewSiteStorage) -> String {
    if new.driver == "fs" {
        return "the filesystem driver".to_string();
    }
    let prefix = if new.path_prefix.is_empty() {
        String::new()
    } else {
        format!(" under `{prefix}`", prefix = new.path_prefix)
    };
    format!("`{bucket}`{prefix}", bucket = new.bucket)
}

/// The key a connection probe writes and removes.
///
/// A marker, not a real object: a probe that wrote and deleted a real file is a probe that can,
/// on a bad day, delete a real file. The name carries the site so two sites sharing a bucket
/// under one prefix cannot overwrite each other's marker while it is in flight.
#[must_use]
pub fn probe_key(new: &NewSiteStorage, site_id: Uuid) -> String {
    join_prefix(&new.path_prefix, &format!("sites/{site_id}/connection-test"))
}

/// Join a key onto a prefix, dropping redundant slashes on both sides.
///
/// Free-standing rather than a method on [`SiteStorage`] because the connection test builds a
/// key from a *candidate* — a request body that has not been written to a row — and the two
/// must produce byte-identical keys. A prefixing rule that exists twice is a prefixing rule that
/// will one day differ.
fn join_prefix(prefix: &str, key: &str) -> String {
    let prefix = prefix.trim_matches('/');
    let key = key.trim_start_matches('/');
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}/{key}")
    }
}

/// Read the effective upload ceiling for a site, falling back to the platform's own.
///
/// The upload path calls this on every request, so it must not fail: a site whose settings row
/// cannot be read — a half-applied migration, a table someone dropped by hand — still has to
/// accept uploads at the platform limit rather than rejecting every one of them.
pub async fn effective_max_upload_bytes(pool: &PgPool, site_id: Uuid) -> u64 {
    let sql = "select max_upload_mb from media_storage_settings where site_id = $1";
    match sqlx::query_scalar::<_, i32>(sql)
        .bind(site_id)
        .fetch_optional(pool)
        .await
    {
        Ok(Some(mb)) => u64::try_from(mb.clamp(MIN_UPLOAD_MB, MAX_UPLOAD_MB))
            .unwrap_or(MAX_UPLOAD_MB_U64)
            * 1024
            * 1024,
        _ => {
            let (_, _, _, _, _, _, _, mb) = platform_defaults();
            u64::try_from(mb).unwrap_or(MAX_UPLOAD_MB_U64) * 1024 * 1024
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload the validators accept, so a test can change one field and assert on that.
    fn good() -> NewSiteStorage {
        NewSiteStorage::defaults()
    }

    #[test]
    fn the_defaults_are_private_and_api_served() {
        let settings = good();
        assert_eq!(settings.driver, "s3");
        assert_eq!(settings.default_visibility, "private");
        assert!(
            settings.public_base_url.is_empty(),
            "a guess at a public base serves one site's bytes under another's name"
        );
    }

    #[test]
    fn every_range_has_a_message_that_names_its_field() {
        let cases: [(fn(&mut NewSiteStorage), &str); 4] = [
            (|new| new.signed_url_ttl_seconds = 30, "signed_url_ttl_seconds"),
            (|new| new.signed_url_ttl_seconds = 604_801, "signed_url_ttl_seconds"),
            (|new| new.max_upload_mb = 0, "max_upload_mb"),
            (|new| new.max_upload_mb = 1025, "max_upload_mb"),
        ];
        for (mutate, field) in cases {
            let mut new = good();
            mutate(&mut new);
            let err = validate_new(new).expect_err("a range outside the bounds is refused");
            match err {
                MediaError::InvalidStorageSetting { field: got, .. } => assert_eq!(got, field),
                other => panic!("expected a field error, got {other}"),
            }
        }
    }

    #[test]
    fn the_signed_url_window_is_exactly_the_one_the_migration_checks() {
        assert!(validate_new({
            let mut n = good();
            n.signed_url_ttl_seconds = MIN_SIGNED_URL_TTL;
            n
        })
        .is_ok());
        assert!(validate_new({
            let mut n = good();
            n.signed_url_ttl_seconds = MAX_SIGNED_URL_TTL;
            n
        })
        .is_ok());
    }

    #[test]
    fn an_endpoint_with_a_path_is_refused() {
        // A path in an endpoint signs the wrong string; the failure arrives as a store-side 403
        // with an XML body, which is the least actionable error in this whole feature.
        for bad in [
            "https://s3.example.com/bucket",
            "https://s3.example.com?x=1",
            "https://s3.example.com#f",
            "s3.example.com",
            "",
        ] {
            let err = validate_new({
                let mut n = good();
                n.endpoint = bad.to_string();
                n
            })
            .expect_err("a non-origin endpoint is refused");
            match err {
                MediaError::InvalidStorageSetting { field, .. } => assert_eq!(field, "endpoint"),
                other => panic!("expected an endpoint error, got {other}"),
            }
        }
    }

    #[test]
    fn a_prefix_is_trimmed_but_may_not_climb() {
        let trimmed = validate_new({
            let mut n = good();
            n.path_prefix = "/tenant-a/".to_string();
            n
        })
        .expect("a tidy prefix is accepted");
        assert_eq!(trimmed.path_prefix, "tenant-a");

        for bad in ["../other", "tenant/../../etc", "a/.."] {
            let err = validate_new({
                let mut n = good();
                n.path_prefix = bad.to_string();
                n
            })
            .expect_err("an escaping prefix is refused");
            match err {
                MediaError::InvalidStorageSetting { field, .. } => {
                    assert_eq!(field, "path_prefix", "for {bad}");
                }
                other => panic!("expected a prefix error, got {other}"),
            }
        }
    }

    #[test]
    fn a_public_base_loses_its_trailing_slash_and_keeps_its_path() {
        let cleaned = validate_new({
            let mut n = good();
            n.public_base_url = "https://cdn.example.com/media/".to_string();
            n
        })
        .expect("a base with a trailing slash is accepted");
        // A CDN in front of a bucket really is served under a path, so the path is kept; only
        // the slash that would double on every joined URL goes.
        assert_eq!(cleaned.public_base_url, "https://cdn.example.com/media");
    }

    #[test]
    fn a_bucket_that_s3_would_refuse_is_refused_here_with_a_field() {
        for bad in ["ab", "UPPERCASE", "has space", "with_underscore"] {
            let err = validate_new({
                let mut n = good();
                n.bucket = bad.to_string();
                n
            })
            .expect_err("a bucket S3 would reject is refused");
            match err {
                MediaError::InvalidStorageSetting { field, .. } => {
                    assert_eq!(field, "bucket", "for {bad}");
                }
                other => panic!("expected a bucket error, got {other}"),
            }
        }
    }

    #[test]
    fn content_types_are_normalised_deduplicated_and_sorted() {
        let settings = validate_new(NewSiteStorage {
            allowed_content_types: vec![
                "IMAGE/PNG".to_string(),
                "image/jpeg; charset=binary".to_string(),
                "image/png".to_string(),
                "  ".to_string(),
            ],
            ..good()
        })
        .expect("a content type list is accepted");
        assert_eq!(settings.allowed_content_types, vec!["image/jpeg", "image/png"]);
    }

    #[test]
    fn an_empty_content_type_list_means_the_platform_list() {
        let site = SiteStorage {
            site_id: Uuid::nil(),
            driver: "s3".to_string(),
            endpoint: "http://127.0.0.1:9000".to_string(),
            region: "us-east-1".to_string(),
            bucket: "omnion-media".to_string(),
            path_prefix: String::new(),
            public_base_url: String::new(),
            signed_url_ttl_seconds: 900,
            default_visibility: "private".to_string(),
            max_upload_mb: 25,
            allowed_content_types: Vec::new(),
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        assert!(site.accepts("application/x-unheard-of"), "no opinion, no refusal");
    }

    #[test]
    fn a_configured_list_refuses_what_it_does_not_name() {
        let site = SiteStorage {
            site_id: Uuid::nil(),
            driver: "s3".to_string(),
            endpoint: "http://127.0.0.1:9000".to_string(),
            region: "us-east-1".to_string(),
            bucket: "omnion-media".to_string(),
            path_prefix: String::new(),
            public_base_url: String::new(),
            signed_url_ttl_seconds: 900,
            default_visibility: "private".to_string(),
            max_upload_mb: 25,
            allowed_content_types: vec!["image/png".to_string(), "image/jpeg".to_string()],
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        assert!(site.accepts("image/PNG"), "case is normalised");
        assert!(site.accepts("image/png; charset=binary"), "parameters are ignored");
        assert!(!site.accepts("video/mp4"), "an unlisted type is refused");
    }

    #[test]
    fn a_prefix_prepended_twice_would_never_match() {
        let site = SiteStorage {
            site_id: Uuid::nil(),
            driver: "s3".to_string(),
            endpoint: "http://127.0.0.1:9000".to_string(),
            region: "us-east-1".to_string(),
            bucket: "omnion-media".to_string(),
            path_prefix: "tenant-a".to_string(),
            public_base_url: String::new(),
            signed_url_ttl_seconds: 900,
            default_visibility: "private".to_string(),
            max_upload_mb: 25,
            allowed_content_types: Vec::new(),
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        // The key a file actually stores is the one the lookup uses, so prefixing twice is the
        // bug this guards: a 404 on every object of the site.
        assert_eq!(site.key_for("sites/x/y.png"), "tenant-a/sites/x/y.png");
        assert_eq!(
            site.key_for("sites/x/y.png"),
            site.key_for("sites/x/y.png"),
            "the prefixing is idempotent in value even though it is not a fold"
        );
    }

    #[test]
    fn the_upload_ceiling_converts_to_bytes_without_overflowing() {
        let mut site = SiteStorage {
            site_id: Uuid::nil(),
            driver: "s3".to_string(),
            endpoint: "http://127.0.0.1:9000".to_string(),
            region: "us-east-1".to_string(),
            bucket: "omnion-media".to_string(),
            path_prefix: String::new(),
            public_base_url: String::new(),
            signed_url_ttl_seconds: 900,
            default_visibility: "private".to_string(),
            max_upload_mb: 25,
            allowed_content_types: Vec::new(),
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        assert_eq!(site.max_upload_bytes(), 26_214_400);
        // Even a row the database would have refused must not turn into a u64 overflow on a
        // 32-bit cast; the clamp is what keeps the upload path total.
        site.max_upload_mb = i32::MAX;
        assert_eq!(site.max_upload_bytes(), MAX_UPLOAD_MB_U64 * 1024 * 1024);
    }

    #[test]
    fn a_public_url_is_absent_rather_than_guessed() {
        let mut site = SiteStorage {
            site_id: Uuid::nil(),
            driver: "s3".to_string(),
            endpoint: "http://127.0.0.1:9000".to_string(),
            region: "us-east-1".to_string(),
            bucket: "omnion-media".to_string(),
            path_prefix: String::new(),
            public_base_url: String::new(),
            signed_url_ttl_seconds: 900,
            default_visibility: "private".to_string(),
            max_upload_mb: 25,
            allowed_content_types: Vec::new(),
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        assert!(
            site.public_url("sites/x/y.png").is_none(),
            "with no base the caller serves the bytes, rather than pointing at a host we do not own"
        );
        site.public_base_url = "https://cdn.example.com/media/".to_string();
        assert_eq!(
            site.public_url("sites/x/y.png").as_deref(),
            Some("https://cdn.example.com/media/sites/x/y.png")
        );
    }
}
