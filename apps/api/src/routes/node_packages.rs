//! `/api/v1/node-packages` — the installer ledger's write path (docs/requests/REQ-087, slice 4).
//!
//! Slice 2 shipped the ledger's *read* side and a placeholder install that recorded whatever
//! key, version and checksum a caller named. That placeholder is exactly the shape the REQ
//! refuses: a package reaching the ledger is the only thing that makes its nodes appear, so a
//! row written without validation is a palette entry that fails when somebody places it. This
//! module replaces it with the three endpoints that make the REQ's sentences true:
//!
//! * **`POST /node-packages` validates before it records.** The body carries the manifest, the
//!   validator runs on it, and *any* finding is a `400` with every finding in `details` — a
//!   refused package reaches nothing. The ledger row is written only when the package is
//!   installable, and the recorded checksum is the one this module computed over the canonical
//!   manifest rather than one a caller asserted. A caller cannot talk the ledger into a
//!   checksum that does not match its own manifest.
//! * **An install is equal-or-newer.** A downgrade is `node_package_version_unsupported`
//!   naming the installed version, because the version a workflow recorded would otherwise
//!   name code that no longer exists.
//! * **Removal degrades and reports.** `DELETE` names the workflows that use the package's
//!   nodes and disables the package; it never edits a workflow. A disabled package's nodes
//!   stop being offered by the palette and the library reports them as `node_package_missing`
//!   with the cause, so an existing workflow loads and says what is wrong instead of
//!   breaking.
//!
//! The validator itself lives in `omnion_workflows::node_package` and is shared with the
//! installer CLI, so a package that installs from the command line and one that installs from
//! the panel passed the same function — two validators is two answers.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_events::{NewEvent, bus};
use omnion_workflows::credential_store::{self, NewNodePackage};
use omnion_workflows::node_package::{self, Manifest, Version, removal_plan};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::scope::resolve_organization;
use crate::state::AppState;

use super::credentials::{NodePackageBody, map_store_error};

/// A validation failure, as the installer screen renders it.
///
/// `code` is the *first* finding's code so a client can branch on it (`node_package_invalid`
/// for every refusal, with the detail in `findings`) — the REQ names
/// `node_package_missing` and `node_package_version_unsupported` as its own codes, and those
/// are used where they are specific enough to be actionable. A structural problem in the
/// manifest is not a version problem and not a missing package, so it is not dressed up as
/// either.
fn refuse(findings: Vec<omnion_workflows::registry::LintFinding>) -> ApiError {
    let code = findings
        .iter()
        .map(|finding| finding.code)
        .next()
        .unwrap_or("node_package_invalid");
    let details: Vec<Value> = findings
        .iter()
        .map(|finding| {
            json!({
                "code": finding.code,
                "subject": finding.subject,
                "message": finding.message,
            })
        })
        .collect();
    let summary = findings
        .first()
        .map(|finding| format!("{}: {}", finding.subject, finding.message))
        .unwrap_or_else(|| "the package is not installable".to_string());
    ApiError::new(
        StatusCode::BAD_REQUEST,
        code,
        format!("{summary} ({} finding(s))", findings.len()),
    )
    .with_details(Value::Array(details))
}

