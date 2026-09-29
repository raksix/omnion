//! `/api/v1/ai/agents/{id}/files` — the per-agent workspace (REQ-099, slice 2).
//!
//! The workspace is the only place in the agent runtime where a **user's** bytes and a **run's**
//! authority meet, and every decision in this module follows from that. Three of them are worth
//! stating up front, because each is a way a workspace can leak or lie:
//!
//! 1. **A file is never served by a storage link.** The download route reads the bytes through
//!    the application's own storage handle and re-checks the path *again* on the way out. A
//!    presigned URL would be cheaper, but it would also mean the path rule has exactly one
//!    enforcement point — and the one point that matters is the one nobody re-runs.
//!
//! 2. **The agent is resolved before the path is parsed.** A caller who guesses another
//!    organization's agent id gets `404 agent.not_found` and never reaches the file layer, so
//!    the file routes cannot be used to probe which agent ids exist. The `agent_id` is in every
//!    query's `where` clause for the same reason: a file row in another tenant is
//!    indistinguishable from one that was never there.
//!
//! 3. **An upload that breaks a cap leaves nothing behind.** The bytes are written to storage
//!    *after* the caps are checked and the key derived, and if the index write fails the object
//!    is dropped again. The reverse order — index first, bytes second — leaves a row that points
//!    at a key nobody holds, which reads to the panel as a corrupt file rather than a failed
//!    upload.
//!
//! ## Why the usage figure is returned with the list
//!
//! `GET .../files` answers the rows *and* the quota. The Workspace tab renders both, and a
//! separate endpoint would let the two disagree: a list read before an upload and a usage read
//! after it produce a usage bar under a table that does not contain the file it is about.

use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use omnion_ai_hub::run_store;
use omnion_ai_hub::workspace::{self, AgentFile, MAX_FILE_BYTES, NewAgentFile, Usage};
use omnion_audit::NewAuditEntry;
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::ai_agents::OrgQuery;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// One file, as the Workspace tab renders it.
#[derive(Debug, Clone, Serialize)]
pub struct FileView {
    /// Row identity — what Delete and Download address.
    pub id: Uuid,
    /// The path inside the workspace.
    pub path: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// The declared content type.
    pub content_type: String,
    /// Hex SHA-256 of the bytes, so a reader can tell two versions apart.
    pub checksum: String,
    /// The run that wrote it, if it was written by one.
    pub run_id: Option<Uuid>,
    /// When it was added.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When a run last named it.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
}

impl FileView {
    /// Build the view from a row.
    fn build(file: &AgentFile) -> Self {
        Self {
            id: file.id,
            path: file.path.clone(),
            size_bytes: file.size_bytes,
            content_type: file.content_type.clone(),
            checksum: file.checksum.clone(),
            run_id: file.run_id,
            created_at: file.created_at,
            last_used_at: file.last_used_at,
        }
    }
}

/// The quota, as the usage bar needs it.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct UsageView {
    /// Bytes stored across the agent's files.
    pub used_bytes: u64,
    /// The per-agent ceiling.
    pub limit_bytes: u64,
    /// How many files count against it.
    pub file_count: u64,
    /// Whole percent, already clamped — the bar's width is a string the client sets.
    pub percent: u8,
    /// The per-file ceiling, so the upload hint can name it without hardcoding a number.
    pub max_file_bytes: u64,
}

impl From<Usage> for UsageView {
    fn from(usage: Usage) -> Self {
        Self {
            used_bytes: usage.used_bytes,
            limit_bytes: usage.limit_bytes,
            file_count: usage.file_count,
            percent: usage.percent(),
            max_file_bytes: MAX_FILE_BYTES,
        }
    }
}

/// `GET /api/v1/ai/agents/{id}/files` — the workspace listing plus its quota.
#[derive(Debug, Serialize)]
pub struct WorkspaceList {
    /// The agent whose workspace this is.
    pub agent_id: Uuid,
    /// Its files, newest first.
    pub files: Vec<FileView>,
    /// The quota, so the bar cannot disagree with the table above it.
    pub usage: UsageView,
}

