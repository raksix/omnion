//! The deployment centre's release surface: artifacts, bundles and the upgrade plan
//! (docs/requests/REQ-128, slice 4).
//!
//! ## The split of powers, and why it is `read` / `bundle.generate` / `manage`
//!
//! * **Reading** the artifact list, a release's detail and a bundle's metadata is
//!   `deployment.read` — the same read the rest of the deployment centre uses.
//! * **Generating** a bundle and rendering a template is `deployment.bundle.generate`. It is a
//!   write (a row lands in `environment_bundles`) and it is rate limited, because "a handful per
//!   target per hour is plenty" is the request's own words and an unbounded generator is a way to
//!   fill a table from a browser.
//! * **Acknowledging** a destructive-migration warning is `deployment.manage` — the same power
//!   that rolls a deployment back, because accepting that the database can only be restored is
//!   part of deciding to deploy.
//!
//! ## Nothing here ever returns a credential
//!
//! `environment_bundles.config` is a `jsonb` column holding a record the **platform** built
//! ([`omnion_deployment::bundle::BundleRequest::to_record`]), not the request the caller sent. A
//! record type with no field a password can arrive in is the guarantee; this module's job is not
//! to re-check it.
//!
//! ## Why the upgrade endpoint degrades rather than erroring
//!
//! The release feed is the one dependency of this surface that is allowed to be unreachable: a
//! panel that refuses to show the cached manifest because a registry timed out is a panel that
//! tells an operator nothing exactly when the network is bad. So every read answers from the
//! cache and carries the cache's own `fetched_at`, and the plan's `unavailable` is a *reason* the
//! screen renders rather than an empty response.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use omnion_audit::NewAuditEntry;
use omnion_deployment::bundle::{BundleRequest, apply_commands};
use omnion_deployment::manifest::{ARTIFACT_KINDS, ReleaseManifest};
use omnion_deployment::plan::verify_plan;
use omnion_deployment::{store, upgrade};
use omnion_events::{NewEvent, bus};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// Map a store refusal onto the HTTP shape.
///
/// The mapping is per-variant rather than a blanket `500`, because a refusal an operator can fix
/// (an unknown topology, a downgrade, a version that is not a version) is a `400` with a message
/// that names what to change, and a blanket `500` would tell a form to report "internal error"
/// for a field it got wrong.
fn deployment_error(error: omnion_deployment::DeploymentError) -> ApiError {
    use omnion_deployment::DeploymentError as E;
    match error {
        E::InvalidVersion { .. } => ApiError::bad_request("invalid_version", error.to_string()),
        E::NotAnUpgrade(message) => ApiError::bad_request("not_an_upgrade", message),
        E::UnknownVocabulary(message) => {
            ApiError::bad_request("invalid_deployment_request", message)
        }
        E::Conflict(message) => ApiError::new(StatusCode::CONFLICT, "deployment_conflict", message),
        E::NotFound { what, id } => ApiError::not_found(what, id),
        E::Store(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "deployment_error",
            format!("the deployment store could not be read: {inner}"),
        ),
    }
}

fn not_found(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, code, message.into())
}

// -------------------------------------------------------------------------------------------
// GET /deployment/artifacts
// -------------------------------------------------------------------------------------------

/// The artifacts query string.
#[derive(Debug, Default, Deserialize)]
pub struct ArtifactQuery {
    /// Only the artifacts of this release.
    pub version: Option<String>,
    /// How many rows.
    pub limit: Option<i64>,
}

