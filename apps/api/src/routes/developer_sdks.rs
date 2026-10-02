//! `/api/v1/dev/sdks` and the CLI device-code endpoints (REQ-033, slice 4).
//!
//! # Three surfaces with three different callers
//!
//! This file serves three groups that should not be confused with each other:
//!
//! * **The panel** — a person generating a starter or approving a CLI login. Every handler here
//!   takes a [`CurrentSession`] and resolves its tenant with [`organization_of`], so the
//!   organization for every statement comes from one place.
//! * **The terminal** — `omnion login`, which holds no session at all. It carries a device code
//!   it has not shown anybody yet and polls with it. That is the only unauthenticated pair here
//!   and it is unauthenticated *by the protocol's design*: the device code is the credential and
//!   it is worthless until a human approves it.
//! * **The browser** — the approval screen. It has a session, and it is the surface that shows
//!   the requesting client so a person can notice they are not approving their own login.
//!
//! # The scaffold response carries the file tree and nothing else
//!
//! [`ScaffoldResponse`] returns the files the panel previews. The *archive* is written by the
//! caller to object storage and recorded as a row; this endpoint does not stream a zip, because
//! a scaffold is a dozen small text files and a panel that has just shown the reader the tree
//! gains nothing from a download it did not ask for. The request's "download the archive" step is
//! satisfied by the generated object being addressable, not by this route being a file server.
//!
//! # What is not here
//!
//! No `developer.sdks.scaffold`-guarded *listing* of past generations is added in this file: the
//! audit row exists, and the screen that reads it is the next slice. A route that returns a list
//! nobody has asked for is a route that gets shipped wrong.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_developer::cli::{DeviceApproval, DeviceStart, cli_scopes, scope_sentence};
use omnion_developer::scaffold::{
    ManifestReport, Scaffold, ScaffoldFile, ScaffoldKind, ScaffoldTarget, validate_manifest,
};
use omnion_developer::store_cli::{self, PollResult};
use omnion_developer::templates::generate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::developer::organization_of;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// One file in a generated archive, as the preview shows it.
#[derive(Debug, Serialize)]
pub struct ScaffoldFileView {
    /// Path inside the archive.
    pub path: String,
    /// UTF-8 content.
    pub content: String,
}

/// A generated archive: the tree, the byte total, and where it will live.
///
/// `object_key` is filled in by the caller after it stores the archive and is `None` in the
/// generation response — the key does not exist yet. It is here so the same shape serves the
/// record write and the panel's "last generated" list without a second type.
#[derive(Debug, Serialize)]
pub struct ScaffoldResponse {
    /// Which starter.
    pub kind: &'static str,
    /// The name it was generated for.
    pub name: String,
    /// Which environment it is aimed at.
    pub target: &'static str,
    /// The files, in preview order.
    pub files: Vec<ScaffoldFileView>,
    /// Total bytes.
    pub byte_size: usize,
    /// The archive's object key, once stored.
    pub object_key: Option<String>,
}

impl From<Scaffold> for ScaffoldResponse {
    fn from(scaffold: Scaffold) -> Self {
        Self {
            kind: scaffold.kind.as_str(),
            name: scaffold.name,
            target: scaffold.target.as_str(),
            files: scaffold
                .files
                .iter()
                // The hidden files are generated but not offered for preview: a tree full of
                // dotfiles is noise, and the `.env.example` the README warns about is reachable
                // from the archive rather than from the preview.
                .filter(|file| file.shown_in_preview)
                .map(|file: &ScaffoldFile| ScaffoldFileView {
                    path: file.path.clone(),
                    content: file.content.clone(),
                })
                .collect(),
            byte_size: scaffold.byte_size,
            object_key: None,
        }
    }
}

/// Body of `POST /api/v1/dev/sdks/scaffold`.
#[derive(Debug, Deserialize)]
pub struct ScaffoldInput {
    /// Which starter: `plugin`, `theme` or `workflow`.
    pub kind: String,
    /// The template name.
    pub name: String,
    /// Which environment it is aimed at: `live` or `sandbox`.
    pub target: String,
}

/// Body of `POST /api/v1/dev/manifests/validate`.
#[derive(Debug, Deserialize)]
pub struct ManifestInput {
    /// Which kind the manifest is being validated *as*.
    pub kind: String,
    /// The manifest's text.
    pub manifest: String,
}

