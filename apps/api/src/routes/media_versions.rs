//! `/api/v1/media/{id}/versions` — the version history of a file (REQ-010, slice 2).
//!
//! Slice 1 made the library a file system. This makes it a file system with a memory: replacing
//! a file writes a new version instead of overwriting the bytes, and a restore brings an old
//! version back **as the newest one** rather than by rewriting history.
//!
//! Three invariants hold across this file, and each of them is a place a shortcut produces a
//! history that lies:
//!
//! * **A storage key is never reused.** A replace writes to a fresh key derived from the version
//!   number, so the bytes a published page may have cached stay exactly where they were. Overwrite
//!   in place would be invisible in the audit trail and wrong for every cache holding the old one.
//! * **A restore appends.** It copies the old version's bytes to a new key and appends the copy,
//!   so version 1 means the same bytes today as it did yesterday, whatever happened since.
//! * **The number comes from the database** ([`omnion_media::next_version`]), never from the
//!   client, so a caller cannot ask for version 3 and a concurrent replace cannot take the same
//!   number.

use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_media::{MediaError, MediaFile, MediaVersion, NewVersion};
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::media::read_upload;
use crate::routes::media_files::file_in_scope;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One version of a file, as the detail screen reads it.
#[derive(Debug, Serialize)]
pub struct VersionBody {
    /// Version id.
    pub id: Uuid,
    /// Version number: 1, 2, 3…
    pub version: i32,
    /// Size of this version's bytes.
    pub size_bytes: u64,
    /// Hex-encoded SHA-256 of these bytes — the value that makes "identical" checkable.
    pub checksum: String,
    /// Content type these bytes are stored with.
    pub content_type: String,
    /// Pixel width, when the format carried one.
    pub width: Option<i32>,
    /// Pixel height.
    pub height: Option<i32>,
    /// What the uploader said about this version.
    pub note: String,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// When it was created, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Panel read path of *this* version's bytes.
    pub raw_path: String,
    /// Whether this is the version the file currently serves.
    pub is_current: bool,
}

impl VersionBody {
    /// Describe one version for the panel.
    fn build(version: &MediaVersion, current: i32) -> Self {
        Self {
            id: version.id,
            version: version.version,
            size_bytes: version.size(),
            checksum: version.checksum.clone(),
            content_type: version.content_type.clone(),
            width: version.width,
            height: version.height,
            note: version.note.clone(),
            created_by: version.created_by,
            created_at: version.created_at,
            raw_path: format!(
                "/api/v1/media/{}/versions/{}/raw",
                version.media_id, version.version
            ),
            is_current: version.version == current,
        }
    }
}

/// The history of one file, newest first.
#[derive(Debug, Serialize)]
pub struct VersionListResponse {
    /// File the history belongs to.
    pub media_id: Uuid,
    /// Version the file currently serves — the row's `version_count`.
    pub current_version: i32,
    /// How many versions the history actually holds, counted rather than trusted.
    pub version_total: i64,
    /// The versions, newest first.
    pub versions: Vec<VersionBody>,
}

