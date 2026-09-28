//! `/api/v1/secret-leases` and `/api/v1/deployment-keys`
//! (docs/requests/REQ-125, slice 3).
//!
//! The surface has exactly one job, and everything here is arranged around it: **a plaintext
//! must be able to reach a workload without ever being able to reach a browser.**
//!
//! * `POST /secrets/{id}/lease` issues an opaque, use-capped, short-lived handle and returns
//!   *that handle*. The value is never in scope in the issuing handler, so there is no path by
//!   which a bug in this file leaks one.
//! * `POST /secret-leases/{id}/redeem` is the one handler with a value field, and it is guarded
//!   by **two** independent things: a machine identity (a deployment key, presented as a bearer
//!   value) and the lease's own TTL and use cap. A redemption is refused with `410` and writes a
//!   denial row when the lease is revoked, spent or expired — a revoked lease redeemed again is
//!   an event, not an error page.
//! * `POST /deployment-keys` mints a scoped, expiring machine credential. The value is shown
//!   once; the row keeps a hash. A key may lease inside its own environment and its own scope
//!   list and is refused (`403`) outside either — including on anything that would read a value
//!   directly, which is why there is no `reveal` route for a machine identity at all.
//!
//! Two permission names come straight from the catalogue (`secrets.lease`,
//! `secrets.deploykeys.read` / `.manage`): an operator leases, an operator reads the key list,
//! and a machine identity is authenticated by a bearer value rather than by a session — so it
//! never passes through the permission guard and never appears in a role.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_secrets::leases::{self, DeploymentKeyRow, LeaseRow};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::resolve_organization;
use crate::state::AppState;

use super::secrets::map_error as map_crypto_error;

/// The header a machine identity presents its deployment key in.
const DEPLOYMENT_KEY_HEADER: &str = "x-omnion-deployment-key";

/// One lease, in the panel's shape. **No token, no value** — the token left the issuing
/// response and the value never enters this struct.
#[derive(Debug, Serialize)]
pub struct LeaseView {
    /// Lease id — the address a revoke names.
    pub id: Uuid,
    /// The secret the lease is over.
    pub secret_id: Uuid,
    /// The secret's name.
    pub name: String,
    /// Who the lease was issued to.
    pub consumer: String,
    /// The environment whose deploy revokes it.
    pub environment: String,
    /// `live`, `spent`, `expired` or `revoked` — the status chip.
    pub state: String,
    /// The redemption budget.
    pub max_uses: i32,
    /// How much of it is spent.
    pub uses: i32,
    /// Whole seconds until expiry; the countdown's input.
    pub expires_in_seconds: i64,
    /// When it stops working.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: time::OffsetDateTime,
    /// When it was issued.
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: time::OffsetDateTime,
    /// When it was revoked, if it was.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<time::OffsetDateTime>,
    /// Why it was revoked — including the automatic reason after a deploy.
    pub revoke_reason: Option<String>,
    /// The last redemption, so the list can show a lease that has actually been used.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_redeemed_at: Option<time::OffsetDateTime>,
    /// The address of the last redemption, so a leaked lease is traceable.
    pub last_address: Option<String>,
    /// The deployment key bound to it, when it was bound to one.
    pub deployment_key_id: Option<Uuid>,
    /// The current version of the secret, so the panel can say what it would hand out.
    pub version: i32,
}

impl LeaseView {
    fn from_row(row: LeaseRow) -> Self {
        let now = time::OffsetDateTime::now_utc();
        Self {
            expires_in_seconds: row.seconds_left(now),
            state: row.state(now).to_owned(),
            deployment_key_id: row.issued_to_key_id,
            id: row.id,
            secret_id: row.secret_id,
            name: row.name,
            consumer: row.consumer,
            environment: row.environment,
            max_uses: row.max_uses,
            uses: row.uses,
            expires_at: row.expires_at,
            issued_at: row.issued_at,
            revoked_at: row.revoked_at,
            revoke_reason: row.revoke_reason,
            last_redeemed_at: row.last_redeemed_at,
            last_address: row.last_address,
            version: row.version,
        }
    }
}

/// The leases screen in one read.
#[derive(Debug, Serialize)]
pub struct LeasesResponse {
    /// Live leases first, then recent revoked ones.
    pub leases: Vec<LeaseView>,
    /// Counters for the header strip.
    pub total: i32,
    pub live: i32,
    pub spent: i32,
    pub revoked: i32,
    /// The environments the filter offers, taken from the rows rather than a second list.
    pub environments: Vec<String>,
}

