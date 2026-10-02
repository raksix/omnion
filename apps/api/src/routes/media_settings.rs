//! `/api/v1/media/settings` and the connection test (REQ-010, slice 3).
//!
//! The preset routes answer *what a transformation is*; this one answers *where the bytes live*.
//! Four rules hold across this file, and each is a place the obvious shortcut is wrong:
//!
//! * **No response ever carries a credential.** The settings table stores a *reference* to a
//!   secret the deployment already holds, and the `SettingsBody` below has no field that could
//!   hold key material — so "no settings response ever returns a credential value" is a
//!   property of the type, not of a review that has to remember to strip a field.
//! * **The connection test writes.** A test that only reads proves the credentials can list,
//!   and read is the permission an object store grants most often. The marker is written and
//!   removed under a key that belongs to no file, so a bad day cannot delete a real object.
//! * **The test is answered for the configuration as saved.** A screen that shows "connected"
//!   next to unsaved edits is a screen that lies: the person would fix the wrong field. The
//!   probe runs against the *posted* candidate, validated the same way a save validates it.
//! * **A field error is a field error.** Every refusal carries the wire name of the setting and
//!   the screen renders the message under that input, so the same message is produced by the
//!   save and by the test.

use axum::Json;
use axum::extract::{Query, State};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_media::{
    ConnectionProbe, NewSiteStorage, describe_public_base, describe_target, read_storage_settings,
    write_storage_settings,
};
use omnion_storage::{Storage, StorageConfig, StorageDriver};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::media::site_in_scope;
use crate::routes::media_transform::SiteQuery;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// A site's storage settings, as the screen reads them.
///
/// There is deliberately no `secret` field, no `access_key` field and no `credentials` field.
/// The one thing a settings screen must never be able to render is the thing the type does not
/// have room for.
#[derive(Debug, Serialize)]
pub struct SettingsBody {
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
    /// Where a public file is served from; empty means the API serves it.
    pub public_base_url: String,
    /// What a public file will actually be served from, in words.
    pub public_base_summary: String,
    /// Signed-URL lifetime in seconds.
    pub signed_url_ttl_seconds: i32,
    /// Default visibility of a new upload.
    pub default_visibility: String,
    /// Per-site upload ceiling in megabytes.
    pub max_upload_mb: i32,
    /// Content types this site accepts; empty means the platform list.
    pub allowed_content_types: Vec<String>,
    /// Whether this row has ever been written since it was created.
    ///
    /// "Every column equals the default" cannot answer "has this site ever been pointed
    /// anywhere", and that is the question an operator opens this screen with.
    pub configured: bool,
    /// How a public file on this site is served — never a guess, always a sentence.
    pub visibility_note: String,
}

impl SettingsBody {
    /// Describe a stored row for the panel.
    fn build(settings: &omnion_media::SiteStorage, created_at: time::OffsetDateTime) -> Self {
        // `configured` compares against the row's own creation instant rather than a stored
        // boolean: a row that was inserted by the trigger and never edited has the two
        // timestamps equal, and a row somebody saved has them apart.
        //
        // The question has to be "were these two instants ever different", **not** "are they
        // at least a second apart". Rounding the gap down to whole seconds first made this a
        // race against the clock: a save followed by the read that observes it inside the same
        // second leaves a difference of a few hundred microseconds, `whole_seconds()`
        // truncated it to `0`, and a site that had just been configured reported itself as
        // never touched. The panel then shows "you have not set this up yet" over a working
        // configuration — the one screen where a wrong `false` destroys the operator's trust in
        // every other value on the page.
        let configured = settings.updated_at != created_at;
        let public_base_summary = describe_public_base(&settings.public_base_url);
        let visibility_note = if settings.uploads_are_public() {
            "New uploads on this site default to public. Anyone holding the file's URL can \
             fetch it without a session — use this only for a bucket you are certain is public."
                .to_string()
        } else {
            "New uploads on this site default to private: they are served through a signed URL \
             that expires."
                .to_string()
        };
        Self {
            driver: settings.driver.clone(),
            endpoint: settings.endpoint.clone(),
            region: settings.region.clone(),
            bucket: settings.bucket.clone(),
            path_prefix: settings.path_prefix.clone(),
            public_base_url: settings.public_base_url.clone(),
            public_base_summary,
            signed_url_ttl_seconds: settings.signed_url_ttl_seconds,
            default_visibility: settings.default_visibility.clone(),
            max_upload_mb: settings.max_upload_mb,
            allowed_content_types: settings.allowed_content_types.clone(),
            configured,
            visibility_note,
        }
    }
}

