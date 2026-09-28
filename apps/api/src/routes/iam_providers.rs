//! `/api/v1/iam/providers` — the sign-in providers of an organization (REQ-006, slice 4b-2;
//! docs/07-IAM.md §11).
//!
//! The management half of enterprise sign-in. The other half — the browser round trip itself —
//! lives in [`crate::routes::sso`], because it is a *public* surface: a callback arrives with no
//! session, so it cannot sit behind a permission guard.
//!
//! Two rules shape everything here:
//!
//! * **No secret ever enters a row or a response.** A provider names where its client secret
//!   lives (`secret_ref`) and the panel can only ever edit that *name*. The list answers with the
//!   wiring plus a boolean saying whether the named variable is defined in this installation, so
//!   an operator can tell "not set up" from "broken" without the value ever being readable.
//! * **Local sign-in stays available in every state.** Nothing on this surface can turn password
//!   sign-in off; a provider is a *choice*, and a disabled one is unreachable rather than gone.
//!
//! The `test` action is the useful one: it asks the provider what it actually is (OIDC discovery
//! and the JWKS, a SAML metadata document) instead of trusting what an operator typed, so a
//! half-configured provider is caught before the first real person tries to sign in with it.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::sso::oidc::{self, Discovery, HttpClient};
use omnion_identity::sso::providers::{
    self, AuthProvider, NewProvider, ProviderChanges, ProviderKind, default_scopes,
};
use omnion_identity::sso::{directory, provisioning};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::iam::record;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// Largest provider event page the list answers.
const MAX_EVENT_PAGE: i64 = 100;

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Query of the provider list and the event log.
#[derive(Debug, Deserialize)]
pub struct ProviderQuery {
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// How many sign-in events to answer with the list.
    #[serde(default)]
    pub event_limit: Option<i64>,
}

/// The body of a provider creation.
#[derive(Debug, Deserialize)]
pub struct NewProviderBody {
    /// URL-safe name the sign-in URL carries (`/auth/sso/okta/start`).
    pub slug: String,
    /// Which protocol: `oidc`, `oauth2` or `saml`.
    pub kind: String,
    /// Label on the sign-in screen.
    pub name: String,
    /// Endpoints and provider-specific settings.
    #[serde(default)]
    pub config: Value,
    /// Name of the environment variable holding the client secret.
    #[serde(default)]
    pub secret_ref: Option<String>,
    /// Scopes to request; the kind's defaults are used when this is empty.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Claim carrying group membership.
    #[serde(default)]
    pub group_claim: Option<String>,
    /// Role every successful sign-in gets.
    #[serde(default)]
    pub default_role_id: Option<Uuid>,
    /// Provision on first sign-in?
    #[serde(default)]
    pub jit_enabled: Option<bool>,
    /// Organization to create in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// How often to sync, in minutes. Omitted keeps the default; `0` means "never on a
    /// schedule", which is a real answer for a directory used only interactively.
    #[serde(default)]
    pub sync_interval_minutes: Option<i32>,
}

/// The body of a provider update; every field is optional and `None` leaves the column alone.
#[derive(Debug, Deserialize)]
pub struct ProviderPatchBody {
    /// New label.
    #[serde(default)]
    pub name: Option<String>,
    /// New endpoints and settings.
    #[serde(default)]
    pub config: Option<Value>,
    /// New secret reference.
    #[serde(default)]
    pub secret_ref: Option<String>,
    /// New scope list.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// New group claim.
    #[serde(default)]
    pub group_claim: Option<String>,
    /// New default role.
    #[serde(default)]
    pub default_role_id: Option<Uuid>,
    /// New JIT flag.
    #[serde(default)]
    pub jit_enabled: Option<bool>,
    /// New reachable flag.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// New sync interval, in minutes.
    #[serde(default)]
    pub sync_interval_minutes: Option<i32>,
}

/// One provider as the panel reads it.
#[derive(Debug, Serialize)]
pub struct ProviderBody {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// URL-safe name.
    pub slug: String,
    /// Protocol.
    pub kind: &'static str,
    /// Label.
    pub name: String,
    /// Endpoints and settings.
    pub config: Value,
    /// The name of the environment variable holding the client secret (never the value).
    pub secret_ref: Option<String>,
    /// Whether that variable is defined in this installation — the field the panel turns into
    /// "secret missing" without ever reading it.
    pub secret_present: bool,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Claim carrying groups.
    pub group_claim: Option<String>,
    /// Default role.
    pub default_role_id: Option<Uuid>,
    /// Provision on first sign-in?
    pub jit_enabled: bool,
    /// Reachable?
    pub enabled: bool,
    /// When a connection test last ran, and whether it passed. `null` on `last_test_ok` means
    /// *never tested* — a third state the panel renders differently from a failure.
    pub last_test_at: Option<String>,
    pub last_test_ok: Option<bool>,
    /// How often this provider syncs, in minutes. Zero means "not on a schedule".
    pub sync_interval_minutes: i32,
    /// When it last synced, and how that went. Both null for a kind that does not sync.
    pub last_sync_at: Option<String>,
    pub last_sync_status: Option<String>,
    /// The plugin declaration that produced this row, when it was not a platform kind.
    pub plugin_key: Option<String>,
    /// The connection status the list's chip column shows, derived rather than stored.
    ///
    /// Four states, and the distinction between the last two is the whole reason this is a
    /// function: `disabled` and `degraded` both look like "not working" but need opposite
    /// responses — one needs a click, the other a fix.
    pub status: &'static str,
    /// When it was created.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// How many sign-ins the event log holds for this provider.
    pub sign_in_count: i64,
    /// When it last signed somebody in.
    pub last_sign_in_at: Option<String>,
}