/// What `POST /secrets/{id}/lease` answers. The token is here, and only here.
#[derive(Debug, Serialize)]
pub struct IssuedLeaseResponse {
    /// The lease, as the list renders it.
    #[serde(flatten)]
    pub lease: LeaseView,
    /// The opaque handle the helper redeems. **Returned once, never stored.**
    pub token: String,
    /// The default lifetime, so the helper knows what to expect without a second call.
    pub expires_in_seconds: i64,
}

/// The body of a lease request.
#[derive(Debug, serde::Deserialize)]
pub struct IssueLeaseInput {
    /// Who the lease is for: a pipeline name, a workload, anything the audit will read back.
    pub consumer: String,
    /// The environment whose deploy revokes this lease. Defaults to `default`.
    #[serde(default)]
    pub environment: Option<String>,
    /// Requested lifetime in seconds, clamped into the documented window.
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
    /// Requested redemption budget, clamped into the documented window.
    #[serde(default)]
    pub max_uses: Option<i32>,
}

/// One deployment key, in the panel's shape. Metadata only — there is no field that could
/// hold the key, because the value is shown once in the create drawer and never again.
#[derive(Debug, Serialize)]
pub struct DeploymentKeyView {
    /// Row id.
    pub id: Uuid,
    /// The name an operator gave it.
    pub name: String,
    /// The environment it is bound to.
    pub environment: String,
    /// The scope list, as the table renders it.
    pub scopes: Vec<String>,
    /// `active`, `revoked` or `expired`.
    pub state: String,
    /// When it stops working.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: time::OffsetDateTime,
    /// Whole seconds until expiry, so the panel can sort by urgency without date math.
    pub expires_in_seconds: i64,
    /// How many times it was presented.
    pub uses: i64,
    /// The last presentation.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<time::OffsetDateTime>,
    /// The optional address allow-list, as the table renders it.
    pub allowed_ips: Vec<String>,
    /// A short recognisable prefix for a CI variable.
    pub key_prefix: String,
    /// The operator-comparable fingerprint.
    pub fingerprint: String,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// When it was revoked, if it was.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<time::OffsetDateTime>,
    /// Why it was revoked.
    pub revoke_reason: Option<String>,
    /// `true` when a revoked key's record can be deleted.
    pub deletable: bool,
}

impl DeploymentKeyView {
    fn from_row(row: DeploymentKeyRow) -> Self {
        let now = time::OffsetDateTime::now_utc();
        Self {
            state: row.state(now).to_owned(),
            expires_in_seconds: (row.expires_at - now).whole_seconds().max(0),
            scopes: row.scope_list().into_iter().map(str::to_owned).collect(),
            allowed_ips: row
                .allowed_ips
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
                .collect(),
            deletable: row.revoked_at.is_some() || row.expires_at <= now,
            id: row.id,
            name: row.name,
            environment: row.environment,
            uses: row.uses,
            expires_at: row.expires_at,
            last_used_at: row.last_used_at,
            key_prefix: row.key_prefix,
            fingerprint: row.key_fingerprint,
            created_at: row.created_at,
            revoked_at: row.revoked_at,
            revoke_reason: row.revoke_reason,
        }
    }
}

/// The deployment keys screen in one read.
#[derive(Debug, Serialize)]
pub struct DeploymentKeysResponse {
    /// The keys.
    pub keys: Vec<DeploymentKeyView>,
    /// Counters for the header strip.
    pub total: i32,
    pub active: i32,
    pub expired: i32,
    pub revoked: i32,
    /// The header the helper presents, so the create drawer can name it exactly.
    pub header: String,
    /// What the panel says about the risk, in the drawer's own words.
    pub guidance: String,
}

/// What `POST /deployment-keys` answers. The value is here, and only here.
#[derive(Debug, Serialize)]
pub struct CreatedDeploymentKeyResponse {
    /// The key, as the list renders it.
    #[serde(flatten)]
    pub key: DeploymentKeyView,
    /// The key value. **Shown once.** It is not in any other response, ever.
    pub value: String,
    /// The header to present it in.
    pub header: String,
}