/// The parts of an upload this route understands.
struct Upload {
    path: String,
    content_type: String,
    bytes: Vec<u8>,
}

/// `GET /api/v1/ai/agents/{id}/files` — list the workspace.
pub async fn list_agent_files(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Json<WorkspaceList>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let agent = agent_in_scope(&state, organization, id).await?;

    let files = workspace::list_files(state.db().pool(), agent.id).await?;
    let usage = workspace::usage(state.db().pool(), agent.id).await?;
    Ok(Json(WorkspaceList {
        agent_id: agent.id,
        files: files.iter().map(FileView::build).collect(),
        usage: UsageView::from(usage),
    }))
}

/// `POST /api/v1/ai/agents/{id}/files` — add or replace one file.
///
/// The path rides as its own multipart part rather than being derived from the uploaded
/// filename, for the same reason media's upload takes a `note` part: a filename is a browser's
/// opinion about the user's disk, and a path built from one carries whatever the operating
/// system put there. The path is validated here exactly as it is on read.
///
/// A repeated path is a **replacement**, not a duplicate: the unique index on
/// `(agent_id, path)` says a path names one file, and the previous object is left for a
/// lifecycle sweep rather than deleted inside the request that is trying to succeed.
pub async fn upload_agent_file(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    multipart: Multipart,
) -> Result<(StatusCode, Json<FileView>), ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let agent = agent_in_scope(&state, organization, id).await?;

    let upload = read_upload(multipart).await?;
    // Validate before the caps: a path that will be refused regardless of size should not
    // consume a quota check, and the two refusals have different fixes.
    let path = workspace::validate_path(&upload.path)?;

    let replacing = workspace::path_size(state.db().pool(), agent.id, &path).await?;
    let usage = workspace::usage(state.db().pool(), agent.id).await?;
    workspace::check_caps(&usage, upload.bytes.len() as u64, replacing)?;

    // The digest is computed by the crate, next to the key derivation that consumes it. A
    // route that computed its own would be a second implementation of the pair, and the one
    // that matters is the one nobody would think to update.
    let checksum = workspace::checksum_of(&upload.bytes);
    let storage_key = workspace::storage_key(agent.id, &checksum);

    // Bytes first, row second — with the object dropped again if the row cannot be written. The
    // alternative leaves a row whose key nobody holds, which the panel renders as a corrupt file
    // rather than as the failed upload it was.
    state
        .storage()
        .put(&storage_key, &upload.bytes, &upload.content_type)
        .await
        .map_err(storage_error)?;

    let new = NewAgentFile {
        agent_id: agent.id,
        run_id: None,
        path: path.clone(),
        size_bytes: upload.bytes.len() as i64,
        content_type: upload.content_type.clone(),
        storage_key,
        checksum: checksum.clone(),
        created_by: Some(current.user.id),
    };
    let file = match workspace::put_file(state.db().pool(), &new).await {
        Ok(file) => file,
        Err(err) => {
            // The object is already written; dropping the index row is impossible because it was
            // never written, so all that is left is to not leave a stray object behind.
            let _ = state.storage().delete(&new.storage_key).await;
            return Err(err.into());
        }
    };

    let entry = NewAuditEntry::by_user(current.user.id, "ai.agent.file_added")
        .organization(organization)
        .target("ai_agent", agent.id)
        .metadata(json!({ "path": path, "size_bytes": file.size_bytes }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok((StatusCode::CREATED, Json(FileView::build(&file))))
}

/// `GET /api/v1/ai/agents/{id}/files/{path}` — download one file's bytes.
///
/// The path is a *wildcard* segment, so it arrives percent-encoded and needs decoding exactly
/// once. Decoding twice turns a file literally named `%2e%2e%2fpasswd` into a traversal after
/// the validator has already blessed it — which is why the decode is here, immediately before
/// the single validation call, and nowhere else in the module.
pub async fn download_agent_file(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path((id, path)): Path<(Uuid, String)>,
) -> Result<Response, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let agent = agent_in_scope(&state, organization, id).await?;
    let path = workspace::validate_path(&decode_segment(&path))?;

    let file = workspace::get_file(state.db().pool(), agent.id, &path)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "workspace.file_not_found",
                "no such file in this workspace",
            )
        })?;

    let bytes = state.storage().get(&file.storage_key).await.map_err(|err| {
        // A missing object is a 404 rather than a 502: the row exists and the bytes do not,
        // which for the caller is indistinguishable from the file never having been uploaded.
        if matches!(err, omnion_storage::StorageError::NotFound { .. }) {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "workspace.file_gone",
                "the file is indexed but its content is missing from storage",
            )
        } else {
            storage_error(err)
        }
    })?;

    // `attachment` rather than `inline`: a workspace holds whatever an operator uploaded, and
    // serving it inline in the panel's origin is how a stored HTML file becomes stored XSS.
    let filename = file
        .path
        .rsplit('/')
        .next()
        .unwrap_or("file")
        .replace(['"', '\r', '\n'], "");
    let disposition = format!("attachment; filename=\"{filename}\"");
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, file.content_type.clone()),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_owned(),
            ),
        ],
        Body::from(bytes),
    )
        .into_response())
}

