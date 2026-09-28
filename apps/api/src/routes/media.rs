//! `/api/v1/media` — the media library surface.
//!
//! v0 of the media library (docs/01-VISION.md §5, docs/requests/REQ-010): a file is uploaded into
//! one site, stored in the object store (`omnion-storage`: MinIO in development, any
//! S3-compatible endpoint in production) and described by one row in the `media` table. The panel
//! lists the library of the selected site, reads a file back through its own session, and removes
//! it — bytes and row together.
//!
//! Two serve paths exist on purpose:
//!
//! * `/api/v1/media/{id}/raw` — the panel's read path, behind the `media.read` permission;
//! * `/api/v1/public/media/{id}` — the renderer's read path, unauthenticated like the rest of
//!   the public surface, so a published page can point at its own assets.
//!
//! Both answer with a content type a browser may render inline **only** for the types
//! [`omnion_media::serve_plan`] allows (images, media, PDF, plain text); everything else leaves
//! as an `application/octet-stream` download with `nosniff` set, so an uploaded document can
//! never become markup or script on the platform's own origin.
//!
//! Media rows belong to sites, so the tenancy scope rule applies on top of the permission guard
//! (`crate::scope`): an account with a primary organization touches only its own tenants.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::Site;
use omnion_identity::sites;
use omnion_media::{
    MAX_UPLOAD_BYTES, Media, MediaError, MediaFile, NewMedia, normalize_content_type, object_key,
    sanitize_filename, serve_plan,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::cdn_cache;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

/// Slack the body limit leaves above the file limit for multipart framing.
pub const UPLOAD_BODY_SLACK: usize = 1024 * 1024;

/// Response body of one media row.
#[derive(Debug, Serialize)]
pub struct MediaBody {
    /// Media id.
    pub id: Uuid,
    /// Site the file belongs to.
    pub site_id: Uuid,
    /// File name.
    pub filename: String,
    /// Content type the file is stored with.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Hex-encoded SHA-256 of the bytes.
    pub checksum: String,
    /// Panel read path of the bytes.
    pub raw_path: String,
    /// Public read path of the bytes.
    pub public_path: String,
    /// When the file arrived, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl MediaBody {
    /// Describe one row for the panel.
    fn build(media: &Media) -> Self {
        Self {
            id: media.id,
            site_id: media.site_id,
            filename: media.filename.clone(),
            content_type: media.content_type.clone(),
            size_bytes: media.size(),
            checksum: media.checksum.clone(),
            raw_path: format!("/api/v1/media/{}/raw", media.id),
            public_path: format!("/api/v1/public/media/{}", media.id),
            created_at: media.created_at,
        }
    }
}

/// Response body of the media list.
#[derive(Debug, Serialize)]
pub struct MediaListResponse {
    /// Site the listed files belong to.
    pub site_id: Uuid,
    /// Files of the site, newest first.
    pub media: Vec<MediaBody>,
}

/// `GET /api/v1/media` — the library of one site.
#[derive(Debug, Deserialize)]
pub struct MediaQuery {
    /// Site whose library is listed.
    pub site_id: Uuid,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The media library of a site.
pub async fn list_media(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<MediaQuery>,
) -> Result<Json<MediaListResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let rows = omnion_media::list_media(state.db().pool(), site.id).await?;

    Ok(Json(MediaListResponse {
        site_id: site.id,
        media: rows.iter().map(MediaBody::build).collect(),
    }))
}

/// Upload one file into a site's library.
pub async fn upload_media(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<MediaQuery>,
    multipart: Multipart,
) -> Result<(StatusCode, Json<MediaBody>), ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let upload = read_upload(multipart).await?;

    let filename = sanitize_filename(&upload.filename)?;
    let content_type = normalize_content_type(&upload.content_type)?;
    if upload.bytes.is_empty() {
        return Err(MediaError::EmptyFile.into());
    }
    if upload.bytes.len() as u64 > MAX_UPLOAD_BYTES {
        return Err(MediaError::SizeTooLarge {
            limit: MAX_UPLOAD_BYTES,
        }
        .into());
    }

    // The object key is derived from the ids, so the bytes land in a place no upload can steer;
    // the row is written only after the object is stored.
    let media_id = Uuid::new_v4();
    let storage_key = object_key(site.id, media_id, &filename);
    let stored = state
        .storage()
        .put(&storage_key, &upload.bytes, &content_type)
        .await?;

    // What the bytes say about themselves, read from their header. A format we cannot read is
    // simply not described — the row keeps its columns empty rather than a guess that would
    // break every layout that reads them.
    let probe = omnion_media::probe_of(&upload.bytes, &content_type);
    // What the camera wrote about its own picture, read from the same prefix as the geometry.
    // Read here rather than after the insert so the row is written once, described once.
    let exif = omnion_media::read_exif(&content_type, &upload.bytes);

    // `insert_media` consumes the name, the type and the checksum, and the history below needs
    // the same three values. They are copied here, once, rather than read back out of the row —
    // a value that has been moved cannot be re-read, and a round trip through the database to
    // get it back would make the two paths disagree if anything changed in between.
    let stored_size = stored.size_bytes as i64;
    let stored_checksum = stored.checksum.clone();
    let content_type_for_version = content_type.clone();

    let written = omnion_media::insert_media(
        state.db().pool(),
        NewMedia {
            site_id: site.id,
            storage_key: storage_key.clone(),
            filename,
            content_type,
            size_bytes: stored_size,
            checksum: stored.checksum,
            created_by: Some(current.user.id),
        },
    )
    .await;

    let media = match written {
        Ok(media) => {
            // The history starts at the file: version 1 is this upload, not a later backfill, so
            // the detail screen never opens on a file with an empty version list. The probe's
            // numbers go onto the row here rather than in a second statement, which is what makes
            // "the file says it is 1920 wide" and "the history says it is 1920 wide" the same
            // fact read twice.
            // One statement writes the record and the geometry it rotates, so a row can never
            // hold the orientation from one call and the dimensions from another.
            omnion_media::versions::fill_exif(
                state.db().pool(),
                media.id,
                &exif,
                (probe.width, probe.height),
            )
            .await?;
            omnion_media::versions::ensure_version_one(
                state.db().pool(),
                media.id,
                &omnion_media::NewVersion {
                    version: 1,
                    storage_key: storage_key.clone(),
                    size_bytes: stored_size,
                    checksum: stored_checksum,
                    content_type: content_type_for_version,
                    probe,
                    note: upload.note.clone(),
                    created_by: Some(current.user.id),
                },
            )
            .await?;
            media
        }
        Err(error) => {
            // The bytes are in the bucket but the row was refused: drop the object again so the
            // library and the store stay in step, then report the real failure.
            if let Err(cleanup) = state.storage().delete(&storage_key).await {
                tracing::warn!(
                    error = %cleanup,
                    key = storage_key,
                    "the rejected upload could not be removed from the object store"
                );
            }
            return Err(error.into());
        }
    };

    // The bus carries the fact that the library changed; the search index (REQ-002) is one of its
    // subscribers, so an upload is findable a moment later without a manual reindex.
    bus::emit(
        state.db().pool(),
        NewEvent::new("media.created")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "media_id": media.id,
                "site_id": site.id,
                "filename": media.filename,
                "content_type": media.content_type,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.uploaded")
            .target("media", media.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "filename": media.filename,
                "content_type": media.content_type,
                "size_bytes": media.size(),
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(MediaBody::build(&media))))
}