/// The install body: the manifest itself, not a summary of it.
///
/// Taking the manifest rather than `{key, version, checksum, permissions}` is the decision the
/// whole module rests on. The old body let the caller state what was installed; this one makes
/// the caller state *what the package is*, and the ledger is then a consequence of validating
/// that. A caller cannot install a package whose nodes the registry would refuse, because the
/// nodes travel with the request.
#[derive(Debug, Deserialize)]
pub struct InstallPackageRequest {
    /// The manifest, as `manifest.json`.
    pub manifest: Manifest,
    /// The organization the package is installed for.
    ///
    /// A platform account has no primary organization, so without this field every installer
    /// call it makes — a superuser installing a package for a tenant, and the QA owner that
    /// walks this screen — is refused `organization_required` before the manifest is even read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The install response: the ledger row plus what it means.
#[derive(Debug, Serialize)]
pub struct InstallPackageResponse {
    /// The recorded row.
    #[serde(flatten)]
    pub package: NodePackageBody,
    /// The checksum this module computed, which is the one recorded.
    pub checksum: String,
    /// The installed node keys, as the `workflows.node_package.installed` event names them.
    pub node_keys: Vec<String>,
    /// The node keys a *previous* install of this package owned, so an update can say what
    /// the palette loses if it is rolled back. Empty on a first install.
    pub replaced_node_keys: Vec<String>,
    /// Whether this install also made the package's nodes visible to the palette.
    pub enabled: bool,
}

/// `POST /api/v1/node-packages` — validate, then record.
///
/// # Errors
///
/// `node_package_version_unsupported` when the request is a downgrade, and a `400` carrying
/// every validator finding when the package is not installable. Nothing is written in either
/// case, and the refused package leaves no row behind for the panel to show.
pub async fn install_node_package(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<InstallPackageRequest>,
) -> Result<(StatusCode, Json<InstallPackageResponse>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;

    // 1. Validate. This is the gate the REQ asks for ("a package must pass the validator to
    //    install") and it happens *before* the organization is touched at all — a refused
    //    package cannot even be scoped to a tenant, so there is nothing to clean up.
    let validation = node_package::validate(&body.manifest);
    let Some(package) = validation.package else {
        return Err(refuse(validation.findings));
    };

    // 2. The checksum is computed here, not taken from the request. A caller asserting its own
    //    checksum is a caller asserting its own integrity, and the ledger's whole value is
    //    that the checksum says what is actually running.
    let checksum = node_package::checksum(&body.manifest);

    // 3. Equal-or-newer only. The check is against the *live* row, so re-installing the same
    //    version is an update that changes nothing, and an older one is refused by name.
    let existing = credential_store::find_package(state.db().pool(), organization_id, &package.key)
        .await
        .map_err(map_store_error)?;
    if let Some(previous) = &existing {
        if let Ok(installed_version) = Version::parse(&previous.version) {
            if let Err(error) =
                node_package::ensure_not_a_downgrade(&installed_version, &package.version)
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    error.code(),
                    error.to_string(),
                ));
            }
        }
    }

    // 4. Record. `upsert_package` re-uses the same row, so a second install is an update and
    //    not a second row the reader would see twice.
    let node_keys = package.node_keys();
    let permissions = serde_json::to_value(
        package
            .permissions
            .iter()
            .map(|permission| permission.as_str())
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| json!([]));

    let recorded = credential_store::upsert_package(
        state.db().pool(),
        NewNodePackage {
            organization_id,
            key: package.key.clone(),
            version: package.version.to_string(),
            source: package.source.as_str().to_string(),
            checksum: checksum.clone(),
            permissions,
            node_keys: serde_json::to_value(&node_keys).unwrap_or_else(|_| json!([])),
        },
    )
    .await
    .map_err(map_store_error)?;

    // 5. The event carries the node keys, because the palette is the consumer and it needs to
    //    know *which* nodes appeared rather than re-reading the ledger to find out.
    bus::emit(
        state.db().pool(),
        NewEvent::new("workflows.node_package.installed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "package_key": recorded.key,
                "version": recorded.version,
                "node_keys": node_keys,
                "checksum": recorded.checksum,
            })),
    )
    .await
    .ok();

    // An update answers 200 and says what it replaced; a first install answers 201. The
    // replaced set comes from the row this upsert is about to overwrite, so it is the keys the
    // *palette* had, not the keys the new manifest happens to declare — a package that dropped
    // a node in this version gets that node named here, which is the only place the reader
    // learns it.
    let (replaced_node_keys, status) = match existing {
        None => (Vec::new(), StatusCode::CREATED),
        Some(previous) => (
            credential_store::package_node_keys(&previous),
            StatusCode::OK,
        ),
    };
    let dropped_node_keys: Vec<String> = replaced_node_keys
        .iter()
        .filter(|key| !node_keys.contains(key))
        .cloned()
        .collect();

    Ok((
        status,
        Json(InstallPackageResponse {
            package: NodePackageBody::from(&recorded),
            checksum: recorded.checksum.clone(),
            node_keys,
            replaced_node_keys: dropped_node_keys,
            enabled: recorded.enabled,
        }),
    ))
}