/// Settings as the caller describes them.
///
/// Every field is optional with a default taken from the stored row, so a client that PUTs a
/// partial body does not silently reset the fields it did not send — which is how a settings
/// screen "saves" and loses the bucket because the form forgot one input.
#[derive(Debug, Deserialize)]
pub struct SettingsInput {
    /// Which driver objects are written through.
    #[serde(default)]
    pub driver: Option<String>,
    /// Object-store origin.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Object-store region.
    #[serde(default)]
    pub region: Option<String>,
    /// Bucket this site's objects live in.
    #[serde(default)]
    pub bucket: Option<String>,
    /// Key prefix inside the bucket.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Where a public file is served from.
    #[serde(default)]
    pub public_base_url: Option<String>,
    /// Signed-URL lifetime in seconds.
    #[serde(default)]
    pub signed_url_ttl_seconds: Option<i32>,
    /// Default visibility of a new upload.
    #[serde(default)]
    pub default_visibility: Option<String>,
    /// Per-site upload ceiling in megabytes.
    #[serde(default)]
    pub max_upload_mb: Option<i32>,
    /// Content types this site accepts.
    #[serde(default)]
    pub allowed_content_types: Option<Vec<String>>,
}

impl SettingsInput {
    /// Fold a partial body onto the row as it stands.
    ///
    /// The merge happens *before* validation, so a form that sends six of ten fields saves the
    /// other four unchanged rather than resetting them to a platform default — a reset is
    /// invisible until a site stops serving its files.
    fn onto(self, current: &omnion_media::SiteStorage) -> NewSiteStorage {
        let pick = |sent: Option<String>, stored: &str| sent.unwrap_or_else(|| stored.to_owned());
        let current_types = current.allowed_content_types.clone();
        NewSiteStorage {
            driver: pick(self.driver, &current.driver),
            endpoint: pick(self.endpoint, &current.endpoint),
            region: pick(self.region, &current.region),
            bucket: pick(self.bucket, &current.bucket),
            path_prefix: pick(self.path_prefix, &current.path_prefix),
            public_base_url: pick(self.public_base_url, &current.public_base_url),
            signed_url_ttl_seconds: self
                .signed_url_ttl_seconds
                .unwrap_or(current.signed_url_ttl_seconds),
            default_visibility: pick(self.default_visibility, &current.default_visibility),
            max_upload_mb: self.max_upload_mb.unwrap_or(current.max_upload_mb),
            allowed_content_types: self.allowed_content_types.unwrap_or(current_types),
        }
    }
}

/// The answer to a connection test.
#[derive(Debug, Serialize)]
pub struct ProbeBody {
    /// Whether the store was reached and written to.
    pub ok: bool,
    /// What the probe proved, in a sentence a person can act on.
    pub detail: String,
    /// Which driver was opened.
    pub driver: String,
    /// The target that was reached.
    pub target: String,
    /// Where a public file on this site will be served from.
    pub public_base_url: String,
    /// Latency of the round trip.
    pub elapsed_ms: u64,
}