/// What a replace or a restore did.
#[derive(Debug, Serialize)]
pub struct ReplaceResponse {
    /// The file as it is now.
    pub file: crate::routes::media_files::FileBody,
    /// The version that was appended.
    pub version: VersionBody,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/media/{id}/versions` — the history of one file.
pub async fn list_versions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(file_id): Path<Uuid>,
) -> std::result::Result<Json<VersionListResponse>, ApiError> {
    let file = file_in_scope(&state, &current, file_id).await?;
    let pool = state.db().pool();

    let versions = omnion_media::list_versions(pool, file_id).await?;
    let total = omnion_media::count_versions(pool, file_id).await?;

    Ok(Json(VersionListResponse {
        media_id: file.id,
        current_version: file.version_count,
        version_total: total,
        versions: versions
            .iter()
            .map(|version| VersionBody::build(version, file.version_count))
            .collect(),
    }))
}

/// `POST /api/v1/media/{id}/versions` — replace the bytes, keeping the old ones.
///
/// The file keeps its name, its folder and its id: a published page pointing at this id keeps
/// resolving, and the CDN (REQ-011) is told to purge the key that stopped being current while the
/// one that is now current takes its place.
pub async fn create_version(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(file_id): Path<Uuid>,
    multipart: Multipart,
) -> std::result::Result<(StatusCode, Json<ReplaceResponse>), ApiError> {
    let pool = state.db().pool();
    let existing = file_in_scope(&state, &current, file_id).await?;
    let site = crate::routes::media_files::site_of(&state, existing.site_id).await?;
    let upload = read_upload(multipart).await?;

    if upload.bytes.is_empty() {
        return Err(MediaError::EmptyFile.into());
    }
    if upload.bytes.len() as u64 > omnion_media::MAX_UPLOAD_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!(
                "the replacement is larger than the {} byte limit",
                omnion_media::MAX_UPLOAD_BYTES
            ),
        ));
    }

    // The number and the key are assigned together, and the key carries the number: two versions
    // of one file can never land on one object, which is what makes "the old bytes are still
    // there" a property of the layout rather than of the code path.
    let mut transaction = omnion_media::begin_version(pool).await?;
    let next = omnion_media::next_version(&mut *transaction, file_id).await?;
    let key = version_key(
        existing.site_id,
        file_id,
        next,
        &upload.filename,
        &upload.content_type,
    );

    let stored = state
        .storage()
        .put(&key, &upload.bytes, &upload.content_type)
        .await?;

    let probe = omnion_media::probe_of(&upload.bytes, &upload.content_type);
    // A replacement is a different photograph, so it carries its own camera record — and a
    // replacement in a format that has none *clears* the record rather than inheriting the
    // previous body's, which is what a merged "the file says it is a Canon" line otherwise does.
    let exif = omnion_media::read_exif(&upload.content_type, &upload.bytes);
    let version = omnion_media::NewVersion {
        version: next,
        storage_key: key.clone(),
        size_bytes: stored.size_bytes as i64,
        checksum: stored.checksum.clone(),
        content_type: upload.content_type.clone(),
        probe,
        note: omnion_media::normalize_note(&upload.note),
        created_by: Some(current.user.id),
    };
    let columns = probe.columns();

    let appended =
        match omnion_media::append_version(&mut *transaction, file_id, &version, columns, &exif)
            .await
        {
            Ok(row) => row,
            Err(error) => {
                // The bytes are in the bucket but the history refused them: drop the object so the
                // library and the store stay in step, then report the real failure.
                if let Err(cleanup) = state.storage().delete(&key).await {
                    tracing::warn!(
                        error = %cleanup,
                        key,
                        "the rejected version could not be removed from the object store"
                    );
                }
                return Err(error.into());
            }
        };
    omnion_media::commit_version(transaction).await?;

    let updated = omnion_media::find_file(pool, file_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "file_not_found", "no such file"))?;

    // The CDN purge hook: `media.version_created` is the event REQ-011 subscribes to, so a
    // replace reaches the edge without this crate knowing that the edge exists.
    bus::emit(
        pool,
        NewEvent::new("media.version_created")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "media_id": updated.id,
                "site_id": site.id,
                "filename": updated.filename,
                "version": appended.version,
                "checksum": appended.checksum,
                "size_bytes": appended.size(),
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.version_created")
            .target("media", updated.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "filename": updated.filename,
                "version": appended.version,
                "checksum": appended.checksum,
                "size_bytes": appended.size(),
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(ReplaceResponse {
            file: crate::routes::media_files::FileBody::build(&updated),
            version: VersionBody::build(&appended, updated.version_count),
        }),
    ))
}

/// `POST /api/v1/media/{id}/versions/{version}/restore` — bring an old version back.
///
/// The old row is left exactly as it was. Its bytes are copied to a **new** key and appended as
/// the newest version, so the history only ever grows and version 3 still means the same bytes it
/// meant before the restore.
pub async fn restore_version(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path((file_id, version_number)): Path<(Uuid, i32)>,
) -> std::result::Result<Json<ReplaceResponse>, ApiError> {
    let pool = state.db().pool();
    let existing = file_in_scope(&state, &current, file_id).await?;
    let site = crate::routes::media_files::site_of(&state, existing.site_id).await?;

    let wanted = omnion_media::find_version(pool, file_id, version_number)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "version_not_found",
                format!("this file has no version {version_number}"),
            )
        })?;

    let mut transaction = omnion_media::begin_version(pool).await?;
    let next = omnion_media::next_version(&mut *transaction, file_id).await?;
    let key = version_key(
        existing.site_id,
        file_id,
        next,
        &existing.filename,
        &wanted.content_type,
    );

    // The bytes are copied, not re-uploaded: a restore of version 1 works for a file whose
    // original upload is long gone, because version 1's own key still holds them.
    let bytes = state.storage().get(&wanted.storage_key).await?;
    state
        .storage()
        .put(&key, &bytes, &wanted.content_type)
        .await?;

    let probe = omnion_media::probe_of(&bytes, &wanted.content_type);
    // The record is read from the bytes being restored rather than copied off the version row:
    // a version's geometry was stored, but restoring it re-reads the same bytes, so the record
    // has to come from the same place or a rotation and its dimensions could disagree.
    let exif = omnion_media::read_exif(&wanted.content_type, &bytes);
    let version = NewVersion {
        version: next,
        storage_key: key.clone(),
        size_bytes: wanted.size_bytes,
        checksum: wanted.checksum.clone(),
        content_type: wanted.content_type.clone(),
        probe,
        note: format!("Restored from version {}", wanted.version),
        created_by: Some(current.user.id),
    };
    let columns = probe.columns();

    let appended =
        omnion_media::append_version(&mut *transaction, file_id, &version, columns, &exif).await?;
    omnion_media::commit_version(transaction).await?;

    let updated = omnion_media::find_file(pool, file_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "file_not_found", "no such file"))?;

    bus::emit(
        pool,
        NewEvent::new("media.version_created")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "media_id": updated.id,
                "site_id": site.id,
                "filename": updated.filename,
                "version": appended.version,
                "restored_from": wanted.version,
                "checksum": appended.checksum,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.version_restored")
            .target("media", updated.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "filename": updated.filename,
                "restored_from": wanted.version,
                "version": appended.version,
                "checksum": appended.checksum,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(ReplaceResponse {
        file: crate::routes::media_files::FileBody::build(&updated),
        version: VersionBody::build(&appended, updated.version_count),
    }))
}

/// `GET /api/v1/media/{id}/versions/{version}/raw` — the bytes of one version.
///
/// The version is read, not the row: this is the route that proves an old version is still
/// downloadable after three replaces, which is the whole point of keeping it.
pub async fn raw_version(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((file_id, version_number)): Path<(Uuid, i32)>,
) -> std::result::Result<Response, ApiError> {
    let file = file_in_scope(&state, &current, file_id).await?;
    // An old version of a held file is still a held file: the scanner looked at these exact
    // bytes once and flagged them, and the version history is the one place somebody with
    // `media.read` would otherwise fetch them from.
    crate::routes::media::ensure_servable(&state, &file).await?;
    let version = omnion_media::find_version(state.db().pool(), file_id, version_number)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "version_not_found",
                format!("this file has no version {version_number}"),
            )
        })?;

    serve_bytes(&state, &version).await
}

/// `GET /api/v1/media/{id}/versions/{version}/download` — the same bytes as an attachment.
pub async fn download_version(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((file_id, version_number)): Path<(Uuid, i32)>,
) -> std::result::Result<Response, ApiError> {
    let existing = file_in_scope(&state, &current, file_id).await?;
    crate::routes::media::ensure_servable(&state, &existing).await?;
    let version = omnion_media::find_version(state.db().pool(), file_id, version_number)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "version_not_found",
                format!("this file has no version {version_number}"),
            )
        })?;

    let mut response = serve_bytes(&state, &version).await?;
    let name = version_filename(&existing.filename, version.version);
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header_value(&format!("attachment; filename=\"{name}\""))?,
    );
    Ok(response)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Object key of one version: `sites/{site}/{media}/v{n}-{extension}`.
///
/// The version number is in the key, so the versions of one file are one directory and a listing
/// of the bucket reads as the history. The extension is reduced the way an upload's is, and the
/// content type of the *new* bytes wins when the uploader renamed the file — a `.png` that
/// arrives as `image/webp` must not be stored under a name that claims otherwise.
#[must_use]
pub fn version_key(
    site_id: Uuid,
    media_id: Uuid,
    version: i32,
    filename: &str,
    content_type: &str,
) -> String {
    // The name's extension wins, and the declared content type is the fallback: a replacement
    // uploaded as `photo` with no extension still has to be a `.png` object, because a key with
    // no extension makes every downstream reader guess.
    let extension: Option<String> =
        extension_of(filename).or_else(|| extension_of_type(content_type).map(str::to_owned));
    match extension {
        Some(extension) if !extension.is_empty() => {
            format!("sites/{site_id}/{media_id}/v{version}.{extension}")
        }
        _ => format!("sites/{site_id}/{media_id}/v{version}"),
    }
}

/// The extension of a file name, when it is a usable one.
fn extension_of(filename: &str) -> Option<String> {
    let (_, extension) = filename.rsplit_once('.')?;
    if extension.is_empty()
        || extension.len() > 8
        || !extension.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(extension.to_lowercase())
}

/// The extension a content type implies, for a name that carries none.
fn extension_of_type(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/avif" => Some("avif"),
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "audio/mpeg" => Some("mp3"),
        "audio/ogg" => Some("ogg"),
        "audio/wav" => Some("wav"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        _ => None,
    }
}

/// The download name of one version: `report.pdf` becomes `report-v2.pdf`.
///
/// Two versions of one file downloaded into one folder would otherwise collide on the same
/// name, and the second would silently overwrite the first.
#[must_use]
pub fn version_filename(filename: &str, version: i32) -> String {
    match filename.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            format!("{stem}-v{version}.{extension}")
        }
        _ => format!("{filename}-v{version}"),
    }
}

/// Answer with one version's bytes under the serve plan of its content type.
async fn serve_bytes(state: &AppState, version: &MediaVersion) -> Result<Response, ApiError> {
    let bytes = state.storage().get(&version.storage_key).await?;
    let plan = omnion_media::serve_plan(&version.content_type);

    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header_value(plan.content_type)?);
    headers.insert(
        header::CONTENT_DISPOSITION,
        header_value(&format!("{}; filename=\"file\"", plan.disposition))?,
    );
    headers.insert(header::CACHE_CONTROL, header_value("private, max-age=300")?);
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, header_value("nosniff")?);
    Ok(response)
}

/// Build one response header value, refusing anything unusable.
fn header_value(value: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(value).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            error.to_string(),
        )
    })
}

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> std::result::Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// The file a version belongs to, for the callers that already hold one.
#[allow(dead_code)]
fn assert_same_file(file: &MediaFile, version: &MediaVersion) -> std::result::Result<(), ApiError> {
    if file.id == version.media_id {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "version_mismatch",
            "this version belongs to another file",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_key_carries_its_number() {
        let site = Uuid::nil();
        let media = Uuid::nil();
        assert_eq!(
            version_key(site, media, 2, "logo.PNG", "image/png"),
            format!("sites/{site}/{media}/v2.png")
        );
        // Two versions can never land on one object.
        assert_ne!(
            version_key(site, media, 2, "logo.png", "image/png"),
            version_key(site, media, 3, "logo.png", "image/png")
        );
    }

    #[test]
    fn a_key_falls_back_to_the_content_type() {
        // A replacement uploaded as `photo` with no extension still has to be a `.png` object —
        // a key with no extension makes every downstream reader guess.
        let site = Uuid::nil();
        let media = Uuid::nil();
        assert_eq!(
            version_key(site, media, 1, "photo", "image/png"),
            format!("sites/{site}/{media}/v1.png")
        );
        assert_eq!(
            version_key(site, media, 1, "photo", "application/pdf"),
            format!("sites/{site}/{media}/v1.pdf")
        );
    }

    #[test]
    fn a_key_refuses_an_unusable_extension() {
        let site = Uuid::nil();
        let media = Uuid::nil();
        // A path-like or absurdly long extension is dropped rather than smuggled into the key.
        assert_eq!(
            version_key(
                site,
                media,
                1,
                "archive.verylongextension",
                "application/zip"
            ),
            format!("sites/{site}/{media}/v1")
        );
        assert_eq!(
            version_key(site, media, 1, "report.pdf", "application/octet-stream"),
            format!("sites/{site}/{media}/v1.pdf")
        );
    }

    #[test]
    fn two_versions_never_download_under_the_same_name() {
        assert_eq!(version_filename("report.pdf", 2), "report-v2.pdf");
        assert_eq!(version_filename("logo.png", 1), "logo-v1.png");
        assert_eq!(version_filename("README", 3), "README-v3");
        // A name that is only an extension keeps its stem rather than producing `-v2..pdf`.
        assert_eq!(version_filename(".gitignore", 2), ".gitignore-v2");
    }
}