/// The body of a create request.
#[derive(Debug, serde::Deserialize)]
pub struct CreateDeploymentKeyInput {
    /// The name an operator gives it.
    pub name: String,
    /// The environment it is bound to.
    pub environment: String,
    /// The scopes it may lease inside. A `family.*` entry covers a family.
    pub scopes: Vec<String>,
    /// The expiry, as an RFC 3339 timestamp. Required: a machine credential that never
    /// expires is a liability, not a convenience.
    ///
    /// The attribute is not decoration. Without it `time::OffsetDateTime` deserializes from a
    /// *tuple* — `(year, ordinal, hour, …)` — so every real client, which sends what
    /// `Date.prototype.toISOString()` produces, is answered `422 expires_at: invalid type:
    /// string`. The panel sends an ISO string, which means the create drawer was a dead button
    /// for every operator: the one field the request marks required was the one field the API
    /// could not read. `time::serde::rfc3339` is on the *output* types already; this brings the
    /// input into the same agreement.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: time::OffsetDateTime,
    /// Optional comma-separated address allow-list (`203.0.113.7`, `10.0.0.0/8`).
    #[serde(default)]
    pub allowed_ips: Option<String>,
}

/// One use of a deployment key, as the use log renders it.
#[derive(Debug, Serialize)]
pub struct DeploymentKeyUseView {
    /// `lease`, `denied` or `revoke`.
    pub action: String,
    /// The lease it touched, when there was one.
    pub lease_id: Option<Uuid>,
    /// The pipeline identity, as presented.
    pub identity: String,
    /// The source address.
    pub address: Option<String>,
    /// `ok` or the refusal code.
    pub result: String,
    /// When it happened.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The body of a revoke request.
#[derive(Debug, serde::Deserialize)]
pub struct RevokeInput {
    /// Why, in the operator's words. Empty means "revoked from the panel".
    #[serde(default)]
    pub reason: Option<String>,
}

/// The redemption response. The **only** struct in this request with a value field.
#[derive(Debug, Serialize)]
pub struct RedemptionResponse {
    /// The value. Loopback-scoped, machine-identity bound, use-capped, audited, never cached.
    pub value: String,
    /// The version it came from.
    pub version: i32,
    /// The secret's name, so the helper can log which credential it just handed out.
    pub name: String,
    /// The redaction hint — an operator can prove *which* value was handed out without it
    /// ever being readable.
    pub hint: String,
    /// The request id, for the audit trail.
    pub request_id: Uuid,
}

/// `GET /api/v1/secret-leases` — the leases screen.
pub async fn read_leases(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<LeasesResponse>, ApiError> {
    let rows = leases::list_leases(state.db().pool(), None, None)
        .await
        .map_err(map_error)?;
    let views: Vec<LeaseView> = rows.into_iter().map(LeaseView::from_row).collect();

    let count_of =
        |state_name: &str| views.iter().filter(|view| view.state == state_name).count() as i32;
    let mut environments: Vec<String> = views
        .iter()
        .map(|view| view.environment.clone())
        .filter(|name| name != "default")
        .collect();
    environments.sort();
    environments.dedup();

    Ok(Json(LeasesResponse {
        total: views.len() as i32,
        live: count_of("live"),
        spent: count_of("spent"),
        revoked: count_of("revoked"),
        environments,
        leases: views,
    }))
}

/// `POST /api/v1/secrets/{id}/lease` — issue a lease. **Never returns the value.**
pub async fn issue_lease(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(input): Json<IssueLeaseInput>,
) -> Result<(StatusCode, Json<IssuedLeaseResponse>), ApiError> {
    let organization_id = resolve_organization(&session, None)?;
    let pool = state.db().pool();
    // The secret has to exist *and* belong to the caller's organization, before a row is
    // written: a lease is a handle on someone else's secret otherwise.
    let owner = omnion_secrets::credentials::find_secret_owner(pool, id)
        .await
        .map_err(map_error)?;
    in_organization(&owner.organization_id, Some(organization_id))?;

    let issued = leases::issue_lease(
        pool,
        id,
        &input.consumer,
        input.environment.as_deref().unwrap_or("default"),
        input.ttl_seconds,
        input.max_uses,
        None,
    )
    .await
    .map_err(map_error)?;

    let request_id: Uuid = Uuid::new_v4();
    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "secret.lease.issued")
            .target("secret", id.to_string())
            .request_id(request_id)
            .lease_id(issued.lease.id)
            .metadata(json!({
                "name": owner.name,
                "consumer": issued.lease.consumer,
                "environment": issued.lease.environment,
                "max_uses": issued.lease.max_uses,
                "expires_at": issued.lease.expires_at,
            }))
            .ip_address(address.as_text()),
    )
    .await;
    emit(
        &state,
        NewEvent::new("secrets.lease_issued")
            .actor(session.user.id)
            .payload(json!({
                "lease_id": issued.lease.id,
                "secret_id": id,
                "name": owner.name,
                "consumer": issued.lease.consumer,
                "environment": issued.lease.environment,
                "max_uses": issued.lease.max_uses,
            })),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(IssuedLeaseResponse {
            expires_in_seconds: issued.lease.seconds_left(time::OffsetDateTime::now_utc()),
            lease: LeaseView::from_row(issued.lease),
            token: issued.token,
        }),
    ))
}