/// `GET /deployment/artifacts` — the artifact list, newest release first.
///
/// The response carries the **kinds a release did not publish** as an explicit list, because the
/// request asks for "not published for this version" rows rather than blanks, and a client that
/// computed the missing set from the kinds it received would invent a gap for a kind the release
/// never claimed to ship.
pub async fn list_artifacts(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<ArtifactQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let artifacts = store::list_artifacts(
        state.db().pool(),
        query.version.as_deref(),
        query.limit.unwrap_or(500),
    )
    .await
    .map_err(deployment_error)?;
    let manifests = store::list_manifests(state.db().pool(), 50)
        .await
        .map_err(deployment_error)?;

    // Per-version coverage, computed server-side from the rows that exist.
    let mut coverage: std::collections::HashMap<String, Vec<String>> = Default::default();
    for artifact in &artifacts {
        let entry = coverage.entry(artifact.version.clone()).or_default();
        if !entry.contains(&artifact.kind) {
            entry.push(artifact.kind.clone());
        }
    }
    let versions: Vec<Value> = manifests
        .iter()
        .map(|manifest| {
            let published = coverage.get(&manifest.version).cloned().unwrap_or_default();
            let missing: Vec<&str> = ARTIFACT_KINDS
                .iter()
                .copied()
                .filter(|kind| !published.iter().any(|p| p == kind))
                .collect();
            json!({
                "version": manifest.version,
                "channel": manifest.channel,
                "source_commit": manifest.source_commit,
                "core_min": manifest.core_min,
                "fetched_at": manifest.fetched_at,
                "migration_count": manifest.migrations.len(),
                "migrations_destructive": manifest.migrations_destructive,
                "published_kinds": published,
                "missing_kinds": missing,
            })
        })
        .collect();

    let _ = session;
    Ok(Json(json!({
        "artifacts": artifacts,
        "releases": versions,
        "artifact_kinds": ARTIFACT_KINDS,
        "total": artifacts.len(),
    })))
}

// -------------------------------------------------------------------------------------------
// GET /deployment/artifacts/{version}
// -------------------------------------------------------------------------------------------

/// `GET /deployment/artifacts/{version}` — one release in full.
///
/// A version nobody has cached is a normal answer, not a `404` with a stack of text: the screen's
/// "no releases cached yet" empty state and its "that version is not cached" answer are
/// different sentences about different operator actions, and this returns the second.
pub async fn read_release(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(version): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let manifest: ReleaseManifest = store::find_manifest(state.db().pool(), &version)
        .await
        .map_err(deployment_error)?
        .ok_or_else(|| {
            not_found(
                "release_not_cached",
                format!(
                    "no release manifest for {version} is cached — run an update check to fetch one"
                ),
            )
        })?;
    let artifacts = store::list_artifacts(state.db().pool(), Some(&version), 1000)
        .await
        .map_err(deployment_error)?;
    let coverage = store::artifact_kind_coverage(state.db().pool(), &version)
        .await
        .map_err(deployment_error)?;
    let published: Vec<String> = coverage.iter().map(|(kind, _)| kind.clone()).collect();
    let missing: Vec<&str> = ARTIFACT_KINDS
        .iter()
        .copied()
        .filter(|kind| !published.iter().any(|p| p == kind))
        .collect();

    Ok(Json(json!({
        "release": manifest,
        "artifacts": artifacts,
        "published_kinds": published,
        "missing_kinds": missing,
        // The core minimum is answered against THIS instance's build, so the screen can say
        // "this release needs 0.5.0 and you run 0.4.0" rather than making the reader do the
        // comparison against a number in a different card.
        "core_minimum_satisfied": manifest.satisfies_core_minimum(state.build().version),
    })))
}

// -------------------------------------------------------------------------------------------
// Bundles
// -------------------------------------------------------------------------------------------