impl ProviderBody {
    /// Read a row for the panel, pairing it with its log summary.
    fn new(provider: &AuthProvider, sign_in_count: i64, last_sign_in_at: Option<String>) -> Self {
        let secret_present = provider
            .secret_ref
            .as_deref()
            .map(str::trim)
            .filter(|reference| !reference.is_empty())
            .is_some_and(|reference| std::env::var(reference).is_ok());
        Self {
            id: provider.id,
            organization_id: provider.organization_id,
            slug: provider.slug.clone(),
            kind: provider.kind.as_str(),
            name: provider.name.clone(),
            config: provider.config.clone(),
            secret_ref: provider.secret_ref.clone(),
            secret_present,
            scopes: provider.scopes.clone(),
            group_claim: provider.group_claim.clone(),
            default_role_id: provider.default_role_id,
            jit_enabled: provider.jit_enabled,
            enabled: provider.enabled,
            last_test_at: stamp(provider.last_test_at),
            last_test_ok: provider.last_test_ok,
            sync_interval_minutes: provider.sync_interval_minutes,
            last_sync_at: stamp(provider.last_sync_at),
            last_sync_status: provider.last_sync_status.clone(),
            plugin_key: provider.plugin_key.clone(),
            status: connection_status(provider),
            created_at: provider.created_at.format(&Rfc3339).unwrap_or_default(),
            updated_at: provider.updated_at.format(&Rfc3339).unwrap_or_default(),
            sign_in_count,
            last_sign_in_at,
        }
    }
}

/// The four states the registry's status chip can be in.
///
/// The order of the checks *is* the meaning: a disabled provider is disabled whatever its test
/// said, because a person cannot reach it. A `degraded` provider is the interesting one — it is
/// enabled and therefore load-bearing, and something about it is wrong. A registry that reported
/// that as "failed" would send an operator looking for an outage that is not there; one that
/// reported it as "ok" would hide it.
#[must_use]
pub fn connection_status(provider: &AuthProvider) -> &'static str {
    if !provider.enabled {
        return "disabled";
    }
    if provider.last_test_ok == Some(false) {
        return "degraded";
    }
    if provider.last_sync_status.as_deref() == Some("failed") {
        return "degraded";
    }
    "enabled"
}

/// Format a timestamp for the panel, or `None` when there is nothing to show.
fn stamp(value: Option<time::OffsetDateTime>) -> Option<String> {
    value.map(|moment| moment.format(&Rfc3339).unwrap_or_default())
}

/// The answer of the discovery test.
#[derive(Debug, Serialize)]
pub struct ProviderTestBody {
    /// The provider that was asked.
    pub provider_id: Uuid,
    /// Its slug.
    pub slug: String,
    /// Its protocol.
    pub kind: &'static str,
    /// `ok` when the provider answered and every required endpoint was present.
    pub status: &'static str,
    /// What the test proved, in words the panel shows next to the button.
    pub detail: String,
    /// The endpoints the provider actually reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Value>,
    /// Whether the client secret is readable in this installation.
    pub secret_present: bool,
    /// The step ladder, for a directory. Absent for the protocol kinds, which report one
    /// result rather than a walk — the field is skipped rather than sent empty so the panel
    /// cannot render six grey rows for an OIDC provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<directory::StepReport>>,
    /// Configuration problems, each attached to the field that owns it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problems: Option<Vec<directory::ConfigProblem>>,
}