// ---------------------------------------------------------------------------------------------
// Scan gating
// ---------------------------------------------------------------------------------------------

/// Refuse to serve a file whose scan state says it may not be served, and explain why.
///
/// This is the strict half of "scanning is best-effort at ingest, strict at serve" (REQ-010
/// *Risks*): a scanner outage must not lose an upload, and an *unscanned* file must not be
/// readable until it clears. Both halves live here rather than in the upload path, because a
/// file can be flagged minutes or years after it was stored — a link made yesterday must stop
/// working the moment the scanner flags the file, and only a check on the serve path does
/// that.
///
/// The policy is read per request rather than cached, because a site flipping `on_error` from
/// `hold` to `serve` has to take effect on the next request: a cached policy would keep
/// refusing a file for as long as the entry lived, and the operator has just said the
/// opposite on the settings screen.
pub(crate) async fn ensure_servable(state: &AppState, media: &MediaFile) -> Result<(), ApiError> {
    // A `clean` file needs no policy read at all: the state already answers the question, and
    // the panel serves far more of those than anything else, so a row read per request on the
    // hottest path in the library is a cost with no benefit.
    if media.scan_status == "clean" {
        return Ok(());
    }
    let quarantined = omnion_media::is_quarantined(state.db().pool(), media.id).await?;
    let policy = omnion_media::read_scan_settings(state.db().pool(), media.site_id).await?;
    // Note that a *disabled* site is not an early return any more. `may_serve` reads
    // `enabled` itself, because one state has to be refused whether or not anybody asked for
    // scanning: a file the scanner flagged stays flagged after the switch is turned off.
    match omnion_media::may_serve(&media.scan_status, quarantined, &policy) {
        Ok(()) => Ok(()),
        Err(refusal) => {
            // A `404` for a trashed file and a `403` for everything else: the trashed case is
            // already what the file's own route says, and the other three are a *state* the
            // caller can act on by asking somebody with `media.scan.manage`.
            let status = match refusal {
                omnion_media::ServeRefusal::Trashed => StatusCode::NOT_FOUND,
                _ => StatusCode::FORBIDDEN,
            };
            Err(ApiError::new(status, refusal.code(), refusal.message()))
        }
    }
}