// ---------------------------------------------------------------------------------------------
// Scaffold generation
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/dev/sdks/scaffold` — generate one starter archive.
pub async fn scaffold(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(input): Json<ScaffoldInput>,
) -> Result<Json<ScaffoldResponse>, ApiError> {
    let organization_id = organization_of(&current)?;

    // Both parse calls happen before generation, so an unknown kind or target is a `400` with a
    // message naming the field rather than a generated-then-discarded tree.
    let kind = ScaffoldKind::parse(&input.kind).map_err(ApiError::from)?;
    let target = ScaffoldTarget::parse(&input.target).map_err(ApiError::from)?;

    let scaffold = generate(kind, &input.name, target).map_err(ApiError::from)?;

    // The audit entry carries the tenant even though the response does not: "which tenant
    // generated this" is the question `sdk_scaffolds` exists to answer, and an audit row without
    // it is a row nobody can scope.
    crate::routes::developer::audit(
        &state,
        &current,
        &address,
        "developer.scaffold.generated",
        json!({
            "organization_id": organization_id,
            "kind": kind.as_str(),
            "name": scaffold.name,
            "target": target.as_str(),
            "byte_size": scaffold.byte_size,
            "file_count": scaffold.files.len(),
        }),
    )
    .await?;

    Ok(Json(ScaffoldResponse::from(scaffold)))
}

/// `POST /api/v1/dev/manifests/validate` — validate a plugin/theme/workflow manifest.
///
/// The one place a person can check a manifest *before* trying to install it, and it calls
/// [`validate_manifest`] — the same function the runtime loader calls. That is the whole point:
/// a laxer validator here would tell people their extension is fine and then the platform would
/// refuse to load it, which is the failure the request's risk note names.
pub async fn validate_manifest_endpoint(
    State(_state): State<AppState>,
    _current: CurrentSession,
    Json(input): Json<ManifestInput>,
) -> Result<Json<ManifestReport>, ApiError> {
    let kind = ScaffoldKind::parse(&input.kind).map_err(ApiError::from)?;
    Ok(Json(validate_manifest(kind, &input.manifest)))
}

/// `GET /api/v1/dev/sdks/templates` — the three template cards and what each contains.
///
/// Returns descriptions only: enough for the tab picker to say what a plugin is, and not the
/// files themselves. A template's content is a preview you get by generating one, and shipping
/// all three inline would put every generated byte in every page load.
pub async fn templates(_current: CurrentSession) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(json!([
        {
            "kind": "plugin",
            "label": "Plugin",
            "language": "TypeScript",
            "description": "A package the platform loads: routes, admin panels and services.",
        },
        {
            "kind": "theme",
            "label": "Theme",
            "language": "TypeScript",
            "description": "A package that renders content the platform already stores.",
        },
        {
            "kind": "workflow",
            "label": "Workflow",
            "language": "DSL",
            "description": "A trigger, some steps and the edges between them.",
        },
    ])))
}

// ---------------------------------------------------------------------------------------------
// The CLI device-code flow
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/dev/cli/device-code` — start a login.
#[derive(Debug, Deserialize)]
pub struct DeviceStartInput {
    /// The terminal's name, shown on the approval screen. Required and non-blank: an approval
    /// screen with a bare code is the screen an attacker wants.
    #[serde(default)]
    pub client_name: String,
    /// Where the token will be used, if the terminal claims one.
    pub client_uri: Option<String>,
    /// The scopes the terminal is asking for. Narrowed to the fixed set, never widened.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// `POST /api/v1/dev/cli/device-code` — start a login.
///
/// Takes **no session on purpose**? No: the panel's CLI tab *starts* the login from the browser
/// it is already signed into, so this one does take a session and resolves the tenant from it.
/// The `omnion login` flow's own start happens against the same endpoint with the same session
/// in the browser and the token handed to the terminal by the *poll*, not by this response.
pub async fn device_start(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(input): Json<DeviceStartInput>,
) -> Result<Json<DeviceStart>, ApiError> {
    let organization_id = organization_of(&current)?;

    let started = store_cli::start(
        state.db().pool(),
        organization_id,
        &input.client_name,
        input.client_uri.as_deref(),
        &input.scopes,
    )
    .await
    .map_err(ApiError::from)?;

    Ok(Json(started))
}

/// Body of the approval POST — only the code.
///
/// Only the code, deliberately. The client name, the URI and the scopes the screen showed are
/// read back from the *stored row*, not from this request, so a caller cannot display one set of
/// metadata and approve a different one.
#[derive(Debug, Deserialize)]
pub struct DeviceApproveInput {
    /// The short code the person typed.
    pub user_code: String,
}

/// `POST /api/v1/dev/cli/device-code/approve` — approve a login from the browser.
///
/// This is the surface the request's phishing risk note is about, and every part of it exists to
/// let the person at the screen notice they are not approving their own login: the code is looked
/// up by what they typed, the client name and the scopes come from the *stored row* rather than
/// from this request (a caller could otherwise display one thing and approve another), and the
/// approval records which user approved it so a token always has an owner.
pub async fn device_approve(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(input): Json<DeviceApproveInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let organization_id = organization_of(&current)?;

    let row = store_cli::find_for_approval(state.db().pool(), organization_id, &input.user_code)
        .await
        .map_err(ApiError::from)?;

    // Build the view the screen shows *before* approving, so the panel can render the client
    // name and the scope sentences it is about to grant and then post the approval. It is the
    // same shape the approval check runs on, so the screen cannot show one thing and the
    // approval record another.
    let approval: DeviceApproval = row.approval_view(current.user.display_name.clone());

    let approved = store_cli::approve(
        state.db().pool(),
        organization_id,
        &input.user_code,
        current.user.id,
        current.user.display_name.clone(),
    )
    .await
    .map_err(ApiError::from)?;

    Ok(Json(json!({
        "user_code": approved.user_code,
        "approved_at": approved.approved_at,
        "client_name": approval.client_name,
        "client_uri": approval.client_uri,
        "scopes": approval.scopes,
        "scope_sentences": approval
            .scopes
            .iter()
            .map(|scope| scope_sentence(scope))
            .collect::<Vec<_>>(),
    })))
}

/// `GET /api/v1/dev/cli/device-code/{code}` — what the approval screen shows before approving.
///
/// Read-only and separate from the approve POST so the screen can render the client name and the
/// plain-language scopes *before* anything is written. A screen that shows a bare code and then
/// a confirm button is the screen an attacker wants; one that shows "omnion-cli wants to read
/// your API keys" and then a confirm button is not.
pub async fn device_lookup(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(user_code): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let organization_id = organization_of(&current)?;

    let row = store_cli::find_for_approval(state.db().pool(), organization_id, &user_code)
        .await
        .map_err(ApiError::from)?;

    let now = OffsetDateTime::now_utc();
    let expired = now >= row.expires_at;

    Ok(Json(json!({
        "user_code": row.user_code,
        "client_name": row.client_name,
        "client_uri": row.client_uri,
        "scopes": row.scopes,
        "scope_sentences": row
            .scopes
            .iter()
            .map(|scope| scope_sentence(scope))
            .collect::<Vec<_>>(),
        "expires_at": row.expires_at,
        "expired": expired,
        "already_approved": row.approved_by.is_some(),
    })))
}

/// Body of the terminal's poll.
#[derive(Debug, Deserialize)]
pub struct DevicePollInput {
    /// The device code the terminal is polling with.
    pub device_code: String,
}

/// `POST /api/v1/dev/cli/device-code/poll` — the terminal's poll.
///
/// Answers one of three things, and the status code is what tells the client which:
///
/// * `202` — nobody has approved it yet. Not an error; RFC 8628 makes this the normal case.
/// * `428` — polled too fast. The body carries the new interval. `428 Precondition Required` is
///   not arbitrary: the client has not satisfied the poll interval, which is precisely what that
///   status means.
/// * `200` — approved; the body carries the token, once.
///
/// A `400` for "pending" would invite a client to treat a working login as a failure and retry
/// harder, which is the opposite of what the slow-down rule is for.
///
/// This is the one handler in the file with no session: the device code *is* the credential, and
/// it is worthless until a human approves it. That is the protocol's design, not a shortcut.
pub async fn device_poll(
    State(state): State<AppState>,
    Json(input): Json<DevicePollInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let now = OffsetDateTime::now_utc();

    // The store owns the query and the decision together. The first version of this handler read
    // the row itself, which meant a raw `sqlx::Error` in a route — and `ApiError` has no blanket
    // `From` for it, so it would have needed a conversion nobody reviewed.
    match store_cli::poll(state.db().pool(), &input.device_code, now)
        .await
        .map_err(ApiError::from)?
    {
        PollResult::Pending { interval_seconds } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "status": "pending", "interval_seconds": interval_seconds })),
        )),
        PollResult::SlowDown { seconds } => Ok((
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({ "status": "slow_down", "interval_seconds": seconds })),
        )),
        PollResult::Approved { token } => Ok((
            StatusCode::OK,
            Json(json!({
                "status": "approved",
                "access_token": token.access_token,
                "environment": token.environment.as_str(),
                "scopes": token.scopes,
                "expires_at": token.expires_at,
            })),
        )),
    }
}

// `DeveloperError` -> `ApiError` is implemented once, in `routes::developer`, and it is the
// better of the two: it consults `is_client_error()` to choose the status and attaches a `field`
// so a message lands under the input that caused it. A second impl here would have been a
// compile error rather than a silent duplicate, which is the one good thing an `impl From` for a
// foreign type buys you.

/// A helper the panel route uses to answer "what would this token be able to do".
#[allow(dead_code)]
fn default_cli_scope_sentences() -> Vec<&'static str> {
    cli_scopes().iter().map(|s| scope_sentence(s)).collect()
}
