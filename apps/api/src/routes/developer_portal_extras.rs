//! The two developer-portal endpoints that main's REQ-022 slice added and this branch did not.
//!
//! # Why this file exists at all
//!
//! `crates/developer` was created independently on `main` (REQ-022, slice 1) and on this branch
//! (REQ-033, the whole developer platform). The add/add merge therefore had to choose one crate,
//! and the resolution kept **this branch's** — it is a strict superset: twenty modules against
//! main's eight, and it already answers every endpoint main's slice registered *except* two.
//! Those two live here rather than being appended to `routes/developer.rs` so that the graft is
//! one reviewable file instead of a silent edit inside a file two writers own.
//!
//! ## What was taken and what was deliberately dropped
//!
//! * [`list_scopes`] — the grouped, grantability-aware catalogue the create-key picker is built
//!   from. Kept verbatim, because it reads `omnion_permissions::catalogue::CATALOGUE` and derives
//!   `grantable` from the caller's *effective* permissions. That derivation is the whole point:
//!   offering a scope the caller cannot delegate is a form that submits and is then refused.
//!   Re-deriving it against this branch's key model would risk answering from the crate's own
//!   scope list instead of the catalogue, which is the version of this endpoint that lets a
//!   read-only operator mint a key they should not be able to.
//! * [`sandbox_probe`] — the one route that accepts an API key in place of a session, so the REQ's
//!   "a key authenticates on a guarded endpoint and is rejected after revocation" criterion has a
//!   real route behind it. Kept verbatim.
//!
//! Dropped on purpose: main's `keys_store` / `logs_store` / `keys` / `logs` / `KeyView` /
//! `IssuedKey` / `UsagePoint`, its `api_request_logs` reader, and the `DEV_*` permission names it
//! introduced (`developer.logs.read`). This branch answers the same reads from
//! `omnion_developer::store` against a richer schema (`prefix` rather than `key_prefix`, a rate
//! tier, an IP allowlist, p95 latency), so a second reader over the same table would be a second
//! answer to the same question, drifting the day a column is added.
//!
//! ## The one place this file differs from main, and it is a fix
//!
//! Main registered these under `/api/v1/developer/scopes` and `/api/v1/developer/sandbox/probe`.
//! This branch mounts its developer platform at `/api/v1` with literal segments (`/api-keys`,
//! `/request-logs`, `/dev/openapi.json`), so the same two are registered here at `/developer/scopes`
//! and `/developer/sandbox/probe` **on the same router**, keeping main's paths byte-for-byte.
//! Changing a path another writer's admin screens call is how a merge silently breaks a screen
//! that no test on this branch covers.

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use serde_json::json;

use crate::auth::CurrentSession;
use crate::state::AppState;

/// The environments a key may carry, read from the crate rather than repeated here.
///
/// [`omnion_developer::Environment::ALL`] is the closed list and the *server* refuses anything
/// outside it with `unknown_environment`. A picker that offers an environment the server will not
/// accept is the same defect as offering a scope the caller cannot delegate: a form that submits
/// and is then refused. Reading the crate is what keeps the two halves from drifting, and
/// `theOfferedEnvironmentsAreTheOnesTheServerAccepts` in `tests/developer.rs` is the gate.

/// Body of `GET /api/v1/developer/scopes`.
#[derive(Debug, Serialize)]
pub struct ScopeCatalogue {
    /// Grouped by category, which is how the picker renders them.
    pub categories: Vec<ScopeCategory>,
    /// The environments a key may carry.
    pub environments: Vec<&'static str>,
}

/// One category of the scope picker.
#[derive(Debug, Serialize)]
pub struct ScopeCategory {
    /// The category key, e.g. `content`.
    pub key: &'static str,
    /// The scopes in it, with a description the picker shows.
    pub scopes: Vec<ScopeRow>,
}

/// One assignable scope.
#[derive(Debug, Serialize)]
pub struct ScopeRow {
    /// The permission key.
    pub key: &'static str,
    /// What it allows, in product language.
    pub description: &'static str,
    /// Whether the caller may grant it — a scope picker that offers something the caller
    /// cannot delegate is a form that submits and is then refused by the server.
    pub grantable: bool,
}

/// `GET /api/v1/developer/scopes` — the assignable catalogue, grouped for the picker.
pub async fn list_scopes(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<ScopeCatalogue>, crate::error::ApiError> {
    let organization_id = crate::scope::resolve_organization(&session, None)?;
    let effective = omnion_permissions::effective_permissions(
        state.db().pool(),
        session.user.id,
        omnion_permissions::Scope::Organization { organization_id },
    )
    .await
    .map_err(|error| {
        crate::error::ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("could not resolve the caller's permissions: {error}"),
        )
    })?;

    // Grouped here rather than in the panel: the grouping is a property of the catalogue, and
    // a panel that re-derives it would drift the day a category is renamed.
    let mut categories: Vec<ScopeCategory> = Vec::new();
    for definition in omnion_permissions::catalogue::CATALOGUE {
        let Some(category) = categories
            .iter_mut()
            .find(|entry: &&mut ScopeCategory| entry.key == definition.category)
        else {
            categories.push(ScopeCategory {
                key: definition.category,
                scopes: Vec::new(),
            });
            continue;
        };
        category.scopes.push(ScopeRow {
            key: definition.key,
            description: definition.description,
            grantable: effective.allows(definition.key),
        });
    }

    Ok(Json(ScopeCatalogue {
        categories,
        environments: omnion_developer::Environment::ALL_STR.to_vec(),
    }))
}

/// `GET /api/v1/developer/sandbox/probe` — the route a key alone may call.
///
/// Answers no tenant data and takes no arguments, so it cannot leak and cannot be enumerated;
/// its whole purpose is to give "a key authenticates on a guarded endpoint" a real route to be
/// true about. The message is the assertion, and the walkthrough reads it back.
pub async fn sandbox_probe() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "message": "the key authenticated and carried the scope this route requires",
    }))
}