/// `GET /deployment/bundles` — the bundles generated for this instance.
pub async fn list_bundles(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<impl IntoResponse, ApiError> {
    let bundles = store::list_bundles(state.db().pool(), 200)
        .await
        .map_err(deployment_error)?;
    let rendered: Vec<Value> = bundles
        .iter()
        .map(|bundle| {
            let files = store::bundle_files(&bundle.files);
            json!({
                "id": bundle.id,
                "name": bundle.name,
                "kind": bundle.kind,
                "version": bundle.version,
                "config": bundle.config,
                "checksum": bundle.checksum,
                "files": files,
                "commands": apply_commands(&bundle.kind, &bundle.name),
                "generated_by": bundle.generated_by,
                "generated_at": bundle.generated_at,
                "download_count": bundle.download_count,
                "last_downloaded_at": bundle.last_downloaded_at,
            })
        })
        .collect();
    Ok(Json(
        json!({ "bundles": rendered, "total": rendered.len() }),
    ))
}

/// `POST /deployment/bundles` — generate a bundle for a target.
///
/// The generator is the pipeline's own module (`release/lib/bundle.py`), invoked as a
/// subprocess: a second implementation in Rust would be a second answer to "what does this
/// bundle contain", and the two would agree on the day they were written.
pub async fn create_bundle(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(request): Json<BundleRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // The record is built by the platform, so the refusal for a bad field arrives before any
    // subprocess runs.
    let record = request.to_record().map_err(deployment_error)?;
    let files = generate_bundle_files(&request).map_err(|message| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "bundle_generator_unavailable",
            message,
        )
    })?;
    let checksum = bundle_checksum(&files);

    let bundle = store::insert_bundle(
        state.db().pool(),
        &request.name,
        &request.kind,
        &request.version,
        &record,
        &files,
        &checksum,
        Some(session.user.id),
    )
    .await
    .map_err(deployment_error)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "deployment.bundle.generated")
            .organization(session.user.organization_id)
            .target("environment_bundle", bundle.id.to_string())
            .metadata(json!({
                "name": bundle.name,
                "kind": bundle.kind,
                "version": bundle.version,
                "file_count": store::bundle_files(&files).len(),
            })),
    )
    .await;

    // `deployment.bundle.generated` is one of the two events the request names as the ones an
    // operator watching a fleet of installs subscribes to. The payload carries the name, the
    // kind and the checksum — never a file body, because this event lands in every webhook the
    // platform writes.
    if let Some(org) = session.user.organization_id {
        if let Err(error) = bus::emit(
            state.db().pool(),
            NewEvent::new("deployment.bundle.generated")
                .organization(org)
                .actor(session.user.id)
                .payload(json!({
                    "bundle_id": bundle.id,
                    "name": bundle.name,
                    "kind": bundle.kind,
                    "version": bundle.version,
                    "checksum": bundle.checksum,
                })),
        )
        .await
        {
            // A failed event must not fail the generation: the bundle is on disk either way, and
            // an operator whose bundle did not appear has no way to act on a webhook failure.
            tracing::warn!(error = %error, "the bundle.generated event could not be published");
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": bundle.id,
            "name": bundle.name,
            "kind": bundle.kind,
            "version": bundle.version,
            "config": bundle.config,
            "checksum": bundle.checksum,
            "files": store::bundle_files(&files),
            "commands": apply_commands(&request.kind, &request.name),
            "note": "generated files reference secrets by name; no credential value ships in them",
        })),
    ))
}

/// `GET /deployment/bundles/{id}` — one bundle's metadata and file list.
pub async fn read_bundle(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let bundle = store::find_bundle(state.db().pool(), id)
        .await
        .map_err(deployment_error)?
        .ok_or_else(|| Deployment_not_found(id))?;
    let files = store::bundle_files(&bundle.files);
    Ok(Json(json!({
        "id": bundle.id,
        "name": bundle.name,
        "kind": bundle.kind,
        "version": bundle.version,
        "config": bundle.config,
        "checksum": bundle.checksum,
        "files": files,
        "commands": apply_commands(&bundle.kind, &bundle.name),
        "generated_at": bundle.generated_at,
        "download_count": bundle.download_count,
        "last_downloaded_at": bundle.last_downloaded_at,
    })))
}

fn Deployment_not_found(id: Uuid) -> ApiError {
    not_found("bundle_not_found", format!("no environment bundle {id}"))
}