/// `DELETE /api/v1/ai/agents/{id}/files/{path}` — remove one file.
///
/// The index row goes first and the object second, and the reason is the mirror of the upload's:
/// an object with no row is an orphan a lifecycle sweep reaps, while a row whose object is gone
/// is a download that 404s forever. The row is the thing the panel can see, so it is the thing
/// that goes away first.
pub async fn delete_agent_file(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    address: ClientAddress,
    Path((id, path)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let agent = agent_in_scope(&state, organization, id).await?;
    let path = workspace::validate_path(&decode_segment(&path))?;

    let file = workspace::get_file(state.db().pool(), agent.id, &path)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "workspace.file_not_found",
                "no such file in this workspace",
            )
        })?;

    let removed = workspace::delete_file(state.db().pool(), agent.id, file.id).await?;
    if !removed {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "workspace.file_not_found",
            "no such file in this workspace",
        ));
    }
    // Best-effort on purpose: the row is already gone, and failing the whole request because a
    // bucket did not answer would tell the panel the delete failed when it succeeded.
    if let Err(err) = state.storage().delete(&file.storage_key).await {
        if !matches!(err, omnion_storage::StorageError::NotFound { .. }) {
            return Err(storage_error(err));
        }
    }

    let entry = NewAuditEntry::by_user(current.user.id, "ai.agent.file_removed")
        .organization(organization)
        .target("ai_agent", agent.id)
        .metadata(json!({ "path": file.path, "size_bytes": file.size_bytes }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Resolve the agent, or answer 404.
///
/// Called before the path is touched in every file route, so an unknown or foreign agent id
/// never reaches the file layer. The organization is the *resolved* one rather than the
/// session's, because a platform-level account acting inside a tenant must still be refused a
/// file in the tenant it is not acting for.
async fn agent_in_scope(
    state: &AppState,
    organization: Uuid,
    id: Uuid,
) -> Result<run_store::Agent, ApiError> {
    run_store::get_agent(state.db().pool(), organization, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "agent.not_found", "no such agent"))
}

/// Percent-decode one path segment exactly once.
fn decode_segment(raw: &str) -> String {
    percent_decode(raw)
}

/// A minimal percent-decoder, written here rather than pulled in for one call site.
///
/// `axum` hands the wildcard segment over still encoded. Decoding must happen **once**: a file
/// genuinely named `%2e%2e%2fpasswd` decodes to the literal string `../passwd`, which the
/// validator then refuses — and a second decode would have turned an already-validated name
/// back into a traversal after the check. `+` is *not* treated as a space: this is a path
/// segment, not a form body, and a `+` is a legitimate character in a file name.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

/// Read the `file` and `path` parts of the upload.
///
/// A body larger than the per-file cap is refused by the *cap check* rather than by a read
/// limit, so the message names the same 10 MB the panel's upload hint does. A deployment that
/// also sets a body limit in front of the router will get its own 413 first, which is correct:
/// that limit is about the request, this one is about the workspace.
async fn read_upload(mut multipart: Multipart) -> Result<Upload, ApiError> {
    let mut path: Option<String> = None;
    let mut upload: Option<Upload> = None;

    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        match field.name() {
            Some("path") => {
                let raw = field.text().await.map_err(multipart_error)?;
                path = Some(raw.trim().to_owned());
            }
            Some("file") => {
                let content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                let bytes = field.bytes().await.map_err(multipart_error)?;
                upload = Some(Upload {
                    path: String::new(),
                    content_type,
                    bytes: bytes.to_vec(),
                });
            }
            // Drain anything else so the parser can reach the next part.
            _ => {
                field.bytes().await.map_err(multipart_error)?;
            }
        }
    }

    let upload = upload.ok_or_else(|| {
        ApiError::bad_request(
            "workspace.missing_file",
            "the request carries no `file` part — send the upload as multipart/form-data",
        )
    })?;
    // A missing path is refused with the rule, not with a default. Defaulting to the uploaded
    // filename would put a browser-chosen name in a namespace the validator is meant to control.
    let path = path.filter(|value| !value.is_empty()).ok_or_else(|| {
        ApiError::bad_request(
            "workspace.missing_path",
            "the request carries no `path` part — a workspace path is not taken from the \
             uploaded filename",
        )
    })?;
    Ok(Upload { path, ..upload })
}