/// `POST /api/v1/secret-leases/{id}/revoke` — revoke with a reason.
pub async fn revoke_lease(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(input): Json<RevokeInput>,
) -> Result<Json<LeaseView>, ApiError> {
    let pool = state.db().pool();
    let existing = leases::find_lease(pool, id)
        .await
        .map_err(map_error)?
        .ok_or_else(|| not_found("lease"))?;
    let organization_id = resolve_organization(&session, None)?;
    in_organization(
        &sqlx::query_scalar::<_, Option<Uuid>>("select organization_id from secrets where id = $1")
            .bind(existing.secret_id)
            .fetch_one(pool)
            .await
            .map_err(|error| ApiError::from_core(error.into()))?,
        Some(organization_id),
    )?;

    let revoked = leases::revoke_lease(
        pool,
        id,
        input.reason.as_deref().unwrap_or("revoked from the panel"),
    )
    .await
    .map_err(map_error)?;

    let request_id: Uuid = Uuid::new_v4();
    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "secret.lease.revoked")
            .request_id(request_id)
            // The column, not only `target_id`: the audit screen's lease filter and this suite's
            // join both read `lease_id`, and a row that names the lease in one place and not the
            // other is a row that only some queries can find.
            .lease_id(id)
            .target("lease", id.to_string())
            .metadata(json!({
                "secret_id": existing.secret_id,
                "name": existing.name,
                "consumer": existing.consumer,
                "reason": revoked.revoke_reason,
            }))
            .ip_address(address.as_text()),
    )
    .await;
    emit(
        &state,
        NewEvent::new("secrets.lease_revoked")
            .actor(session.user.id)
            .payload(json!({
                "lease_id": id,
                "secret_id": existing.secret_id,
                "name": existing.name,
                "consumer": existing.consumer,
                "reason": revoked.revoke_reason,
            })),
    )
    .await;

    Ok(Json(LeaseView::from_row(revoked)))
}