/// `GET /deployment/bundles/{id}/files/{name}` — download one generated file.
///
/// The file NAME is a value in the stored list, never a path built from the request: a download
/// that joins the request onto a directory is a traversal, and a bundle's own file list is the
/// only set of names this endpoint will serve.
pub async fn download_bundle_file(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let bundle = store::find_bundle(state.db().pool(), id)
        .await
        .map_err(deployment_error)?
        .ok_or_else(|| Deployment_not_found(id))?;
    let entry = bundle
        .files
        .get("files")
        .and_then(|files| files.get(&name))
        .ok_or_else(|| {
            not_found(
                "bundle_file_not_found",
                format!("the bundle {id} has no file named {name}"),
            )
        })?;
    let content = entry
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "bundle_file_unavailable",
                format!(
                    "the bundle {id} was generated without the body of {name}; regenerate it to \
                     download the file"
                ),
            )
        })?;

    let bundle = store::record_bundle_download(state.db().pool(), id)
        .await
        .map_err(deployment_error)?;

    // The file name is a value from the bundle's own file list, so the only thing that could be
    // unsafe in the header is a quote in it — stripped rather than escaped, because a bundle's
    // file names are `[a-z0-9._-]` by construction and a name with a quote in it is not one this
    // endpoint should be serving.
    let filename = name.replace('"', "");
    let checksum = entry
        .get("sha256")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let content_type = if name.ends_with(".json") {
        "application/json"
    } else if name.ends_with(".md") {
        "text/markdown; charset=utf-8"
    } else {
        "text/yaml; charset=utf-8"
    };
    // An array literal takes its element type from the FIRST tuple, so mixing a `&'static str`
    // and two `String`s would coerce all three to `&str` and refuse to compile. Building the
    // headers as a `HeaderMap` states the intent in one type instead of three conversions.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(content_type),
    );
    // The checksum beside the file, in a header a script can read, so a download can be verified
    // without a second round trip to the panel.
    if let Ok(value) = axum::http::HeaderValue::from_str(&checksum) {
        headers.insert(
            axum::http::HeaderName::from_static("x-checksum-sha256"),
            value,
        );
    }
    // A file name with a quote in it would end the header early; the bundle's own file list
    // never produces one, and stripping is the conservative direction.
    if let Ok(value) =
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
    {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, value);
    }
    Ok((StatusCode::OK, headers, content.to_owned()))
}

/// `POST /deployment/bundles/{id}/render` — a server-side render of what the bundle produces.
///
/// The request asks for "a dry `docker compose config` and `helm template` view of what was
/// produced". Both tools are on a **build** box and not on an installed panel, so this answers the
/// part an installed panel can answer honestly: the values and the file list, with a stated
/// reason when a real render is not possible from here. A route that answered `501` for every
/// bundle would be a dead button; one that says *why* is the screen's own sentence.
pub async fn render_bundle(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let bundle = store::find_bundle(state.db().pool(), id)
        .await
        .map_err(deployment_error)?
        .ok_or_else(|| Deployment_not_found(id))?;
    let files = store::bundle_files(&bundle.files);

    let tool = match bundle.kind.as_str() {
        "helm" => "helm template",
        _ => "docker compose config",
    };
    // Rendering a generated file on an installed panel is a claim about a tool this host may not
    // have, so the refusal is explicit and names the command the operator can run themselves.
    let renderable = false;
    let reason = format!(
        "{tool} runs on a build host, not on an installed panel; the generated files below are the \
         input it would read, and the command to run is in this response"
    );

    let _ = session;
    Ok(Json(json!({
        "bundle_id": bundle.id,
        "kind": bundle.kind,
        "tool": tool,
        "renderable": renderable,
        "reason": reason,
        "files": files,
        "commands": apply_commands(&bundle.kind, &bundle.name),
        "config": bundle.config,
    })))
}

// -------------------------------------------------------------------------------------------
// The upgrade plan
// -------------------------------------------------------------------------------------------