/// The answer of the enable action.
#[derive(Debug, Serialize)]
pub struct ProviderEnabledBody {
    /// The provider that was switched.
    pub id: Uuid,
    /// Its new state.
    pub enabled: bool,
    /// Whether the stored test result satisfied the gate. `false` on a disable, where there is
    /// nothing to satisfy — so the panel does not imply a test was run.
    pub gate_passed: bool,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The organization's providers, each with its sign-in summary.
pub async fn list_providers(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ProviderQuery>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let list = providers::list_providers(state.db().pool(), organization_id).await?;

    // One grouped query rather than one per provider: the list is small but the event log is not,
    // and an N+1 here is a page that scales with the number of providers.
    let mut summaries: std::collections::HashMap<Uuid, (i64, Option<String>)> =
        std::collections::HashMap::new();
    for provider in &list {
        let (count, last) = sqlx::query_as::<_, (i64, Option<time::OffsetDateTime>)>(
            "select count(*), max(created_at) from auth_provider_events \
             where provider_id = $1 and outcome in ('success', 'provisioned')",
        )
        .bind(provider.id)
        .fetch_one(state.db().pool())
        .await
        .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))?;
        summaries.insert(
            provider.id,
            (
                count,
                last.map(|value| value.format(&Rfc3339).unwrap_or_default()),
            ),
        );
    }

    let rows = list
        .iter()
        .map(|provider| {
            let (sign_in_count, last_sign_in_at) =
                summaries.get(&provider.id).cloned().unwrap_or((0, None));
            ProviderBody::new(provider, sign_in_count, last_sign_in_at)
        })
        .collect::<Vec<_>>();

    Ok(Json(json!({
        "organization_id": organization_id,
        "providers": rows,
        "kinds": [
            { "value": "oidc", "label": "OpenID Connect", "default_scopes": default_scopes(ProviderKind::Oidc), "family": "protocol" },
            { "value": "oauth2", "label": "OAuth 2.0", "default_scopes": default_scopes(ProviderKind::Oauth2), "family": "protocol" },
            { "value": "saml", "label": "SAML 2.0", "default_scopes": Vec::<String>::new(), "family": "protocol" },
            // The directory kinds are not a variation on the protocol half: they are a live
            // connection with a service account, a search filter and a test ladder, so the
            // panel groups them separately and never offers them an OAuth scope.
            { "value": "ldap", "label": "LDAP", "default_scopes": Vec::<String>::new(), "family": "directory" },
            { "value": "active_directory", "label": "Active Directory", "default_scopes": Vec::<String>::new(), "family": "directory" },
        ],
    })))
}

/// One provider by id.
pub async fn get_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<ProviderBody>, ApiError> {
    let provider = load(&state, &current, id).await?;
    let (sign_in_count, last) = sign_in_summary(&state, id).await?;
    Ok(Json(ProviderBody::new(
        &provider,
        sign_in_count,
        last.map(|value| value.format(&Rfc3339).unwrap_or_default()),
    )))
}

/// Connect a provider.
pub async fn create_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<NewProviderBody>,
) -> Result<(StatusCode, Json<ProviderBody>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;
    let kind = ProviderKind::parse(&body.kind)?;

    // The claim → role rules live inside `config`, and they are checked at save time: a rule
    // that can never be read is a sign-in that silently loses a role, which is exactly the kind
    // of thing an operator only finds out about after somebody could not get in.
    omnion_identity::sso::claims::mappings_from_config(&body.config).map_err(|error| {
        ApiError::bad_request("invalid_request", error.to_string())
            .with_details(json!({ "field": "config.role_mappings" }))
    })?;
    if let Some(reference) = body.secret_ref.as_deref().map(str::trim)
        && !reference.is_empty()
        && !reference
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(ApiError::bad_request(
            "invalid_request",
            "secret_ref is the name of an environment variable, so it may only hold A–Z, 0–9 and underscores",
        )
        .with_details(json!({ "field": "secret_ref" })));
    }

    let scopes = if body.scopes.is_empty() {
        default_scopes(kind)
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect()
    } else {
        body.scopes
    };

    let created = providers::create_provider(
        state.db().pool(),
        NewProvider {
            organization_id,
            slug: body.slug,
            kind,
            name: body.name,
            config: body.config,
            secret_ref: body.secret_ref,
            scopes,
            group_claim: body.group_claim,
            default_role_id: body.default_role_id,
            // Provisioning is off until it is asked for: a provider that silently creates accounts
            // is a provider that can be used to fill an organization with strangers.
            jit_enabled: body.jit_enabled.unwrap_or(false),
            // A provider is created switched off. The `test` action is how it is proved, and
            // `enabled` is how it is published.
            enabled: false,
            sync_interval_minutes: body.sync_interval_minutes,
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provider_connected")
            .target("auth_provider", created.id.to_string())
            .metadata(json!({
                "slug": created.slug,
                "kind": created.kind.as_str(),
                "organization_id": created.organization_id,
            }))
            .ip_address(address.as_text())
            .organization(Some(organization_id)),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.provider_connected")
            .organization(Some(organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "provider_id": created.id,
                "slug": created.slug,
                "kind": created.kind.as_str(),
            })),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(ProviderBody::new(&created, 0, None)),
    ))
}