/// One media row of the library.
pub async fn get_media(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(media_id): Path<Uuid>,
) -> Result<Json<MediaBody>, ApiError> {
    let media = media_in_scope(&state, &current, media_id).await?;
    Ok(Json(MediaBody::build(&media)))
}

/// The bytes of one file, for the panel.
pub async fn raw_media(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(media_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let media = media_in_scope(&state, &current, media_id).await?;
    serve(&state, &media, "private, max-age=300").await
}

/// The bytes of one file for the panel, through the full file-manager row.
///
/// The plain `Media` row the original raw path reads carries no scan state at all, which is
/// why this handler exists rather than the original being edited in place: the gate needs
/// `deleted_at` and `scan_status`, and a `403` for a quarantined file must not be reachable by
/// a caller who happens to know the old row's shape.
pub async fn raw_file(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(media_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let file = crate::routes::media_files::file_in_scope(&state, &current, media_id).await?;
    ensure_servable(&state, &file).await?;
    serve_file(&state, &file, "private, max-age=300").await
}

/// Remove one file: the object and its row.
pub async fn delete_media(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(media_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let media = media_in_scope(&state, &current, media_id).await?;
    let site = site_of(&state, media.site_id).await?;

    // The object goes first: when the store refuses, the row stays and the operator can retry
    // instead of leaving bytes behind that nothing points at.
    state.storage().delete(&media.storage_key).await?;

    if !omnion_media::delete_media(state.db().pool(), media.id).await? {
        return Err(media_not_found());
    }

    // The row is gone; the index has to hear about it or the library would keep answering with a
    // file nobody can open.
    bus::emit(
        state.db().pool(),
        NewEvent::new("media.deleted")
            .organization(site.organization_id)
            .site(media.site_id)
            .actor(current.user.id)
            .payload(json!({ "media_id": media.id, "site_id": media.site_id })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.deleted")
            .target("media", media.id.to_string())
            .metadata(json!({
                "site_id": media.site_id,
                "filename": media.filename,
                "size_bytes": media.size(),
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// The bytes of one file, for the public site renderer.
///
/// Unauthenticated, like the rest of the public surface: a media id is an opaque UUID and this
/// route serves exactly the object the row names. Per-media visibility (private folders,
/// unpublished assets) arrives with the file manager (REQ-010).
///
/// The cache headers are not written here but decided by the site's cache-rule set (REQ-011):
/// a rule an operator created has to change what a visitor's browser does, and a hardcoded
/// `max-age` in this handler is what made the rule table a screen that changed nothing. The
/// file's checksum is the validator, so a conditional request for a file whose bytes did not
/// change answers `304` without reading the object store twice.
pub async fn public_media(
    State(state): State<AppState>,
    Path(media_id): Path<Uuid>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    // The public renderer reads the full row, not the base one, because a file that is
    // quarantined or unscanned must be refused here too: the public path is the one an
    // unauthenticated visitor reaches, and it is strictly the *worst* place to serve a file
    // the scanner has not cleared.
    let file = omnion_media::find_file_any_state(state.db().pool(), media_id)
        .await?
        .ok_or_else(media_not_found)?;
    ensure_servable(&state, &file).await?;

    // The rule set is matched against the API path here, not a public address: a media
    // file is addressed by its id and has no address of its own, so `/api/v1/public/media/*`
    // is the only path a rule author can write that matches this response.
    let request_path = format!("/api/v1/public/media/{media_id}");
    let shape = cdn_cache::request_shape(&request_path, None, &headers);
    let policy = cdn_cache::policy_for(state.db().pool(), file.site_id, &shape).await;

    let response = serve_file(&state, &file, "no-store").await?;
    Ok(cdn_cache::apply(
        response,
        &policy,
        cdn_cache::Validator::File {
            checksum: &file.checksum,
            id: file.id,
        },
        &headers,
    ))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// One part of an upload as it was received.
pub struct Upload {
    /// File name the client sent (reduced before it is used).
    pub filename: String,
    /// Content type the client declared (normalised before it is used).
    pub content_type: String,
    /// The bytes themselves.
    pub bytes: Bytes,
    /// What the uploader said about this version, reduced to one bounded line.
    pub note: String,
}

/// Read the `file` part of a multipart upload; other parts are consumed and ignored.
///
/// `note` is read from a second part when the client sends one, which is how a replace says
/// what changed about itself. Any other part is drained so the parser can reach the next field.
pub async fn read_upload(mut multipart: Multipart) -> Result<Upload, ApiError> {
    let mut upload: Option<Upload> = None;
    let mut note = String::new();

    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        if field.name() == Some("note") {
            // The version note rides as its own part rather than a query parameter, so a note
            // with a newline or a quote in it cannot corrupt the URL.
            let raw = field.text().await.map_err(multipart_error)?;
            note = omnion_media::normalize_note(&raw);
        } else if field.name() == Some("file") {
            let filename = field.file_name().unwrap_or("upload").to_owned();
            let content_type = field
                .content_type()
                .unwrap_or("application/octet-stream")
                .to_owned();
            let bytes = field.bytes().await.map_err(multipart_error)?;
            upload = Some(Upload {
                filename,
                content_type,
                bytes,
                note: String::new(),
            });
        } else {
            // Drain the part so the parser can reach the next one.
            field.bytes().await.map_err(multipart_error)?;
        }
    }

    let mut upload = upload.ok_or_else(|| {
        ApiError::bad_request(
            "missing_file",
            "the request carries no `file` part — send the upload as multipart/form-data",
        )
    })?;
    upload.note = note;
    Ok(upload)
}

/// Map a multipart failure onto the API surface; a body over the limit is a `413`.
fn multipart_error(error: axum::extract::multipart::MultipartError) -> ApiError {
    match error.status() {
        StatusCode::PAYLOAD_TOO_LARGE => ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("the upload is larger than the {MAX_UPLOAD_BYTES} byte limit"),
        ),
        status => ApiError::new(status, "invalid_multipart", error.body_text()),
    }
}

/// Answer with the bytes of one file, under the serve plan of its content type.
async fn serve(
    state: &AppState,
    media: &Media,
    cache_control: &'static str,
) -> Result<Response, ApiError> {
    let bytes = state.storage().get(&media.storage_key).await?;
    let plan = serve_plan(&media.content_type);

    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header_value(plan.content_type)?);
    headers.insert(
        header::CONTENT_DISPOSITION,
        header_value(&format!(
            "{}; filename=\"{}\"",
            plan.disposition, media.filename
        ))?,
    );
    headers.insert(header::CACHE_CONTROL, header_value(cache_control)?);
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, header_value("nosniff")?);
    Ok(response)
}

/// Answer with the bytes of one file-manager row, under the serve plan of its content type.
///
/// A separate function from [`serve`] rather than a conversion between the two row types: a
/// conversion would have to pick which row's columns are authoritative, and the answer would
/// change the day a column is added to one of them.
async fn serve_file(
    state: &AppState,
    media: &omnion_media::MediaFile,
    cache_control: &'static str,
) -> Result<Response, ApiError> {
    let bytes = state.storage().get(&media.storage_key).await?;
    let plan = serve_plan(&media.content_type);

    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header_value(plan.content_type)?);
    headers.insert(
        header::CONTENT_DISPOSITION,
        header_value(&format!(
            "{}; filename=\"{}\"",
            plan.disposition, media.filename
        ))?,
    );
    headers.insert(header::CACHE_CONTROL, header_value(cache_control)?);
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, header_value("nosniff")?);
    Ok(response)
}

/// Build one response header value, refusing anything unusable.
fn header_value(value: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(value).map_err(|err| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            err.to_string(),
        )
    })
}

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Load a site or answer `404 site_not_found`.
pub(crate) async fn site_of(state: &AppState, site_id: Uuid) -> Result<Site, ApiError> {
    sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))
}