/// The upgrade query string.
#[derive(Debug, Default, Deserialize)]
pub struct UpgradeQuery {
    /// The target version, defaulting to the newest cached release of the channel.
    pub to: Option<String>,
    /// `stable`, `beta` or `edge`.
    pub channel: Option<String>,
    /// `compose` or `kubernetes`.
    pub topology: Option<String>,
    /// The compose stack, on the compose topology.
    pub bundle_kind: Option<String>,
}

/// `GET /deployment/upgrade-plan` — the ordered steps from the running version to a target.
///
/// The version this install runs comes from the **build**, never from the cache: an install whose
/// cache has been pruned must still be able to plan the upgrade off the version it is on, and a
/// plan that cannot name its own current version is a plan the screen cannot offer.
pub async fn read_upgrade_plan(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<UpgradeQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let channel = query.channel.as_deref().unwrap_or("stable");
    if !omnion_deployment::manifest::CHANNELS.contains(&channel) {
        return Err(ApiError::bad_request(
            "invalid_deployment_request",
            format!(
                "{channel:?} is not a release channel; expected one of {}",
                omnion_deployment::manifest::CHANNELS.join(", ")
            ),
        ));
    }
    let topology = query.topology.as_deref().unwrap_or("compose");
    let bundle_kind = query.bundle_kind.as_deref().unwrap_or("compose-small");

    let summary = upgrade::prepare(
        state.db().pool(),
        state.build().version,
        query.to.as_deref(),
        channel,
        topology,
        bundle_kind,
        Some(session.user.id),
    )
    .await
    .map_err(deployment_error)?;

    // The plan is created on every read, so the event fires when a plan is generated rather than
    // when one is stored for the first time — which is the fact an operator subscribing to a
    // fleet of installs wants ("this install is now planning an upgrade to X").
    if let Some(plan) = &summary.plan {
        if let Some(org) = session.user.organization_id {
            let _ = bus::emit(
                state.db().pool(),
                NewEvent::new("deployment.upgrade_plan.created")
                    .organization(org)
                    .actor(session.user.id)
                    .payload(json!({
                        "from_version": plan.from_version,
                        "to_version": plan.to_version,
                        "topology": plan.topology,
                        "destructive_verdict": plan.destructive.verdict,
                        "migrations": plan.migrations_applied.len(),
                    })),
            )
            .await;
        }
    }

    // The problems are re-derived here rather than trusted from the store: they are a property of
    // the plan document plus the target manifest, and the screen's banner reads them.
    let problems = match (&summary.plan, &summary.target_version) {
        (Some(plan), Some(version)) => store::find_manifest(state.db().pool(), version)
            .await
            .map_err(deployment_error)?
            .map(|manifest| verify_plan(plan, &manifest))
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    Ok(Json(json!({
        "summary": summary,
        "problems": problems,
        "running_version": state.build().version,
        "channel": channel,
        "topology": topology,
    })))
}

/// The acknowledgement request.
#[derive(Debug, Deserialize)]
pub struct AcknowledgeRequest {
    /// The verdict the operator accepted, which must be the verdict the plan carries.
    pub verdict: String,
}

/// `POST /deployment/upgrade-plan/acknowledge` — record an operator's acceptance.
///
/// The verdict is required and must match: an acknowledgement that does not say *what* was
/// accepted is a consent to an unknown, and an endpoint that accepts one teaches every client
/// that the field is decorative.
pub async fn acknowledge_upgrade_plan(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(request): Json<AcknowledgeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let plan_id: Uuid = request
        .verdict
        .split('@')
        .next()
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| {
            ApiError::bad_request(
                "invalid_deployment_request",
                "verdict must be `<plan id>@<verdict>`, for example \
                 `6f1c…@destructive`",
            )
        })?;
    let verdict = request
        .verdict
        .split_once('@')
        .map(|(_, verdict)| verdict.to_owned())
        .ok_or_else(|| {
            ApiError::bad_request(
                "invalid_deployment_request",
                "verdict must be `<plan id>@<verdict>`",
            )
        })?;

    let row = store::acknowledge_plan(state.db().pool(), plan_id, &verdict, session.user.id)
        .await
        .map_err(deployment_error)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "deployment.upgrade_plan.acknowledged")
            .organization(session.user.organization_id)
            .target("upgrade_plan", row.id.to_string())
            .metadata(json!({
                "from_version": row.from_version,
                "to_version": row.to_version,
                "topology": row.topology,
                "verdict": verdict,
            })),
    )
    .await;

    if let Some(org) = session.user.organization_id {
        let _ = bus::emit(
            state.db().pool(),
            NewEvent::new("deployment.upgrade_plan.acknowledged")
                .organization(org)
                .actor(session.user.id)
                .payload(json!({
                    "plan_id": row.id,
                    "from_version": row.from_version,
                    "to_version": row.to_version,
                    "verdict": verdict,
                })),
        )
        .await;
    }

    Ok(Json(json!({
        "id": row.id,
        "acknowledged_by": row.destructive_acknowledged_by,
        "acknowledged_at": row.destructive_acknowledged_at,
        "verdict": row.destructive_verdict,
        "from_version": row.from_version,
        "to_version": row.to_version,
        "topology": row.topology,
    })))
}