/// Change a provider's mutable fields.
pub async fn update_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<ProviderPatchBody>,
) -> Result<Json<ProviderBody>, ApiError> {
    let before = load(&state, &current, id).await?;

    if let Some(config) = body.config.as_ref() {
        omnion_identity::sso::claims::mappings_from_config(config)
            .map_err(|error| ApiError::bad_request("invalid_request", error.to_string()))?;
    }

    // An `enabled: true` arriving through the generic PATCH must not be a way around the gate —
    // otherwise the checkbox on the edit form and the Enable button are two doors, and only one
    // of them is locked. The refusal names the gate rather than looking like a validation error.
    if body.enabled == Some(true) && !before.enabled {
        providers::enable_gate(&before)
            .map_err(|error| ApiError::bad_request("provider_not_ready", error.to_string()))?;
    }

    // Names of the fields actually supplied, so the audit trail says what changed rather than
    // echoing a whole row. Values never appear — an audit log that carries a config is an audit
    // log that eventually carries a secret reference and then a secret.
    let mut changed: Vec<&str> = Vec::new();
    if body.name.is_some() {
        changed.push("name");
    }
    if body.config.is_some() {
        changed.push("config");
    }
    if body.secret_ref.is_some() {
        changed.push("secret_ref");
    }
    if body.scopes.is_some() {
        changed.push("scopes");
    }
    if body.group_claim.is_some() {
        changed.push("group_claim");
    }
    if body.default_role_id.is_some() {
        changed.push("default_role_id");
    }
    if body.jit_enabled.is_some() {
        changed.push("jit_enabled");
    }
    if body.enabled.is_some() {
        changed.push("enabled");
    }
    if body.sync_interval_minutes.is_some() {
        changed.push("sync_interval_minutes");
    }

    // The row is read back rather than taken from the update's `returning` clause, because the
    // invalidation below may have changed it again.
    providers::update_provider(
        state.db().pool(),
        id,
        ProviderChanges {
            name: body.name,
            config: body.config,
            secret_ref: body.secret_ref,
            scopes: body.scopes,
            group_claim: body.group_claim,
            default_role_id: body.default_role_id,
            jit_enabled: body.jit_enabled,
            enabled: body.enabled,
            sync_interval_minutes: body.sync_interval_minutes,
        },
    )
    .await?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            "no such provider",
        )
    })?;

    // A test proves a *configuration*. Repointing the host, the base DN or the secret reference
    // makes the stored answer untrue, and leaving it would let a provider whose connection was
    // just broken keep a green "Last test" and an enabled checkbox. The stored *timestamp* stays:
    // "tested before the last edit" is useful, "tested against what is configured now" is not.
    if providers::edit_invalidates_test(&changed) {
        providers::invalidate_test(state.db().pool(), id).await?;
    }
    let updated = providers::find_provider(state.db().pool(), id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "provider_not_found",
                "no such provider",
            )
        })?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provider_updated")
            .target("auth_provider", id.to_string())
            .metadata(json!({
                "slug": updated.slug,
                "kind": updated.kind.as_str(),
                "enabled": updated.enabled,
                "changed_fields": changed,
            }))
            .ip_address(address.as_text())
            .organization(Some(updated.organization_id)),
    )
    .await?;

    Ok(Json(ProviderBody::new(&updated, 0, None)))
}

/// Remove a provider. Its challenges and event log go with it (the migration cascades them).
pub async fn delete_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let provider = load(&state, &current, id).await?;
    providers::delete_provider(state.db().pool(), id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provider_removed")
            .target("auth_provider", id.to_string())
            .metadata(json!({ "slug": provider.slug, "kind": provider.kind.as_str() }))
            .ip_address(address.as_text())
            .organization(Some(provider.organization_id)),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Ask the provider what it is.
///
/// This is the action that makes the screen honest: a typed-in issuer that is not the issuer the
/// provider publishes, a JWKS that cannot be read, or a SAML certificate nobody can parse are
/// all caught here rather than on the first real sign-in. The answer is deliberately a `200` with
/// a `status` field even when the provider is broken — a failed *test* is a result, not an error
/// the browser has to guess at.
pub async fn test_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<ProviderTestBody>, ApiError> {
    let provider = load(&state, &current, id).await?;

    // A directory is a different kind of question, so it gets a different shape of answer: the
    // configuration ladder plus, once the network half lands, a step walk. The protocol kinds
    // still get one result — a boolean for them is not a loss, because they fail in exactly one
    // place (the provider did not answer, or answered with something unreadable).
    if provider.kind.is_directory() {
        let config = directory::DirectoryConfig::from_value(&provider.config)
            .map_err(|error| ApiError::bad_request("invalid_request", error.to_string()))?;
        let outcome = directory::test_steps(&config);

        // A directory's secret is the *bind* password, named by its own reference in the config
        // rather than by the provider's `secret_ref` — the two are different credentials and
        // conflating them is how a directory ends up authenticating with a client secret.
        let bind_present = std::env::var(config.bind_secret_ref.trim()).is_ok();
        let (detail, endpoints) = (
            directory_detail(&outcome, bind_present),
            Some(json!({
                "host": config.hostname(),
                "port": config.port(),
                "encrypted": config.is_secure(),
                "login_attribute": config.login_attribute(),
                "bind_within_base": config.bind_within_base(),
            })),
        );

        // Recorded on the row either way: a failed test is a result, and the enable gate reads
        // it. Not recording failures is how a provider ends up enabled with a red "Last test".
        providers::record_test(state.db().pool(), provider.id, outcome.passed()).await?;

        emit(
            &state,
            NewEvent::new(if outcome.passed() {
                "iam.provider_test_passed"
            } else {
                "iam.provider_test_failed"
            })
            .organization(Some(provider.organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "provider_id": provider.id,
                "kind": provider.kind.as_str(),
                "step": outcome.failing_step().map(|step| step.as_str()),
            })),
        )
        .await;

        return Ok(Json(ProviderTestBody {
            provider_id: provider.id,
            slug: provider.slug.clone(),
            kind: provider.kind.as_str(),
            status: outcome.status,
            detail,
            endpoints,
            secret_present: bind_present,
            steps: Some(outcome.steps),
            problems: if outcome.problems.is_empty() {
                None
            } else {
                Some(outcome.problems)
            },
        }));
    }

    let client = HttpClient::new();
    let secret_present = provisioning::resolve_client_secret(&provider)
        .map(|value| value.is_some())
        .unwrap_or(false);

    let (status, detail, endpoints) = match provider.kind {
        ProviderKind::Oidc | ProviderKind::Oauth2 => {
            test_oidc(&client, &provider, secret_present).await
        }
        ProviderKind::Saml => test_saml(&provider, secret_present),
        // A directory is handled above, so this arm is unreachable — but `match` on an enum
        // without it would be a compile error the day a sixth kind lands, which is the right
        // moment to be interrupted and not one second later.
        _ => ("failed", "this kind has no connection test yet".to_owned(), None),
    };

    // The protocol kinds record their outcome too, so the registry's column and the gate mean
    // the same thing for every kind rather than only for directories.
    providers::record_test(state.db().pool(), provider.id, status == "ok").await?;

    emit(
        &state,
        NewEvent::new(if status == "ok" {
            "iam.provider_test_passed"
        } else {
            "iam.provider_test_failed"
        })
        .organization(Some(provider.organization_id))
        .actor(Some(current.user.id))
        .payload(json!({
            "provider_id": provider.id,
            "kind": provider.kind.as_str(),
        })),
    )
    .await;

    Ok(Json(ProviderTestBody {
        provider_id: provider.id,
        slug: provider.slug.clone(),
        kind: provider.kind.as_str(),
        status,
        detail,
        endpoints,
        secret_present,
        steps: None,
        problems: None,
    }))
}

