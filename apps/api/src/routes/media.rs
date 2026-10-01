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
pub(crate) async fn ensure_servable(
    state: &AppState,
    current: &CurrentSession,
    media: &MediaFile,
) -> Result<(), ApiError> {
    ensure_grant_allows(state, current, media).await?;
    ensure_scan_allows(state, media).await
}

/// Refuse a file a grant chain has taken a capability away from.
///
/// The gate lives beside [`ensure_scan_allows`] rather than inside it, and it is *first*, for
/// two reasons. A grant is about who is asking and a scan is about what the bytes are: a
/// caller who may not read this file learns that from the grant, not from a `file_scan_failed`
/// that names a state they were never allowed to know about. And the grant check is the
/// cheaper one — two statements and a membership read, against a policy row and a quarantine
/// probe on the hottest path in the library.
///
/// The narrowing rule is applied in one place, [`crate::routes::media_grants::require_capability`]:
/// a chain that names this caller not at all leaves the catalogue's answer verbatim, and a
/// chain that names them may only subtract from it. A version that intersected unconditionally
/// would refuse every file in a library that has no grants on it.
pub(crate) async fn ensure_grant_allows(
    state: &AppState,
    current: &CurrentSession,
    media: &MediaFile,
) -> Result<(), ApiError> {
    // Fast path: a file in a library with no grants at all must not pay for a chain walk on
    // every thumbnail. One indexed existence probe, and the common answer is that there is
    // nothing to resolve.
    let chain = omnion_media::load_chain(state.db().pool(), media.id, media.folder_id)
        .await
        .map_err(ApiError::from)?;
    let group_ids = omnion_media::group_ids_of(state.db().pool(), current.user.id)
        .await
        .map_err(ApiError::from)?;
    let decision = omnion_media::resolve(&chain, "user", current.user.id, &group_ids);
    if !decision.touched {
        return Ok(());
    }

    // What the catalogue alone would say. The route's own `guards::require` already refused a
    // caller without `media.read`, so read is present by construction and this only decides
    // the rest — but the *whole* intersection is computed so a grant naming nobody in
    // particular cannot read past a catalogue that refused.
    let catalogue = omnion_media::Capabilities {
        read: true,
        write: true,
        delete: true,
        // `share` is the capability that hands bytes to somebody who never signs in, and the
        // route that creates a link is the only place it matters — the chain is read there
        // again with the caller's own groups, so a share can never be created against a
        // capability the caller does not hold.
        share: true,
    };
    let effective = crate::routes::media_grants::require_capability(&decision, catalogue);
    if effective.read {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "media_grant_denied",
        format!("You do not have access to this file. {}", decision.reason),
    ))
}

/// The scan half of the serve gate, split out of [`ensure_servable`] so the grant rule has a
/// name of its own.
pub(crate) async fn ensure_scan_allows(
    state: &AppState,
    media: &MediaFile,
) -> Result<(), ApiError> {
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
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let media = media_in_scope(&state, &current, media_id).await?;
    let range = range_header(&headers);
    let conditional = conditional_headers(&headers, &media.checksum, Some(media.created_at));
    serve(&state, &media, "private, max-age=300", range, conditional).await
}