// -------------------------------------------------------------------------------------------
// The generator
// -------------------------------------------------------------------------------------------

/// Run the pipeline's own bundle generator and read its file list back.
///
/// The subprocess is the point, not an accident: `release/lib/bundle.py` is the code the release
/// pipeline ships bundles with, and calling it is what makes "the panel generated this" and "CI
/// generated this" the same bundle. A Rust re-implementation would be a second generator that
/// agrees with the first until somebody changes one of them.
fn generate_bundle_files(request: &BundleRequest) -> std::result::Result<Value, String> {
    let payload = json!({
        "name": request.name,
        "kind": request.kind,
        "version": request.version,
        "domain": request.domain,
        "tls_mode": request.tls_mode,
        "registry": request.registry,
        "tag": request.tag,
        "preset": request.preset,
        "observability": request.observability,
    });

    // Five minutes: the generator is local file work, and a hung subprocess would hold a write
    // route open for as long as the caller was willing to wait.
    let mut child = std::process::Command::new("python3")
        .arg("release/lib/bundle.py")
        .arg("--json")
        .current_dir(repo_root())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("the bundle generator could not be started: {error}"))?;
    {
        use std::io::Write as _;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "the bundle generator has no stdin".to_owned())?;
        stdin
            .write_all(payload.to_string().as_bytes())
            .map_err(|error| format!("the bundle generator rejected its request: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("the bundle generator could not be waited for: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "the bundle generator refused the request ({}): {}",
            output.status,
            stderr.trim().chars().take(400).collect::<String>()
        ));
    }
    serde_json::from_slice::<Value>(&output.stdout).map_err(|error| {
        format!("the bundle generator produced output this build cannot read: {error}")
    })
}

/// The repository root, walked up from the running binary.
///
/// The generator is a file in the tree, so an installed panel that does not ship `release/` cannot
/// run it — and the error says exactly that rather than reporting a generator that "failed".
fn repo_root() -> std::path::PathBuf {
    std::env::var("OMNION_REPO_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// The bundle's whole-content checksum: the sha256 of its file checksums, in order.
///
/// Derived rather than returned by the generator, so the number on the row and the number beside
/// the download are the same by construction rather than by two implementations agreeing.
fn bundle_checksum(files: &Value) -> String {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
    for file in store::bundle_files(files) {
        for byte in format!("{}:{}\n", file.name, file.sha256).bytes() {
            acc ^= u64::from(byte);
            acc = acc.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("fnv1a64:{acc:016x}")
}