/// One sentence for the top of a directory test result.
///
/// The ladder below carries the detail; this is what the panel shows before it, and it has to
/// say the *same* thing the ladder says. `incomplete` gets its own sentence rather than being
/// folded into "failed", because "not tested yet" and "tested and broken" call for different
/// actions and a screen that conflates them sends the operator to the wrong one.
fn directory_detail(outcome: &directory::TestOutcome, bind_present: bool) -> String {
    let mut sentence = match outcome.status {
        "failed" => {
            let count = outcome.problems.len();
            return format!(
                "{} configuration {} to fix before this directory can be tested: {}",
                count,
                if count == 1 { "problem" } else { "problems" },
                outcome
                    .problems
                    .iter()
                    .map(|problem| problem.field)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        "incomplete" => "the configuration is sound; the directory itself has not been reached \
                         yet, so this is not a passing test"
            .to_owned(),
        _ => "every step passed".to_owned(),
    };
    if !bind_present {
        sentence.push_str(" — the bind password is not defined in this installation, so the bind \
                           step cannot succeed until it is");
    }
    sentence
}

/// Switch a provider on.
///
/// Refused until the stored test says it passed. The gate lives in the crate rather than here so
/// it cannot be bypassed by a second route that forgets to ask — and the error says which of the
/// three states it is, because "enable failed" is not an action anybody can take.
pub async fn enable_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    address: ClientAddress,
) -> Result<Json<ProviderEnabledBody>, ApiError> {
    set_enabled(&state, &current, id, true, address).await
}

/// Switch a provider off. Always allowed, gate or no gate: turning something off is never the
/// dangerous direction, and an operator staring at a broken provider must be able to stop it.
pub async fn disable_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    address: ClientAddress,
) -> Result<Json<ProviderEnabledBody>, ApiError> {
    set_enabled(&state, &current, id, false, address).await
}

async fn set_enabled(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
    enabled: bool,
    address: ClientAddress,
) -> Result<Json<ProviderEnabledBody>, ApiError> {
    let provider = load(state, current, id).await?;
    if enabled {
        providers::enable_gate(&provider).map_err(|error| {
            ApiError::bad_request("provider_not_ready", error.to_string())
                .with_details(json!({ "provider_id": provider.id }))
        })?;
    }
    if provider.enabled == enabled {
        // Idempotent on purpose: a double-click on a checkbox is not a state change, and
        // answering it with a second audit row makes the trail claim something happened twice.
        return Ok(Json(ProviderEnabledBody {
            id: provider.id,
            enabled,
            gate_passed: provider.last_test_ok == Some(true),
        }));
    }

    let updated = providers::update_provider(
        state.db().pool(),
        id,
        ProviderChanges {
            enabled: Some(enabled),
            ..ProviderChanges::default()
        },
    )
    .await?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            "no such provider",
        )
    })?;

    record(
        state,
        NewAuditEntry::by_user(current.user.id, "iam.provider_enabled")
            .target("auth_provider", id.to_string())
            .metadata(json!({ "slug": updated.slug, "enabled": true }))
            .ip_address(address.as_text())
            .organization(Some(updated.organization_id)),
    )
    .await?;

    emit(
        state,
        NewEvent::new("iam.provider_enabled").organization(Some(updated.organization_id))
            .actor(Some(current.user.id))
            .payload(json!({ "provider_id": updated.id, "slug": updated.slug })),
    )
    .await;

    Ok(Json(ProviderEnabledBody {
        id: updated.id,
        enabled: true,
        gate_passed: updated.last_test_ok == Some(true),
    }))
}