/// Map a multipart failure; a body over the transport limit is a 413.
fn multipart_error(error: axum::extract::multipart::MultipartError) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "workspace.file_too_large",
            format!("the upload is larger than the {MAX_FILE_BYTES} byte per-file limit"),
        )
    } else {
        ApiError::bad_request("workspace.malformed_upload", error.body_text())
    }
}

/// Map a storage failure onto the API surface.
///
/// A missing bucket is a `503` and not a `500`: the workspace is fine, the dependency is not,
/// and the panel's Retry button is the right answer to the first and the wrong one to the
/// second.
fn storage_error(error: omnion_storage::StorageError) -> ApiError {
    match error {
        omnion_storage::StorageError::NotFound { .. } => ApiError::new(
            StatusCode::NOT_FOUND,
            "workspace.object_missing",
            "the object is not in storage",
        ),
        other => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "dependency_unavailable",
            format!("object storage: {other}"),
        ),
    }
}

/// The workspace's own limits, re-exported so the admin client and the tests name the same
/// numbers instead of hardcoding them.
pub use omnion_ai_hub::workspace::{MAX_AGENT_BYTES, MAX_PATH_CHARS};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_encoded_path_decodes_once() {
        assert_eq!(percent_decode("notes.md"), "notes.md");
        assert_eq!(percent_decode("data%2Fq3.csv"), "data/q3.csv");
        assert_eq!(percent_decode("r%C3%A9sum%C3%A9.pdf"), "résumé.pdf");
    }

    #[test]
    fn a_plus_sign_is_a_character_and_not_a_space() {
        // This is a path segment, not a form body. Turning `+` into a space would rename
        // `a+b.csv` to `a b.csv`, and the two are different files.
        assert_eq!(percent_decode("a+b.csv"), "a+b.csv");
    }

    #[test]
    fn a_double_decoded_traversal_still_fails_validation() {
        // `%252e%252e%252fpasswd` decodes once to the literal `%2e%2e%2fpasswd` — a file name
        // with percent signs in it, which is legal — and decoding it *again* is what would
        // produce `../passwd`. The module decodes once, and this test pins that count.
        let once = percent_decode("%252e%252e%252fpasswd");
        assert_eq!(once, "%2e%2e%2fpasswd");
        assert!(workspace::validate_path(&once).is_ok());
    }

    #[test]
    fn a_decoded_traversal_is_refused() {
        let decoded = percent_decode("..%2f..%2fetc%2fpasswd");
        assert!(workspace::validate_path(&decoded).is_err());
    }

    #[test]
    fn a_malformed_escape_is_left_alone() {
        // A lone `%` or a truncated escape is a literal in a file name, not a decoder bug.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
        assert_eq!(percent_decode("trailing%2"), "trailing%2");
    }
}