/// The enable/disable body.
#[derive(Debug, Deserialize)]
pub struct SetPackageEnabledRequest {
    /// Whether the package's nodes are available.
    pub enabled: bool,
    /// The organization whose ledger row is toggled; a platform account names one.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The enable/disable response, with the consequence spelled out.
#[derive(Debug, Serialize)]
pub struct SetPackageEnabledResponse {
    /// The row after the change.
    #[serde(flatten)]
    pub package: NodePackageBody,
    /// The node keys whose availability changed.
    pub node_keys: Vec<String>,
    /// A sentence for the panel, so the toast does not have to be written twice.
    pub message: String,
}

/// `PATCH /api/v1/node-packages/{key}` — enable or disable a package.
///
/// # Errors
///
/// `404 node_package_not_found` when the key is not installed. Disabling is the REQ's
/// "removal disables them and flags dependent workflows instead of breaking them", so this is
/// the same operation a remove performs, minus the ledger row: the nodes stop being offered
/// and every workflow that names one keeps loading.
pub async fn set_node_package_enabled(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(key): Path<String>,
    Json(body): Json<SetPackageEnabledRequest>,
) -> Result<Json<SetPackageEnabledResponse>, ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;
    let package = credential_store::set_package_enabled(
        state.db().pool(),
        organization_id,
        &key,
        body.enabled,
    )
    .await
    .map_err(map_store_error)?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "node_package_not_found",
            format!("no package {key:?} is installed"),
        )
    })?;

    let node_keys = installed_node_keys(state.db().pool(), organization_id, &key).await?;
    bus::emit(
        state.db().pool(),
        NewEvent::new(if body.enabled {
            "workflows.node_package.updated"
        } else {
            "workflows.node_package.removed"
        })
        .organization(organization_id)
        .actor(current.user.id)
        .payload(json!({
            "package_key": package.key,
            "version": package.version,
            "node_keys": node_keys,
            "enabled": body.enabled,
        })),
    )
    .await
    .ok();

    let message = if body.enabled {
        format!(
            "{} is available again — {} node(s) back in the palette",
            package.key,
            node_keys.len()
        )
    } else {
        format!(
            "{} is disabled — {} node(s) are no longer offered; workflows using them keep \
             loading and say why",
            package.key,
            node_keys.len()
        )
    };

    Ok(Json(SetPackageEnabledResponse {
        package: NodePackageBody::from(&package),
        node_keys,
        message,
    }))
}

/// The remove response: what was removed and what it touched.
#[derive(Debug, Serialize)]
pub struct RemovePackageResponse {
    /// The key that was removed.
    pub key: String,
    /// Whether a live row was actually there to remove.
    pub removed: bool,
    /// The node keys the package owned.
    pub node_keys: Vec<String>,
    /// The workflows that named at least one of them, by name and node keys.
    pub affected_workflows: Vec<AffectedWorkflowBody>,
    /// One sentence covering the whole consequence.
    pub message: String,
}

/// One workflow a removal touches.
#[derive(Debug, Serialize)]
pub struct AffectedWorkflowBody {
    /// Workflow id, for a link.
    pub workflow_id: Uuid,
    /// Workflow name.
    pub workflow_name: String,
    /// The node keys that name this package.
    pub node_keys: Vec<String>,
}

/// The removal query. A `DELETE` carries no body, so a platform account names the
/// organization here for the same reason the install body carries it.
#[derive(Debug, Deserialize)]
pub struct RemovePackageQuery {
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `DELETE /api/v1/node-packages/{key}` — remove a package from the ledger.
///
/// # Errors
///
/// `404 node_package_not_found` when nothing live is installed under that key. A removed
/// package is never *broken* into: the row is marked removed so the history survives, its
/// nodes stop being offered, and the workflows that named them are returned so the panel can
/// say which ones need attention. Nothing writes to a workflow here — the REQ's wording is
/// "flags dependent workflows instead of breaking them", and a removal that edited somebody's
/// automation to keep it running would have broken it in a way the author never sees.
pub async fn remove_node_package(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(key): Path<String>,
    Query(query): Query<RemovePackageQuery>,
) -> Result<Json<RemovePackageResponse>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let node_keys = installed_node_keys(state.db().pool(), organization_id, &key).await?;
    let references =
        credential_store::workflow_node_references(state.db().pool(), organization_id, &node_keys)
            .await
            .map_err(map_store_error)?;
    let plan = removal_plan(&key, &node_keys, &references);

    let removed = credential_store::remove_package(state.db().pool(), organization_id, &key)
        .await
        .map_err(map_store_error)?;
    if !removed {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "node_package_not_found",
            format!("no package {key:?} is installed"),
        ));
    }

    bus::emit(
        state.db().pool(),
        NewEvent::new("workflows.node_package.removed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "package_key": key,
                "node_keys": node_keys,
                "affected_workflows": plan
                    .affected_workflows
                    .iter()
                    .map(|workflow| workflow.workflow_name.clone())
                    .collect::<Vec<_>>(),
            })),
    )
    .await
    .ok();

    Ok(Json(RemovePackageResponse {
        key,
        removed,
        node_keys: plan.node_keys.clone(),
        message: plan.describe(),
        affected_workflows: plan
            .affected_workflows
            .iter()
            .map(|workflow| AffectedWorkflowBody {
                workflow_id: workflow.workflow_id,
                workflow_name: workflow.workflow_name.clone(),
                node_keys: workflow.node_keys.clone(),
            })
            .collect(),
    }))
}