/// The sign-in event log of one provider, newest first.
pub async fn list_provider_events(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Query(query): Query<ProviderQuery>,
) -> Result<Json<Value>, ApiError> {
    let provider = load(&state, &current, id).await?;
    let limit = query.event_limit.unwrap_or(25).clamp(1, MAX_EVENT_PAGE);

    let rows = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<String>,
            Option<Uuid>,
            String,
            Option<String>,
            Option<String>,
        ),
    >(
        "select outcome, reason, external_subject, user_id, \
                array_to_string(roles_applied, ', '), ip_address::text, created_at::text \
         from auth_provider_events where provider_id = $1 \
         order by created_at desc limit $2",
    )
    .bind(provider.id)
    .bind(limit)
    .fetch_all(state.db().pool())
    .await
    .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))?;

    let events = rows
        .into_iter()
        .map(
            |(outcome, reason, subject, user_id, roles, ip, created_at)| {
                json!({
                    "outcome": outcome,
                    "reason": reason,
                    "external_subject": subject,
                    "user_id": user_id,
                    "roles_applied": roles,
                    "ip_address": ip,
                    "created_at": created_at,
                })
            },
        )
        .collect::<Vec<_>>();

    Ok(Json(
        json!({ "provider_id": provider.id, "events": events }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Read a provider and refuse one outside the caller's organization.
async fn load(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<AuthProvider, ApiError> {
    let provider = providers::find_provider(state.db().pool(), id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "provider_not_found",
                "no such provider",
            )
        })?;
    resolve_organization(current, Some(provider.organization_id))?;
    Ok(provider)
}

/// How many times a provider signed somebody in, and when it last did.
async fn sign_in_summary(
    state: &AppState,
    id: Uuid,
) -> Result<(i64, Option<time::OffsetDateTime>), ApiError> {
    sqlx::query_as::<_, (i64, Option<time::OffsetDateTime>)>(
        "select count(*), max(created_at) from auth_provider_events \
         where provider_id = $1 and outcome in ('success', 'provisioned')",
    )
    .bind(id)
    .fetch_one(state.db().pool())
    .await
    .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))
}

/// The `code` flows: discovery first, then the key set the discovery document points at.
async fn test_oidc(
    client: &HttpClient,
    provider: &AuthProvider,
    secret_present: bool,
) -> (&'static str, String, Option<Value>) {
    // A SAML provider stores its endpoints explicitly; an OIDC one may either name an issuer to
    // discover, or name the endpoints outright for a provider that publishes no discovery
    // document. Both are supported, and which one is in use is visible in the row.
    let issuer = provider
        .config
        .get("issuer")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    let discovery = match issuer.as_deref() {
        Some(issuer) => {
            let url = format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            );
            match client.get_json(&url).await {
                Ok(document) => match Discovery::from_value(&document) {
                    Ok(discovery) => discovery,
                    Err(error) => {
                        return (
                            "failed",
                            format!("the discovery document is incomplete: {error}"),
                            None,
                        );
                    }
                },
                Err(error) => {
                    return (
                        "failed",
                        format!("the discovery document could not be read: {error}"),
                        None,
                    );
                }
            }
        }
        None => {
            let text = |field: &str| {
                provider
                    .config
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            };
            let (Some(issuer), Some(authorization_endpoint), Some(token_endpoint), Some(jwks_uri)) = (
                text("issuer"),
                text("authorization_endpoint"),
                text("token_endpoint"),
                text("jwks_uri"),
            ) else {
                return (
                    "failed",
                    "the provider names no issuer, so nothing can be discovered — set `issuer` \
                     in the configuration to discover the rest"
                        .to_owned(),
                    None,
                );
            };
            Discovery {
                issuer,
                authorization_endpoint,
                token_endpoint,
                jwks_uri,
                userinfo_endpoint: text("userinfo_endpoint"),
            }
        }
    };

    let jwks = match client.get_json(&discovery.jwks_uri).await {
        Ok(document) => document,
        Err(error) => {
            return (
                "failed",
                format!(
                    "the discovery document is fine, but its signing keys are not readable: {error}"
                ),
                Some(endpoints_json(&discovery)),
            );
        }
    };
    let keys = oidc::parse_jwks(&jwks);
    if keys.is_empty() {
        return (
            "failed",
            "the provider publishes no RSA signing key this platform can verify".to_owned(),
            Some(endpoints_json(&discovery)),
        );
    }

    let note = if secret_present {
        String::new()
    } else {
        " (the client secret is not defined in this installation, so a sign-in will be refused \
         until it is)"
            .to_owned()
    };
    (
        "ok",
        format!(
            "discovery answered and {} usable signing key(s) were published{note}",
            keys.len()
        ),
        Some(endpoints_json(&discovery)),
    )
}