/// `POST /api/v1/secret-leases/{id}/redeem` — the one path a value takes.
///
/// Guarded by two independent things, and the order is the point: a machine identity is
/// authenticated **first**, so an anonymous caller can never reach the lease's own state and
/// learn from the difference between the statuses.
pub async fn redeem_lease(
    State(state): State<AppState>,
    headers: HeaderMap,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(input): Json<RedeemInput>,
) -> Result<Json<RedemptionResponse>, ApiError> {
    let pool = state.db().pool();
    let request_id = Uuid::new_v4();
    let address_text = address.as_text();

    // 1. Who is this? A deployment key, and only a deployment key.
    let key = match presented_key(&headers) {
        Some(value) => {
            match leases::authenticate_deployment_key(pool, &value, address_text.as_deref()).await {
                Ok(key) => key,
                Err(error) => {
                    return Err(ApiError::new(
                        StatusCode::UNAUTHORIZED,
                        error.code(),
                        error.to_string(),
                    )
                    .with_details(json!({ "request_id": request_id })));
                }
            }
        }
        None => {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "deployment_key_required",
                "a lease is redeemed by a deployment key, not by a session; present the key in \
                 the X-Omnion-Deployment-Key header from the machine that runs the workload",
            )
            .with_details(json!({ "request_id": request_id, "header": DEPLOYMENT_KEY_HEADER })));
        }
    };

    // 2. May this key act on this lease? Environment and scope, checked before the token so a
    //    key in the wrong environment never reaches the lease's own budget.
    let lease = leases::find_lease(pool, id).await.map_err(map_error)?;
    let Some(lease) = lease else {
        // A key that names a lease that does not exist is still logged: probing leaves a trace.
        let _ = leases::record_use(
            pool,
            key.id,
            "denied",
            None,
            &key.name,
            address_text.as_deref(),
            "lease_not_found",
        )
        .await;
        return Err(
            ApiError::new(StatusCode::NOT_FOUND, "lease_not_found", "no such lease")
                .with_details(json!({ "request_id": request_id })),
        );
    };
    let secret_name = leases::secret_name_for(pool, lease.secret_id)
        .await
        .map_err(map_error)?;
    if let Err(error) = leases::check_key_may_touch(&key, &secret_name, &lease.environment) {
        let _ = leases::record_use(
            pool,
            key.id,
            "denied",
            Some(lease.id),
            &key.name,
            address_text.as_deref(),
            error.code(),
        )
        .await;
        audit(
            &state,
            NewAuditEntry::system("secret.access.denied")
                .target("lease", lease.id.to_string())
                // The request id goes in the COLUMN, not only in the metadata object: the audit
                // screen filters on `audit_log.request_id`, so an id that only ever lived inside a
                // JSON blob would be readable but unjoinable -- which is the whole point of it.
                .request_id(request_id)
                .lease_id(lease.id)
                .machine(Some(key.id), None, Some(lease.id))
                .metadata(json!({
                    "reason": error.code(),
                    "deployment_key_id": key.id,
                    "environment": lease.environment,
                    "name": secret_name,
                }))
                .ip_address(address_text.clone()),
        )
        .await;
        return Err(
            ApiError::new(StatusCode::FORBIDDEN, error.code(), error.to_string())
                .with_details(json!({ "request_id": request_id })),
        );
    }

    // 3. Redeem. Every refusal from here is a `410`-shaped answer plus a denial row.
    match leases::redeem_lease(pool, lease.id, &input.token, address_text.as_deref()).await {
        Ok(redemption) => {
            let _ = leases::record_use(
                pool,
                key.id,
                "lease",
                Some(lease.id),
                &key.name,
                address_text.as_deref(),
                "ok",
            )
            .await;
            audit(
                &state,
                NewAuditEntry::system("secret.lease.redeemed")
                    .target("secret", lease.secret_id.to_string())
                    .request_id(request_id)
                    .machine(Some(key.id), None, Some(lease.id))
                    .metadata(json!({
                        "name": redemption.name,
                        "version": redemption.version,
                        "hint": redemption.hint,
                        "consumer": lease.consumer,
                    }))
                    .ip_address(address_text.clone()),
            )
            .await;
            emit(
                &state,
                NewEvent::new("secrets.lease_redeemed").payload(json!({
                    "lease_id": lease.id,
                    "secret_id": lease.secret_id,
                    "name": redemption.name,
                    "version": redemption.version,
                    "deployment_key_id": key.id,
                    "consumer": lease.consumer,
                })),
            )
            .await;

            Ok(Json(RedemptionResponse {
                value: redemption.value,
                version: redemption.version,
                name: redemption.name,
                hint: redemption.hint,
                request_id,
            }))
        }
        Err(error) => {
            let _ = leases::record_use(
                pool,
                key.id,
                "denied",
                Some(lease.id),
                &key.name,
                address_text.as_deref(),
                error.code(),
            )
            .await;
            audit(
                &state,
                NewAuditEntry::system("secret.access.denied")
                    .target("lease", lease.id.to_string())
                    .metadata(json!({
                        "request_id": request_id,
                        "reason": error.code(),
                        "deployment_key_id": key.id,
                        "name": secret_name,
                        "message": error.to_string(),
                    }))
                    .ip_address(address_text.clone()),
            )
            .await;
            Err(
                ApiError::new(StatusCode::GONE, error.code(), error.to_string())
                    .with_details(json!({ "request_id": request_id })),
            )
        }
    }
}

/// The body of a redemption: the lease token, as the helper holds it.
#[derive(Debug, serde::Deserialize)]
pub struct RedeemInput {
    /// The opaque handle `POST /secrets/{id}/lease` returned.
    pub token: String,
}