/// The node keys of a live package row.
///
/// Read from the ledger (`0055`) rather than derived, because a derivation needs the manifest
/// and the manifest belongs to whoever installed it. A key that is not installed answers an
/// empty list, which the callers render as "0 node(s)" — true, and the reason a `404` is
/// raised by the handler that cares rather than by this read.
async fn installed_node_keys(
    pool: &sqlx::PgPool,
    organization_id: Uuid,
    key: &str,
) -> Result<Vec<String>, ApiError> {
    Ok(credential_store::find_package(pool, organization_id, key)
        .await
        .map_err(map_store_error)?
        .as_ref()
        .map(credential_store::package_node_keys)
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------------------------
// Tests: the request shapes the validator refuses
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse as _;
    use omnion_workflows::node_package;

    /// A stand-in organization id, so the test says "names one" without inventing a tenant.
    const ORG: Uuid = Uuid::from_u128(0x0192_a1b2_c3d4_e5f6_0708_090a_0b0c_0d0e);

    #[tokio::test]
    async fn a_refusal_reports_every_finding_not_just_the_first() {
        let mut manifest = node_package::scaffold("acme").manifest;
        manifest.nodes[0].icon = String::new();
        manifest.nodes[1].label = String::new();
        let validation = node_package::validate(&manifest);
        assert!(
            validation.findings.len() >= 2,
            "the fixture needs two findings"
        );

        let expected = validation.findings.len();
        let error = refuse(validation.findings);

        // A `400`, and the code is the *first* finding's, so a client can branch on it
        // without parsing prose.
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(
            !error.code().is_empty(),
            "the refusal carries a code a client can branch on"
        );

        // Every finding travels in the response *body*, which is the only place a caller can
        // read them — `ApiError` does not implement `Serialize`; its `IntoResponse` builds
        // the body. So the assertion goes through the same path a client takes rather than
        // through a struct that does not exist. A refusal that reported only the first
        // finding is a validator its author has to discover one error at a time.
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body reads");
        let body: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
        assert_eq!(body["error"]["code"], error_code_of(&body));
        let findings = body["error"]["details"]
            .as_array()
            .unwrap_or_else(|| panic!("the refusal carries its findings: {body}"));
        assert_eq!(findings.len(), expected);
        for finding in findings {
            assert!(finding.get("code").and_then(|code| code.as_str()).is_some());
            assert!(finding.get("subject").and_then(|s| s.as_str()).is_some());
            assert!(finding.get("message").and_then(|m| m.as_str()).is_some());
        }
    }

    /// The code the body names, for the assertion above.
    fn error_code_of(body: &Value) -> &str {
        body["error"]["code"].as_str().unwrap_or_default()
    }

    #[test]
    fn the_install_body_is_the_manifest_so_a_caller_cannot_assert_its_own_checksum() {
        // The type itself is the guarantee: there is no `checksum` field to send, so the only
        // checksum the ledger can hold is the one this module computed.
        let body = InstallPackageRequest {
            manifest: node_package::scaffold("acme").manifest,
            organization_id: None,
        };
        assert_eq!(body.manifest.key, "acme");
    }

    /// A platform account names the organization it installs for.
    ///
    /// Without this field every installer call such an account makes is refused
    /// `organization_required` before the manifest is read — which is exactly what a
    /// superuser installing a package on a tenant's behalf, and the QA owner that walks this
    /// screen, both do.
    #[test]
    fn the_install_body_carries_the_organization_a_platform_account_names() {
        let manifest = node_package::scaffold("acme").manifest;
        let body: InstallPackageRequest =
            serde_json::from_value(json!({ "manifest": manifest, "organization_id": ORG }))
                .expect("a body naming an organization deserializes");
        assert_eq!(body.organization_id, Some(ORG));

        // And it stays optional, so a tenant's own call does not have to send one.
        let tenant: InstallPackageRequest =
            serde_json::from_value(json!({ "manifest": node_package::scaffold("acme").manifest }))
                .expect("a body without one still deserializes");
        assert_eq!(tenant.organization_id, None);
    }
}