/// SAML has no discovery: every endpoint is entered, so the test is a *parse* of what is
/// configured — which is exactly the failure an operator hits first (a certificate copied with
/// its BEGIN line missing, or a base64 blob that lost its wrapping).
fn test_saml(
    provider: &AuthProvider,
    secret_present: bool,
) -> (&'static str, String, Option<Value>) {
    let text = |field: &str| {
        provider
            .config
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(issuer), Some(audience), Some(certificate)) =
        (text("issuer"), text("audience"), text("certificate_pem"))
    else {
        return (
            "failed",
            "a SAML provider needs `issuer`, `audience` and `certificate_pem` in its \
             configuration"
                .to_owned(),
            None,
        );
    };

    let email_attribute = text("email_attribute").unwrap_or_else(|| "email".to_owned());
    let group_attribute = text("group_attribute");
    let display_name_attribute = text("display_name_attribute");

    // A configuration is proved by asking the *real* parser whether the certificate is usable,
    // not by a second implementation of the parse that could drift from it. The earlier version
    // fed a probe response to `verify_response` and inferred the answer from the error text —
    // and because the probe carried a self-closing `<saml:Assertion/>` (which the reader never
    // parses at all), the button reported every certificate as broken and could never succeed.
    if let Err(error) = omnion_identity::sso::saml::certificate_is_readable(&certificate) {
        return (
            "failed",
            format!("the configured certificate could not be read: {error}"),
            None,
        );
    }

    // The attribute names are configuration too, and a typo in one is invisible until a real
    // assertion arrives carrying no address the reader recognises. So the probe runs the whole
    // reader over a document that has *every* attribute the configuration names — which is the
    // last check that needs no signature, and it proves the wiring end to end.
    let probe = omnion_identity::sso::saml::probe_document(
        &issuer,
        &audience,
        &email_attribute,
        group_attribute.as_deref(),
        display_name_attribute.as_deref(),
    );
    let config = omnion_identity::sso::saml::SamlConfig {
        issuer: issuer.clone(),
        audience: audience.clone(),
        certificate_pem: certificate,
        email_attribute,
        group_attribute,
        display_name_attribute,
    };
    if let Err(error) = omnion_identity::sso::saml::probe_claims(&probe, &config) {
        return (
            "failed",
            format!("the assertion reader cannot read this configuration: {error}"),
            None,
        );
    }

    let note = if secret_present {
        String::new()
    } else {
        " (SAML usually needs no client secret — the certificate is the credential)".to_owned()
    };
    (
        "ok",
        format!("the assertion reader accepts this configuration{note}"),
        Some(json!({ "issuer": issuer, "audience": audience })),
    )
}