/// Load a site and refuse it when it lives outside the caller's organization.
pub(crate) async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<Site, ApiError> {
    let site = site_of(state, site_id).await?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}

/// Load a media row and refuse the request when its site is out of the caller's scope.
async fn media_in_scope(
    state: &AppState,
    current: &CurrentSession,
    media_id: Uuid,
) -> Result<Media, ApiError> {
    let media = omnion_media::find_media(state.db().pool(), media_id)
        .await?
        .ok_or_else(media_not_found)?;
    site_in_scope(state, current, media.site_id).await?;
    Ok(media)
}

fn media_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "media_not_found",
        "no such media in this library",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(content_type: &str, filename: &str) -> Media {
        Media {
            id: Uuid::nil(),
            site_id: Uuid::nil(),
            storage_key: "sites/a/one.png".to_owned(),
            filename: filename.to_owned(),
            content_type: content_type.to_owned(),
            size_bytes: 12,
            checksum: "a".repeat(64),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_body_describes_both_read_paths() {
        let media = row("image/png", "logo.png");
        let body = MediaBody::build(&media);
        assert_eq!(body.size_bytes, 12);
        assert_eq!(
            body.raw_path,
            "/api/v1/media/00000000-0000-0000-0000-000000000000/raw"
        );
        assert_eq!(
            body.public_path,
            "/api/v1/public/media/00000000-0000-0000-0000-000000000000"
        );

        let rendered = serde_json::to_value(&body).expect("the body serialises");
        assert_eq!(rendered["filename"], "logo.png");
        assert_eq!(rendered["content_type"], "image/png");
        assert!(rendered["created_at"].as_str().is_some());
    }

    #[test]
    fn a_negative_row_size_reports_zero() {
        let mut media = row("image/png", "logo.png");
        media.size_bytes = -1;
        assert_eq!(MediaBody::build(&media).size_bytes, 0);
    }

    #[test]
    fn the_body_limit_leaves_room_for_the_multipart_frame() {
        assert!(
            UPLOAD_BODY_SLACK as u64 >= 64 * 1024,
            "the frame needs real slack"
        );
        assert!(UPLOAD_BODY_SLACK as u64 <= MAX_UPLOAD_BYTES);
    }

    #[test]
    fn the_query_names_the_site() {
        let query: MediaQuery =
            serde_json::from_str("{\"site_id\":\"11111111-1111-1111-1111-111111111111\"}")
                .expect("a site id is required");
        assert_eq!(query.site_id.to_string().len(), 36);

        assert!(serde_json::from_str::<MediaQuery>("{}").is_err());
    }

    #[test]
    fn serve_headers_are_shaped_like_headers() {
        assert!(header_value("inline; filename=\"logo.png\"").is_ok());
        assert!(header_value("broken\nvalue").is_err());
    }
}