/// The caller's `Range` header, if it carries one.
///
/// A header that is present but not valid UTF-8 reads as `None` rather than as an error: the
/// parser would ignore it either way, and refusing the request over an undecodable byte would
/// make a broken proxy into a broken video.
pub(crate) fn range_header(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get(header::RANGE)?.to_str().ok()
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
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let file = crate::routes::media_files::file_in_scope(&state, &current, media_id).await?;
    ensure_servable(&state, &current, &file).await?;
    let range = range_header(&headers);
    let conditional = conditional_headers(&headers, &file.checksum, file.updated_at.or(Some(file.created_at)));
    serve_file(&state, &file, "private, max-age=300", range, conditional).await
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
    // The **scan** half only. A grant narrows a sign-in's access and cannot describe an
    // anonymous visitor, so applying one here would break every published page the moment
    // somebody narrowed a folder — and the scanner's verdict is exactly the state an
    // unauthenticated reader must not reach. The two halves are separate functions for this
    // reason, and conflating them is the mistake this comment is here to stop.
    ensure_scan_allows(&state, &file).await?;
    let range = range_header(&headers);
    let conditional = conditional_headers(&headers, &file.checksum, file.updated_at.or(Some(file.created_at)));
    serve_file(&state, &file, "public, max-age=3600", range, conditional).await
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
///
/// `range` is the caller's `Range` header, already extracted. A `None` header and a header the
/// parser could not read are the same request as far as this function is concerned, and they get
/// the same answer — see [`omnion_media::RangePlan`] for why an unreadable range is ignored
/// rather than refused.
async fn serve(
    state: &AppState,
    media: &Media,
    cache_control: &'static str,
    range: Option<&str>,
    conditional: ConditionalAnswer,
) -> Result<Response, ApiError> {
    // Before the body moves: a revalidation that reads the object store and then decides to send
    // nothing has cost exactly what an unconditional `200` would have.
    if conditional.verdict == omnion_media::validators::Conditional::NotModified {
        return not_modified(&conditional);
    }
    let plan = serve_plan(&media.content_type);
    let (status, body) = read_window(state, &media.storage_key, range, media.size()).await?;
    // The range headers are decided *before* the body moves into the response, because they
    // report the number of bytes that arrived and the body is the only thing that knows it.
    let ranges = range_header_values(range, media.size(), &body)?;

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
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
    apply_range_headers(headers, ranges)?;
    apply_validators(headers, &conditional)?;
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
    range: Option<&str>,
    conditional: ConditionalAnswer,
) -> Result<Response, ApiError> {
    if conditional.verdict == omnion_media::validators::Conditional::NotModified {
        return not_modified(&conditional);
    }
    let plan = serve_plan(&media.content_type);
    let (status, body) = read_window(state, &media.storage_key, range, media.size()).await?;
    // The range headers are decided *before* the body moves into the response, because they
    // report the number of bytes that arrived and the body is the only thing that knows it.
    let ranges = range_header_values(range, media.size(), &body)?;

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
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
    apply_range_headers(headers, ranges)?;
    apply_validators(headers, &conditional)?;
    Ok(response)
}

/// Read the bytes a request asks for: the whole object, or the window its `Range` header names.
///
/// Three decisions live here and each is a shortcut that produces a plausible wrong answer:
///
/// 1. **The window is chosen from the row's length, and a store that disagrees is answered with
///    what actually came back.** The `media` row records the size at upload; the object may be
///    shorter (a truncated write, a key rewritten by hand). Slicing to the row's length and
///    reporting it in `Content-Range` would tell a player it received bytes that never existed.
/// 2. **A store that cannot satisfy the window yields the whole object, not an error.** A
///    `416` from the *store* means the object is shorter than the row says — the client did
///    nothing wrong, so answering `416` to it blames the client for the server's disagreement.
///    The body is then the whole object, which is what a player can still play.
/// 3. **A window is only requested when it is worth requesting.** For a small object the whole
///    read is cheaper than a second round trip, and the branch keeps a one-kilobyte text file on
///    exactly the code path it used before ranges existed.
pub(crate) async fn read_window(
    state: &AppState,
    storage_key: &str,
    range: Option<&str>,
    claimed_total: u64,
) -> Result<(StatusCode, Vec<u8>), ApiError> {
    match omnion_media::RangePlan::decide(range, claimed_total) {
        omnion_media::RangePlan::Whole => {
            let bytes = state.storage().get(storage_key).await?;
            Ok((StatusCode::OK, bytes))
        }
        omnion_media::RangePlan::Unsatisfiable { .. } => {
            // Nothing the client named exists, and no body is sent: the `Content-Range` header
            // on this response is the whole answer, and a player reads the length from it.
            Ok((StatusCode::RANGE_NOT_SATISFIABLE, Vec::new()))
        }
        omnion_media::RangePlan::Partial { window, .. } => {
            match state
                .storage()
                .get_range(storage_key, window.start, window.end)
                .await
            {
                Ok(bytes) if !bytes.is_empty() => Ok((StatusCode::PARTIAL_CONTENT, bytes)),
                // Decision 2 above: the store refused the window the *row* implied, so the whole
                // object is the honest answer and the status follows the bytes that arrived.
                Ok(_) => {
                    let bytes = state.storage().get(storage_key).await?;
                    Ok((StatusCode::OK, bytes))
                }
                Err(omnion_storage::StorageError::RangeNotSatisfiable { .. }) => {
                    let bytes = state.storage().get(storage_key).await?;
                    Ok((StatusCode::OK, bytes))
                }
                Err(error) => Err(error.into()),
            }
        }
    }
}

/// Write the three headers a range answer carries.
///
/// `Accept-Ranges` goes on **every** response, including the whole-object one: a client that has
/// to fail a request to discover that ranges work never tries again, and the media player is
/// exactly the client that most needs the second attempt. `Content-Range` is only written when
/// bytes were actually windowed, and its total is the number of bytes that really arrived.
pub(crate) fn range_header_values(
    range: Option<&str>,
    claimed_total: u64,
    served: &[u8],
) -> Result<Option<String>, ApiError> {
    let plan = omnion_media::RangePlan::decide(range, claimed_total);
    // A `200` carries no `Content-Range` even when the client asked for a window and the answer
    // turned out to be the whole object — a `Content-Range` on a `200` is a lie about a
    // response that is not a range.
    match plan.content_range(served.len() as u64) {
        Some(value) => Ok(Some(value)),
        None => Ok(None),
    }
}

/// Write the headers a range answer carries.
///
/// `Accept-Ranges` goes on **every** response, including the whole-object one: a client that has
/// to fail a request to discover that ranges work never tries again, and the media player is
/// exactly the client that most needs the second attempt. `Content-Range` is only written when
/// bytes were actually windowed, and its total is the number of bytes that really arrived.
pub(crate) fn apply_range_headers(
    headers: &mut axum::http::HeaderMap,
    content_range: Option<String>,
) -> Result<(), ApiError> {
    headers.insert(
        header::ACCEPT_RANGES,
        header_value(omnion_media::RangePlan::ACCEPT_RANGES)?,
    );
    if let Some(value) = content_range {
        headers.insert(header::CONTENT_RANGE, header_value(&value)?);
    }
    Ok(())
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

/// Answer a conditional `GET` with `304` when the caller already holds these bytes.
///
/// The three media serve paths — the panel's `raw`, the public renderer and a version's `raw` —
/// all answer from an address that **names the file rather than its contents**, and a replace
/// changes the bytes behind that address without changing it. So the response has to carry a
/// validator, and the server has to honour one, or the panel's own `max-age` window is a promise
/// the bytes behind it can break.
///
/// Three decisions, each a shortcut that produces a plausible wrong answer:
///
/// 1. **The check happens before the body is read.** Reading first and comparing after is the same
///    work as not checking at all, which defeats the entire point: a revalidation that still
///    pulls every byte off the object store saves the *client* nothing an unconditional `200`
///    would not have saved it.
/// 2. **A `304` carries no `Content-Length` and no body** — only the validators. Sending `0`
///    as the length is the detail that makes several clients treat the response as an empty file,
///    and a `Content-Range` on it is a lie about a response that is not a range.
/// 3. **The ETag is weak and the validator is the checksum.** `Media::checksum` is the SHA-256 of
///    the bytes, and `append_version` rewrites it in the same statement that moves
///    `storage_key`, so it cannot outlive its object. A validator built from `updated_at` would
///    instead be changed by a rename and unchanged by a replace that happened without a stamp.
///
/// `last_modified` is optional because the version row records `created_at` rather than a change
/// instant; a row with no recorded instant offers no date validator, which is honest rather than
/// a missing header a client has to guess about.
pub(crate) fn conditional_headers(
    headers: &axum::http::HeaderMap,
    checksum: &str,
    last_modified: Option<OffsetDateTime>,
) -> ConditionalAnswer {
    let etag = omnion_media::validators::weak_etag_of(checksum);
    let rendered = last_modified.map(omnion_media::validators::imf_fixdate);
    let get = |name: header::HeaderName| {
        headers
            .get(&name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let verdict = omnion_media::validators::decide(
        get(header::IF_NONE_MATCH).as_deref(),
        get(header::IF_MODIFIED_SINCE).as_deref(),
        &etag,
        rendered.as_deref(),
    );
    ConditionalAnswer { verdict, etag, rendered }
}

/// What a conditional answer decided, carried with the values a `304` still has to report.
pub(crate) struct ConditionalAnswer {
    /// Whether the body is sent or the caller is told it already has it.
    pub verdict: omnion_media::validators::Conditional,
    /// The current representation's validator.
    pub etag: String,
    /// The current representation's instant, when one was recorded.
    pub rendered: Option<String>,
}

/// Write the validators onto a response, whether or not it carries a body.
pub(crate) fn apply_validators(
    headers: &mut axum::http::HeaderMap,
    answer: &ConditionalAnswer,
) -> Result<(), ApiError> {
    headers.insert(header::ETAG, header_value(&answer.etag)?);
    if let Some(last_modified) = &answer.rendered {
        headers.insert(header::LAST_MODIFIED, header_value(last_modified)?);
    }
    Ok(())
}

/// Build the `304` a revalidated request is answered with.
pub(crate) fn not_modified(answer: &ConditionalAnswer) -> Result<Response, ApiError> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NOT_MODIFIED;
    apply_validators(response.headers_mut(), answer)?;
    Ok(response)
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