/// `GET /api/v1/deployment-keys` — the keys screen. Metadata only.
pub async fn read_deployment_keys(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<DeploymentKeysResponse>, ApiError> {
    let rows = leases::list_deployment_keys(state.db().pool())
        .await
        .map_err(map_error)?;
    let views: Vec<DeploymentKeyView> = rows.into_iter().map(DeploymentKeyView::from_row).collect();
    let count_of =
        |state_name: &str| views.iter().filter(|view| view.state == state_name).count() as i32;

    Ok(Json(DeploymentKeysResponse {
        total: views.len() as i32,
        active: count_of("active"),
        expired: count_of("expired"),
        revoked: count_of("revoked"),
        keys: views,
        header: DEPLOYMENT_KEY_HEADER.to_owned(),
        guidance: "A deployment key is a machine credential in CI, so it will leak eventually. \
                   Keep the scopes narrow, set an expiry, and revoke the moment a pipeline \
                   changes."
            .to_owned(),
    }))
}

/// `POST /api/v1/deployment-keys` — mint a key. The value is shown once.
pub async fn create_deployment_key(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
    Json(input): Json<CreateDeploymentKeyInput>,
) -> Result<(StatusCode, Json<CreatedDeploymentKeyResponse>), ApiError> {
    let issued = leases::create_deployment_key(
        state.db().pool(),
        &input.name,
        &input.environment,
        &input.scopes,
        input.expires_at,
        input.allowed_ips.as_deref().unwrap_or(""),
    )
    .await
    .map_err(map_error)?;

    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "deployment_key.created")
            .target("deployment_key", issued.key.id.to_string())
            .metadata(json!({
                "name": issued.key.name,
                "environment": issued.key.environment,
                "scopes": issued.key.scopes,
                "expires_at": issued.key.expires_at,
                "fingerprint": issued.key.key_fingerprint,
            }))
            .ip_address(address.as_text()),
    )
    .await;
    emit(
        &state,
        NewEvent::new("secrets.deployment_key_created")
            .actor(session.user.id)
            .payload(json!({
                "deployment_key_id": issued.key.id,
                "name": issued.key.name,
                "environment": issued.key.environment,
                "scopes": issued.key.scopes,
                "fingerprint": issued.key.key_fingerprint,
            })),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(CreatedDeploymentKeyResponse {
            key: DeploymentKeyView::from_row(issued.key),
            value: issued.value,
            header: DEPLOYMENT_KEY_HEADER.to_owned(),
        }),
    ))
}

/// `POST /api/v1/deployment-keys/{id}/revoke` — revoke immediately, and with it the leases
/// it minted. Idempotent, because a deploy arriving after a manual revoke is normal.
pub async fn revoke_deployment_key(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(input): Json<RevokeInput>,
) -> Result<StatusCode, ApiError> {
    let pool = state.db().pool();
    let existing = leases::find_deployment_key(pool, id)
        .await
        .map_err(map_error)?
        .ok_or_else(|| not_found("deployment key"))?;
    let reason = input
        .reason
        .as_deref()
        .unwrap_or("revoked from the panel")
        .to_owned();
    leases::revoke_deployment_key(pool, id, &reason)
        .await
        .map_err(map_error)?;

    // The revocation use row is what a leaked-key investigation reads first.
    let _ = leases::record_use(
        pool,
        id,
        "revoke",
        None,
        &existing.name,
        address.as_text().as_deref(),
        &reason,
    )
    .await;

    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "deployment_key.revoked")
            .target("deployment_key", id.to_string())
            .metadata(json!({ "name": existing.name, "reason": reason }))
            .ip_address(address.as_text()),
    )
    .await;
    emit(
        &state,
        NewEvent::new("secrets.deployment_key_revoked")
            .actor(session.user.id)
            .payload(json!({ "deployment_key_id": id, "name": existing.name, "reason": reason })),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/deployment-keys/{id}` — delete a revoked key's record. A live key can only
/// be revoked; keeping the row visible is what makes "this was once valid" answerable later.
pub async fn delete_deployment_key(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let existing = leases::find_deployment_key(state.db().pool(), id)
        .await
        .map_err(map_error)?
        .ok_or_else(|| not_found("deployment key"))?;
    leases::delete_deployment_key(state.db().pool(), id)
        .await
        .map_err(map_error)?;

    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "deployment_key.deleted")
            .target("deployment_key", id.to_string())
            .metadata(json!({ "name": existing.name, "environment": existing.environment }))
            .ip_address(address.as_text()),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/deployment-keys/{id}/uses` — the use log. Metadata only, like everything else.
pub async fn read_deployment_key_uses(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<DeploymentKeyUseView>>, ApiError> {
    // The key has to exist, or the panel would render an empty log for a typo.
    leases::find_deployment_key(state.db().pool(), id)
        .await
        .map_err(map_error)?
        .ok_or_else(|| not_found("deployment key"))?;
    let rows = leases::list_key_uses(state.db().pool(), id)
        .await
        .map_err(map_error)?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(action, lease_id, identity, address, result, created_at)| DeploymentKeyUseView {
                    action,
                    lease_id,
                    identity,
                    address,
                    result,
                    created_at,
                },
            )
            .collect(),
    ))
}