/// The endpoints the panel shows next to a successful test.
fn endpoints_json(discovery: &Discovery) -> Value {
    json!({
        "issuer": discovery.issuer,
        "authorization_endpoint": discovery.authorization_endpoint,
        "token_endpoint": discovery.token_endpoint,
        "jwks_uri": discovery.jwks_uri,
        "userinfo_endpoint": discovery.userinfo_endpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_kind_is_refused_rather_than_guessed() {
        assert!(ProviderKind::parse("oidc").is_ok());
        assert!(ProviderKind::parse("saml").is_ok());
        assert!(ProviderKind::parse("OAuth2").is_err());
        assert!(ProviderKind::parse("").is_err());
    }

    #[test]
    fn a_provider_without_a_secret_reference_reads_as_absent_rather_than_broken() {
        let provider = AuthProvider {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            slug: "public".into(),
            kind: ProviderKind::Oidc,
            name: "Public client".into(),
            config: json!({}),
            secret_ref: Some("  ".into()),
            scopes: vec!["openid".into()],
            group_claim: None,
            default_role_id: None,
            jit_enabled: false,
            enabled: false,
            last_test_at: None,
            last_test_ok: None,
            sync_interval_minutes: 60,
            last_sync_at: None,
            last_sync_status: None,
            plugin_key: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let body = ProviderBody::new(&provider, 7, None);
        assert!(!body.secret_present);
        assert_eq!(body.sign_in_count, 7);
    }

    #[test]
    fn a_defined_secret_is_reported_present_without_its_value_leaving_the_row() {
        // The body carries the *name* and a boolean; there is no field a secret could ride in.
        let provider = AuthProvider {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            slug: "confidential".into(),
            kind: ProviderKind::Oidc,
            name: "Confidential client".into(),
            config: json!({}),
            secret_ref: Some("OMNION_SSO_TEST_PRESENT".into()),
            scopes: vec![],
            group_claim: None,
            default_role_id: None,
            jit_enabled: true,
            enabled: true,
            last_test_at: None,
            last_test_ok: None,
            sync_interval_minutes: 60,
            last_sync_at: None,
            last_sync_status: None,
            plugin_key: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let body = ProviderBody::new(&provider, 0, None);
        assert_eq!(body.secret_ref.as_deref(), Some("OMNION_SSO_TEST_PRESENT"));
        assert!(
            !body.secret_present,
            "the variable is not set in this process"
        );
        let encoded = serde_json::to_string(&body).expect("the body serializes");
        assert!(!encoded.contains("client_secret"));
    }

    /// A provider row with the three test states, because the gate is the whole feature and it
    /// has to be provable without a database.
    fn provider_with(last_test_ok: Option<bool>, enabled: bool) -> AuthProvider {
        AuthProvider {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            slug: "dir".into(),
            kind: ProviderKind::Ldap,
            name: "Directory".into(),
            config: json!({}),
            secret_ref: None,
            scopes: vec![],
            group_claim: None,
            default_role_id: None,
            jit_enabled: false,
            enabled,
            last_test_at: None,
            last_test_ok,
            sync_interval_minutes: 60,
            last_sync_at: None,
            last_sync_status: None,
            plugin_key: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// The requirement is "a provider cannot be enabled while its last test has never passed".
    /// "Never" is the case that gets skipped, because `Some(false)` is the one that looks like a
    /// bug report.
    #[test]
    fn an_untested_provider_cannot_be_switched_on() {
        assert!(
            providers::enable_gate(&provider_with(None, false)).is_err(),
            "never tested is not a passing test"
        );
        assert!(
            providers::enable_gate(&provider_with(Some(false), false)).is_err(),
            "tested and failed is not a passing test"
        );
        assert!(
            providers::enable_gate(&provider_with(Some(true), false)).is_ok(),
            "a passing test is the whole requirement"
        );
    }

    /// Turning something *off* is never gated. An operator staring at a broken provider has to
    /// be able to stop it without first making it pass anything.
    #[test]
    fn an_enabled_provider_is_never_re_refused_by_the_gate() {
        assert!(
            providers::enable_gate(&provider_with(None, true)).is_ok(),
            "already-enabled is a state the gate does not second-guess"
        );
    }

    /// The three states must not collapse. A registry that rendered `null` and `false` the same
    /// way would tell an operator a brand-new provider is broken.
    #[test]
    fn the_status_chip_distinguishes_disabled_from_degraded() {
        assert_eq!(connection_status(&provider_with(None, false)), "disabled");
        assert_eq!(connection_status(&provider_with(Some(false), false)), "disabled");
        // Enabled and failing is the state worth seeing: it is load-bearing and broken.
        assert_eq!(connection_status(&provider_with(Some(false), true)), "degraded");
        assert_eq!(connection_status(&provider_with(Some(true), true)), "enabled");
        // Never tested but enabled can only happen through a hand-edited row; it is not
        // "degraded" because nothing is known to be wrong.
        assert_eq!(connection_status(&provider_with(None, true)), "enabled");

        let mut failed_sync = provider_with(Some(true), true);
        failed_sync.last_sync_status = Some("failed".into());
        assert_eq!(
            connection_status(&failed_sync),
            "degraded",
            "a failed sync is also a reason an operator has to look at this"
        );
    }

    /// A test proves a configuration, not a provider id. Repointing the host has to drop it.
    #[test]
    fn an_edit_that_moves_the_connection_drops_the_stored_test() {
        for field in ["config", "secret_ref", "kind"] {
            assert!(
                providers::edit_invalidates_test(&[field]),
                "`{field}` changes what a test would prove"
            );
        }
        for field in ["name", "default_role_id", "jit_enabled", "sync_interval_minutes"] {
            assert!(
                !providers::edit_invalidates_test(&[field]),
                "renaming a provider does not make its last test untrue: `{field}`"
            );
        }
        assert!(
            !providers::edit_invalidates_test(&[]),
            "a PATCH with no fields is not a change"
        );
    }

    /// The sentence above the ladder has to agree with the ladder underneath it.
    #[test]
    fn the_summary_sentence_agrees_with_the_status() {
        let broken = directory::TestOutcome {
            status: "failed",
            steps: vec![],
            problems: vec![omnion_identity::sso::directory::ConfigProblem {
                field: "host",
                message: "required".into(),
                kind: omnion_identity::sso::directory::Problem::Missing,
            }],
            reached_server: None,
        };
        let sentence = directory_detail(&broken, true);
        assert!(sentence.contains("1 configuration problem"), "{sentence}");
        assert!(sentence.contains("host"), "it names the field: {sentence}");

        let untested = directory::TestOutcome {
            status: "incomplete",
            steps: vec![],
            problems: vec![],
            reached_server: None,
        };
        assert!(
            directory_detail(&untested, true).contains("not a passing test"),
            "an untested directory must not read like a broken one"
        );
        assert!(
            directory_detail(&untested, false).contains("bind password"),
            "a missing bind secret is the operator's next action, so it belongs in the sentence"
        );
    }

    /// A directory has no OAuth scopes. Offering them would write values into a column the
    /// sign-in path cannot act on.
    #[test]
    fn a_directory_kind_is_told_apart_from_a_protocol_one() {
        assert!(ProviderKind::Ldap.is_directory());
        assert!(ProviderKind::ActiveDirectory.is_directory());
        assert!(!ProviderKind::Oidc.is_directory());
        assert!(!ProviderKind::Ldap.uses_scopes(), "a directory requests no scopes");
        assert!(ProviderKind::Oidc.uses_scopes());
        // A directory has no authorization-code flow, and naming one anyway would be a lie the
        // challenge table cannot even store.
        assert_eq!(omnion_identity::sso::oidc::flow_of(ProviderKind::Ldap), "directory");
        assert!(default_scopes(ProviderKind::ActiveDirectory).is_empty());
    }

    #[test]
    fn the_event_page_is_clamped_to_a_sane_window() {
        for (asked, expected) in [(0, 1), (5, 5), (10_000, MAX_EVENT_PAGE)] {
            assert_eq!(asked.clamp(1, MAX_EVENT_PAGE), expected);
        }
    }
}