impl From<ConnectionProbe> for ProbeBody {
    fn from(probe: ConnectionProbe) -> Self {
        Self {
            ok: probe.ok,
            detail: probe.detail,
            driver: probe.driver,
            target: probe.target,
            public_base_url: probe.public_base_url,
            elapsed_ms: probe.elapsed_ms,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// A site's storage settings.
pub async fn read(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SiteQuery>,
) -> std::result::Result<Json<SettingsBody>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let settings = read_storage_settings(state.db().pool(), site.id).await?;
    Ok(Json(SettingsBody::build(&settings, settings.created_at)))
}

/// Save a site's storage settings.
///
/// Audited and announced, because this row decides where a live site's files are read from and
/// written to. The audit entry names the fields that *changed* rather than the whole body: a
/// log that repeats ten unchanged columns on every save is a log nobody reads.
pub async fn write(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<SiteQuery>,
    Json(input): Json<SettingsInput>,
) -> std::result::Result<Json<SettingsBody>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let before = read_storage_settings(state.db().pool(), site.id).await?;
    let saved = write_storage_settings(state.db().pool(), site.id, input.onto(&before)).await?;

    let changed = changed_fields(&before, &saved);
    if !changed.is_empty() {
        bus::emit(
            state.db().pool(),
            NewEvent::new("media.storage_updated")
                .organization(site.organization_id)
                .site(site.id)
                .actor(current.user.id)
                // The payload names the fields, never the endpoint or the prefix values: an
                // event fan-out carries this to webhooks, and a private bucket's host name is
                // not something to hand to a third party's endpoint.
                .payload(json!({ "site_id": site.id, "fields": changed })),
        )
        .await?;

        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "media.storage_updated")
                .target("site", site.id.to_string())
                .metadata(json!({
                    "site_id": site.id,
                    "fields": changed,
                    "bucket_changed": before.bucket != saved.bucket,
                }))
                .ip_address(address.as_text())
                .organization(site.organization_id),
        )
        .await?;
    }

    Ok(Json(SettingsBody::build(&saved, before.created_at)))
}

/// Prove that a configuration reaches a bucket.
///
/// The test runs against **the candidate the caller posted**, not against what is stored: the
/// person clicking "Test connection" has a form in front of them with unsaved edits, and a
/// result that describes the *saved* row is a result about a configuration they are no longer
/// looking at. The candidate is validated exactly as a save validates it, so the two paths
/// cannot disagree about which values are acceptable.
pub async fn test_connection(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<SiteQuery>,
    Json(input): Json<SettingsInput>,
) -> std::result::Result<Json<ProbeBody>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let stored = read_storage_settings(state.db().pool(), site.id).await?;
    let candidate = omnion_media::validate_storage(input.onto(&stored))?;

    let target = describe_target(&candidate);
    let public_base = describe_public_base(&candidate.public_base_url);
    let start = std::time::Instant::now();

    let probe = match open_candidate(&candidate) {
        Err(reason) => ConnectionProbe::unopened(&candidate.driver, &target, &public_base, &reason),
        Ok(store) => {
            let key = omnion_media::probe_key(&candidate, site.id);
            match store.put(&key, PROBE_BODY, "text/plain").await {
                Ok(_) => {
                    // The marker is removed whatever the read-back says: a probe that leaves an
                    // object behind is a probe that fills a bucket with one small file per
                    // click, and "Test connection" is a button people press repeatedly.
                    let removed = store.delete(&key).await;
                    let elapsed = elapsed_ms(start);
                    match removed {
                        // A store that accepts writes and refuses deletes is a store that will
                        // fill up, and saying so is more useful than a green tick.
                        Ok(false) => ConnectionProbe::read_only(
                            &candidate.driver,
                            &target,
                            &public_base,
                            elapsed,
                            "the probe object was written but could not be removed — this \
                             credential may not have delete permission",
                        ),
                        Ok(true) => ConnectionProbe::wrote(
                            &candidate.driver,
                            &target,
                            &public_base,
                            elapsed,
                        ),
                        Err(reason) => ConnectionProbe::read_only(
                            &candidate.driver,
                            &target,
                            &public_base,
                            elapsed,
                            &format!("the probe object could not be removed: {reason}"),
                        ),
                    }
                }
                Err(err) => ConnectionProbe::read_only(
                    &candidate.driver,
                    &target,
                    &public_base,
                    elapsed_ms(start),
                    &err.to_string(),
                ),
            }
        }
    };

    // A probe that failed is a fact about a configuration somebody is about to save, so it is
    // audited; a probe that passed is not, because "someone clicked a button" is not an event.
    if !probe.ok {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "media.storage_test_failed")
                .target("site", site.id.to_string())
                .metadata(json!({ "site_id": site.id, "detail": probe.detail }))
                .ip_address(address.as_text())
                .organization(site.organization_id),
        )
        .await?;
    }

    Ok(Json(ProbeBody::from(probe)))
}