/// The deployment key a request presented, if any.
///
/// Bearer form first, then the bare header: a CI job has a header it can set and no way to
/// rename an `Authorization` that another layer may already be using, so both are accepted
/// and the machine identity is still required either way.
fn presented_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(DEPLOYMENT_KEY_HEADER) {
        if let Ok(text) = value.to_str() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    let authorization = headers.get(axum::http::header::AUTHORIZATION)?;
    let text = authorization.to_str().ok()?;
    let value = text.strip_prefix("Bearer ").unwrap_or(text).trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Turn a crate failure into the HTTP status the request names for it.
///
/// `410` on a spent lease is the status that carries the meaning "this used to work and no
/// longer does", which is exactly what a CI job hitting a revoked lease needs to tell apart
/// from a typo in its own configuration. `403` on a scope or environment escalation is the
/// other half: the key is real, it is just not allowed to do that.
fn map_error(error: omnion_secrets::SecretsError) -> ApiError {
    let status = match &error {
        omnion_secrets::SecretsError::LeaseUnavailable(_) => StatusCode::GONE,
        omnion_secrets::SecretsError::DeploymentKeyUnavailable(_) => StatusCode::UNAUTHORIZED,
        omnion_secrets::SecretsError::ReadOnly => StatusCode::METHOD_NOT_ALLOWED,
        // Everything else is the shared mapping from slice 1 (`404` on a missing row, `503` on
        // a missing operator key, `409` on a live rotation), so the two surfaces answer the
        // same way about the same crate failure.
        _ => return map_crypto_error(error),
    };
    ApiError::new(status, error.code(), error.to_string())
}

/// Refuse a secret that belongs to another organization. Copied from the credential routes
/// rather than re-exported, so the two files can diverge in the sentence an operator reads
/// without the lease surface having to import a private helper.
fn in_organization(row: &Option<Uuid>, caller: Option<Uuid>) -> Result<(), ApiError> {
    match (row, caller) {
        (None, _) => Ok(()),
        (Some(_), None) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_organization",
            "this secret belongs to an organization",
        )),
        (Some(owner), Some(caller)) if *owner == caller => Ok(()),
        (Some(_), Some(_)) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_organization",
            "this secret belongs to another organization",
        )),
    }
}

fn not_found(what: &'static str) -> ApiError {
    let code = match what {
        "lease" => "lease_not_found",
        "deployment key" => "deployment_key_not_found",
        _ => "secrets_not_found",
    };
    ApiError::new(StatusCode::NOT_FOUND, code, format!("no such {what}"))
}

async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

/// Write an audit row, and never let an audit problem fail the caller's request.
async fn audit(state: &AppState, entry: NewAuditEntry) {
    if let Err(error) = omnion_audit::entries::record(state.db().pool(), entry).await {
        tracing::warn!(error = %error, "the audit row could not be written");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(name: &str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("a valid header name"),
            HeaderValue::from_str(value).expect("a valid header value"),
        );
        headers
    }

    #[test]
    fn a_deployment_key_is_read_from_its_own_header() {
        let headers = headers_with(DEPLOYMENT_KEY_HEADER, "  omnion_dk_abc  ");
        assert_eq!(
            presented_key(&headers).as_deref(),
            Some("omnion_dk_abc"),
            "the value is trimmed, so a CI newline cannot change what is presented"
        );
    }

    #[test]
    fn a_bearer_token_is_also_accepted() {
        let headers = headers_with(
            axum::http::header::AUTHORIZATION.as_str(),
            "Bearer omnion_dk_abc",
        );
        assert_eq!(presented_key(&headers).as_deref(), Some("omnion_dk_abc"));
    }

    #[test]
    fn no_header_means_no_machine_identity() {
        // A session cookie is deliberately not a fallback: redemption must be a machine
        // proving who it is, never a browser asking nicely.
        assert!(presented_key(&HeaderMap::new()).is_none());
    }

    #[test]
    fn a_blank_header_is_no_identity_rather_than_an_empty_one() {
        let headers = headers_with(DEPLOYMENT_KEY_HEADER, "   ");
        assert!(presented_key(&headers).is_none());
    }

    /// The wire format of the create body, asserted where the DTO lives.
    ///
    /// This test exists because of a real defect rather than as a formality: the drawer's
    /// `expires_at` was answered `422` for every operator. The panel sends
    /// `new Date(...).toISOString()` and the field deserialized from a *tuple*, so the one
    /// required field was the one field the API could not read — and the integration walk, which
    /// posts the same ISO string, caught it only on the tick after the slice shipped.
    ///
    /// A round trip through the exact bytes the browser sends is the cheapest way to keep every
    /// future timestamp in every future DTO honest. Written as deserialize-then-serialize rather
    /// than as a string comparison, so it also proves the two directions agree with each other.
    #[test]
    fn the_create_body_reads_the_iso_string_the_panel_sends() {
        let body = r#"{
            "name": "release-runner",
            "environment": "production",
            "scopes": ["secrets.read"],
            "expires_at": "2026-09-29T01:44:23.561667032Z"
        }"#;
        let input: CreateDeploymentKeyInput =
            serde_json::from_str(body).expect("an ISO timestamp must be a readable expiry");
        assert_eq!(input.name, "release-runner");
        assert_eq!(input.expires_at.year(), 2026);
        assert_eq!(input.expires_at.month(), time::Month::September);
        assert_eq!(input.expires_at.day(), 29);

        // And back out again, because the panel reads the same shape from the created response.
        // A local one-field struct stands in for the view types rather than widening the input
        // DTO to `Serialize`: the input's job is to *read* a body, and giving it a second,
        // untested responsibility to write one would be the wrong fix for a test.
        #[derive(serde::Serialize)]
        struct Rendered {
            #[serde(with = "time::serde::rfc3339")]
            expires_at: time::OffsetDateTime,
        }
        let rendered = serde_json::to_string(&Rendered {
            expires_at: input.expires_at,
        })
        .expect("serializable");
        assert!(
            rendered.contains('T') && !rendered.contains('['),
            "the expiry must leave as a string, not as a tuple: {rendered}"
        );
    }

    /// A tuple — what `time::OffsetDateTime` speaks natively — is not accepted. The point is not
    /// to forbid it forever but to document that the contract is RFC 3339, so nobody "fixes" a
    /// future failure by widening the parser and silently re-introducing the same bug.
    ///
    /// The assertion reads the *expected* form out of the message, not the field name. I wrote
    /// the field name first and it failed: `serde_json::Error`'s `Display` renders
    /// `invalid type: sequence, expected an RFC3339-formatted OffsetDateTime` with no path, so
    /// `contains("expires_at")` is false for a *correct* refusal. The field name lives in
    /// `err.line()/column()` and in the route's own `422` body, which the integration walk
    /// already asserts; the unit test's job here is the narrower one — the parser demands
    /// RFC 3339 and says so.
    #[test]
    fn a_tuple_expiry_is_refused_rather_than_guessed_at() {
        let body = r#"{
            "name": "release-runner",
            "environment": "production",
            "scopes": [],
            "expires_at": [2026, 272, 1, 44, 23, 0]
        }"#;
        let error = serde_json::from_str::<CreateDeploymentKeyInput>(body)
            .expect_err("a tuple is not an RFC 3339 timestamp");
        let message = error.to_string();
        assert!(
            message.contains("RFC3339") || message.contains("RFC 3339"),
            "the refusal states the format it wanted, which is the whole diagnostic: {message}"
        );
        assert!(
            message.contains("sequence") || message.contains("tuple"),
            "and it echoes what it actually received: {message}"
        );
    }
}