/// The bytes a connection probe writes and removes.
const PROBE_BODY: &[u8] = b"omnion media connection test";

/// Milliseconds since `start`, saturating rather than wrapping.
fn elapsed_ms(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Open a store for a candidate configuration.
///
/// The credentials come from the process environment, never from the request and never from the
/// row — a request body is the least trustworthy place a secret can arrive from, and the
/// settings table holds a *reference* precisely so the secret does not have to travel. A
/// deployment that keeps no matching key is told so in a sentence instead of getting a 500 from
/// a client library.
fn open_candidate(candidate: &NewSiteStorage) -> std::result::Result<Storage, String> {
    let driver = StorageDriver::parse(&candidate.driver)
        .map_err(|reason: omnion_storage::StorageError| reason.to_string())?;
    let config = StorageConfig {
        driver,
        endpoint: candidate.endpoint.clone(),
        region: candidate.region.clone(),
        bucket: candidate.bucket.clone(),
        // The credentials are the process's own: the row points at a bucket, and this process
        // holds the key for it. In a development stack that is the compose default; in a
        // deployment it is whatever `OMNION_S3_*` names.
        access_key: std::env::var("OMNION_S3_ACCESS_KEY")
            .unwrap_or_else(|_| omnion_storage::DEFAULT_S3_ACCESS_KEY.to_owned()),
        secret_key: std::env::var("OMNION_S3_SECRET_KEY")
            .unwrap_or_else(|_| omnion_storage::DEFAULT_S3_SECRET_KEY.to_owned()),
        root: std::env::var("OMNION_STORAGE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(omnion_storage::DEFAULT_STORAGE_DIR)),
    };
    // The store's own message is passed through, because "connection refused" is the whole
    // answer when the endpoint is wrong, and a generic "could not connect" throws it away.
    Storage::from_config(&config).map_err(|reason| reason.to_string())
}

/// Which fields a save actually changed, as a JSON array of wire names.
fn changed_fields(
    before: &omnion_media::SiteStorage,
    after: &omnion_media::SiteStorage,
) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if before.driver != after.driver {
        changed.push("driver");
    }
    if before.endpoint != after.endpoint {
        changed.push("endpoint");
    }
    if before.region != after.region {
        changed.push("region");
    }
    if before.bucket != after.bucket {
        changed.push("bucket");
    }
    if before.path_prefix != after.path_prefix {
        changed.push("path_prefix");
    }
    if before.public_base_url != after.public_base_url {
        changed.push("public_base_url");
    }
    if before.signed_url_ttl_seconds != after.signed_url_ttl_seconds {
        changed.push("signed_url_ttl_seconds");
    }
    if before.default_visibility != after.default_visibility {
        changed.push("default_visibility");
    }
    if before.max_upload_mb != after.max_upload_mb {
        changed.push("max_upload_mb");
    }
    if before.allowed_content_types != after.allowed_content_types {
        changed.push("allowed_content_types");
    }
    changed
}

/// Write an audit entry, tolerating a failure to do so.
///
/// A settings save that succeeded must not be reported as failed because the audit table was
/// briefly unavailable — the operator would click Save again, and the second save is the one
/// that lands twice.
async fn record(state: &AppState, entry: NewAuditEntry) -> std::result::Result<(), ApiError> {
    match omnion_audit::record(state.db().pool(), entry).await {
        Ok(_entry) => Ok(()),
        Err(err) => {
            tracing::warn!(error = %err, "the storage settings audit entry could not be written");
            Ok(())
        }
    }
}
