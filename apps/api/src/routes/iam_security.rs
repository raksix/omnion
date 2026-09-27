//! `/api/v1/iam` — the security policy, sessions, devices and second factors (REQ-006, slice 3).
//!
//! Slice 3 makes the identity stack *operable*: an organization's policy is editable and every
//! threshold in it is read by the sign-in path; sessions can be listed and revoked; devices are
//! remembered with a trust window; and an account can hold a TOTP factor with single-use
//! recovery codes. Two design rules run through the module:
//!
//! 1. **Dangerous operations demand a fresh step-up.** Resetting another account's factors,
//!    revoking their sessions or issuing a machine key needs a mark the session does not have
//!    from ordinary browsing (`auth/step-up`): the caller proves identity again with their own
//!    password or an enrolled code. The window is [`STEP_UP_WINDOW_MINUTES`].
//! 2. **The policy is the only source of thresholds.** Nothing here hard-codes a lifetime, a
//!    lockout or a cap — the values come from `security_policies`, and the defaults live in
//!    `crates/identity` next to the table's own constraints.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::{HeaderValue, SET_COOKIE, USER_AGENT};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::mfa;
use omnion_identity::password::verify_password;
use omnion_identity::secrets::SecretBox;
use omnion_identity::security::{self, PolicyChange, PolicyPatch, SecurityPolicy};
use omnion_identity::sessions::{self, STEP_UP_WINDOW_MINUTES, SessionFilter, SessionView};
use omnion_identity::users;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::cookies;
use crate::error::ApiError;
use crate::routes::iam::record;
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

/// Product name used in the `otpauth://` label.
const TOTP_ISSUER: &str = "Omnion";

/// Largest page the session and device lists answer.
const MAX_PAGE: i64 = 500;

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

// ---------------------------------------------------------------------------------------------
// Security policy
// ---------------------------------------------------------------------------------------------

/// The security policy as the panel reads it.
#[derive(Debug, Serialize)]
pub struct PolicyBody {
    /// Organization the document belongs to.
    pub organization_id: Uuid,
    /// Minimum password length.
    pub password_min_length: i32,
    /// Character classes a password must use.
    pub password_require_classes: i32,
    /// How many previous passwords are remembered.
    pub password_history: i32,
    /// Password expiry in days (`0` = never).
    pub password_expiry_days: i32,
    /// Failed attempts before an account locks.
    pub lockout_attempts: i32,
    /// How long a lockout lasts, in minutes.
    pub lockout_minutes: i32,
    /// Networks allowed to sign in.
    pub ip_allowlist: Vec<String>,
    /// Networks refused outright.
    pub ip_denylist: Vec<String>,
    /// Idle session lifetime in minutes.
    pub session_idle_minutes: i32,
    /// Absolute session lifetime in days.
    pub session_absolute_days: i32,
    /// Concurrent-session cap.
    pub session_concurrent_max: i32,
    /// Device trust window in days.
    pub device_trust_days: i32,
    /// Whether a second factor is required by policy.
    pub mfa_required: bool,
    /// Who last changed the document.
    pub updated_by: Option<Uuid>,
    /// When it last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<&SecurityPolicy> for PolicyBody {
    fn from(policy: &SecurityPolicy) -> Self {
        Self {
            organization_id: policy.organization_id,
            password_min_length: policy.password_min_length,
            password_require_classes: policy.password_require_classes,
            password_history: policy.password_history,
            password_expiry_days: policy.password_expiry_days,
            lockout_attempts: policy.lockout_attempts,
            lockout_minutes: policy.lockout_minutes,
            ip_allowlist: policy.ip_allowlist.clone(),
            ip_denylist: policy.ip_denylist.clone(),
            session_idle_minutes: policy.session_idle_minutes,
            session_absolute_days: policy.session_absolute_days,
            session_concurrent_max: policy.session_concurrent_max,
            device_trust_days: policy.device_trust_days,
            mfa_required: policy.mfa_required,
            updated_by: policy.updated_by,
            updated_at: policy.updated_at,
        }
    }
}

/// One field of the before/after diff a save reports.
#[derive(Debug, Serialize)]
pub struct ChangeBody {
    /// Column name.
    pub field: &'static str,
    /// Value before.
    pub before: String,
    /// Value after.
    pub after: String,
}

impl From<&PolicyChange> for ChangeBody {
    fn from(change: &PolicyChange) -> Self {
        Self {
            field: change.field,
            before: change.before.clone(),
            after: change.after.clone(),
        }
    }
}

/// Response of a save: the state before and after, plus the fields that moved.
#[derive(Debug, Serialize)]
pub struct PolicySaveBody {
    /// The document before the save.
    pub before: PolicyBody,
    /// The document after the save.
    pub after: PolicyBody,
    /// Fields that changed, in column order.
    pub changes: Vec<ChangeBody>,
}

/// Query of the policy endpoints (a platform account names the tenant).
#[derive(Debug, Deserialize)]
pub struct OrganizationQuery {
    /// Organization to work in; required for a platform account.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// Body of `PUT /api/v1/iam/security-policies`; every field is optional (a partial save).
#[derive(Debug, Default, Deserialize)]
pub struct PolicyPatchBody {
    /// See [`PolicyBody::password_min_length`].
    #[serde(default)]
    pub password_min_length: Option<i32>,
    /// See [`PolicyBody::password_require_classes`].
    #[serde(default)]
    pub password_require_classes: Option<i32>,
    /// See [`PolicyBody::password_history`].
    #[serde(default)]
    pub password_history: Option<i32>,
    /// See [`PolicyBody::password_expiry_days`].
    #[serde(default)]
    pub password_expiry_days: Option<i32>,
    /// See [`PolicyBody::lockout_attempts`].
    #[serde(default)]
    pub lockout_attempts: Option<i32>,
    /// See [`PolicyBody::lockout_minutes`].
    #[serde(default)]
    pub lockout_minutes: Option<i32>,
    /// See [`PolicyBody::ip_allowlist`].
    #[serde(default)]
    pub ip_allowlist: Option<Vec<String>>,
    /// See [`PolicyBody::ip_denylist`].
    #[serde(default)]
    pub ip_denylist: Option<Vec<String>>,
    /// See [`PolicyBody::session_idle_minutes`].
    #[serde(default)]
    pub session_idle_minutes: Option<i32>,
    /// See [`PolicyBody::session_absolute_days`].
    #[serde(default)]
    pub session_absolute_days: Option<i32>,
    /// See [`PolicyBody::session_concurrent_max`].
    #[serde(default)]
    pub session_concurrent_max: Option<i32>,
    /// See [`PolicyBody::device_trust_days`].
    #[serde(default)]
    pub device_trust_days: Option<i32>,
    /// See [`PolicyBody::mfa_required`].
    #[serde(default)]
    pub mfa_required: Option<bool>,
}

impl PolicyPatchBody {
    /// The store-side patch, trimmed of blank IP list entries a textarea leaves behind.
    fn to_patch(&self) -> PolicyPatch {
        let clean = |list: &Option<Vec<String>>| {
            list.as_ref().map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.trim().to_owned())
                    .filter(|entry| !entry.is_empty())
                    .collect::<Vec<String>>()
            })
        };
        PolicyPatch {
            password_min_length: self.password_min_length,
            password_require_classes: self.password_require_classes,
            password_history: self.password_history,
            password_expiry_days: self.password_expiry_days,
            lockout_attempts: self.lockout_attempts,
            lockout_minutes: self.lockout_minutes,
            ip_allowlist: clean(&self.ip_allowlist),
            ip_denylist: clean(&self.ip_denylist),
            session_idle_minutes: self.session_idle_minutes,
            session_absolute_days: self.session_absolute_days,
            session_concurrent_max: self.session_concurrent_max,
            device_trust_days: self.device_trust_days,
            mfa_required: self.mfa_required,
        }
    }
}

/// Read the security policy of an organization (creating the defaults when missing).
pub async fn get_security_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<OrganizationQuery>,
) -> Result<Json<PolicyBody>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let policy = security::ensure_policy(state.db().pool(), organization_id).await?;
    Ok(Json(PolicyBody::from(&policy)))
}

/// Update the security policy and answer the diff the save applied.
pub async fn update_security_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<OrganizationQuery>,
    Json(body): Json<PolicyPatchBody>,
) -> Result<Json<PolicySaveBody>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let patch = body.to_patch();
    let (before, after) = security::update_policy(
        state.db().pool(),
        organization_id,
        &patch,
        Some(current.user.id),
    )
    .await?;

    let changes = security::diff(&before, &after);
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.security_policy_updated")
            .organization(organization_id)
            .target("organization", organization_id.to_string())
            .metadata(json!({
                "changed": changes
                    .iter()
                    .map(|change| json!({
                        "field": change.field,
                        "before": change.before,
                        "after": change.after,
                    }))
                    .collect::<Vec<_>>(),
            })),
    )
    .await?;

    // The webhook surface hears about it: the security centre (REQ-012) subscribes to this name
    // to explain a refusal the operator did not expect.
    emit(
        &state,
        NewEvent::new("iam.policy_changed")
            .organization(Some(organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "scope": "security_policy",
                "fields": changes.iter().map(|change| change.field).collect::<Vec<_>>(),
            })),
    )
    .await;

    Ok(Json(PolicySaveBody {
        before: PolicyBody::from(&before),
        after: PolicyBody::from(&after),
        changes: changes.iter().map(ChangeBody::from).collect(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

/// One row of the session list.
#[derive(Debug, Serialize)]
pub struct SessionBody {
    /// Session id.
    pub id: Uuid,
    /// Owning account.
    pub user_id: Uuid,
    /// Account address.
    pub user_email: String,
    /// Account display name.
    pub user_display_name: String,
    /// Address the session came from.
    pub ip_address: Option<String>,
    /// User agent as recorded.
    pub user_agent: Option<String>,
    /// Device label, when the session carries one.
    pub device_label: Option<String>,
    /// How the caller authenticated.
    pub auth_methods: Vec<String>,
    /// Creation timestamp.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last activity.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen_at: Option<OffsetDateTime>,
    /// Expiry.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// Hard end, when one was set.
    #[serde(with = "time::serde::rfc3339::option")]
    pub absolute_expires_at: Option<OffsetDateTime>,
    /// Revocation timestamp.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// Revocation reason.
    pub revoke_reason: Option<String>,
    /// When the caller last stepped up.
    #[serde(with = "time::serde::rfc3339::option")]
    pub step_up_at: Option<OffsetDateTime>,
    /// `live`, `idle`, `expired` or `revoked`.
    pub state: &'static str,
    /// Whether this row is the caller's own session.
    pub current: bool,
    /// Whether the caller may revoke it (their own, or a session in their tenant).
    pub revocable: bool,
}

/// Response of the session list.
#[derive(Debug, Serialize)]
pub struct SessionsResponse {
    /// The page.
    pub sessions: Vec<SessionBody>,
    /// How many rows the filter matched.
    pub total: usize,
    /// The idle window the states were judged by, in minutes.
    pub idle_minutes: i32,
}

/// Query of `GET /api/v1/iam/sessions`.
#[derive(Debug, Deserialize)]
pub struct SessionsQuery {
    /// Only sessions of this account.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Only sessions of this organization.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Free text over account address and display name.
    #[serde(default)]
    pub search: Option<String>,
    /// `live`, `idle`, `expired` or `revoked`.
    #[serde(default)]
    pub state: Option<String>,
    /// Include revoked and expired rows.
    #[serde(default)]
    pub include_inactive: Option<bool>,
}

/// List sessions, newest first, with the state each row is in.
pub async fn list_sessions(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SessionsQuery>,
) -> Result<Json<SessionsResponse>, ApiError> {
    ensure_same_organization(&current, query.organization_id)?;
    let organization_id = match current.user.organization_id {
        Some(own) => Some(own),
        None => query.organization_id,
    };

    let filter = SessionFilter {
        user_id: query.user_id,
        organization_id,
        search: query.search.clone(),
        state: query.state.clone(),
        include_inactive: query.include_inactive.unwrap_or(false),
    };

    let idle_minutes =
        sessions::idle_minutes_for_organization(state.db().pool(), organization_id).await?;
    let now = OffsetDateTime::now_utc();
    let rows = sessions::list_sessions(state.db().pool(), &filter).await?;

    let mut sessions: Vec<SessionBody> = rows
        .iter()
        .map(|row| SessionBody::from_row(row, now, idle_minutes, current.session.id))
        .collect();
    // The state filter is computed from the same values the reader sees, so a filtered list is
    // exactly the rows whose badge matches the filter.
    if let Some(wanted) = filter.state.as_deref() {
        sessions.retain(|session| session.state == wanted);
    }
    sessions.truncate(usize::try_from(MAX_PAGE).unwrap_or(500));

    let total = sessions.len();
    Ok(Json(SessionsResponse {
        sessions,
        total,
        idle_minutes,
    }))
}

impl SessionBody {
    /// Build a row body from a stored session and the policy the states follow.
    fn from_row(
        row: &SessionView,
        now: OffsetDateTime,
        idle_minutes: i32,
        own_session: Uuid,
    ) -> Self {
        Self {
            id: row.id,
            user_id: row.user_id,
            user_email: row.user_email.clone(),
            user_display_name: row.user_display_name.clone(),
            ip_address: row.ip_address.clone(),
            user_agent: row.user_agent.clone(),
            device_label: row.device_label.clone(),
            auth_methods: row.auth_methods.clone(),
            created_at: row.created_at,
            last_seen_at: row.last_seen_at,
            expires_at: row.expires_at,
            absolute_expires_at: row.absolute_expires_at,
            revoked_at: row.revoked_at,
            revoke_reason: row.revoke_reason.clone(),
            step_up_at: row.step_up_at,
            state: sessions::state_of(row, now, idle_minutes),
            current: row.id == own_session,
            revocable: row.revoked_at.is_none(),
        }
    }
}

/// Revoke one session by id.
pub async fn revoke_session(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(session_id): Path<Uuid>,
    client: ClientAddress,
) -> Result<Json<SessionBody>, ApiError> {
    let revoked = sessions::revoke_session_by_id(
        state.db().pool(),
        session_id,
        Some(current.user.id),
        "admin_revoke",
    )
    .await?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "this session is already gone",
        )
    })?;

    // A tenant account may only revoke sessions it can see; the check runs after the revoke
    // because the row has to be read to know whose it is — a cross-tenant attempt is undone by
    // the same statement that refuses the request.
    if current.user.organization_id.is_some() {
        let target = organization_of(&state, revoked.user_id).await?;
        if target != current.user.organization_id {
            return Err(crate::scope::cross_organization());
        }
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.session_revoked")
            .organization(current.user.organization_id)
            .target("session", revoked.id.to_string())
            .ip_address(client.as_text())
            .metadata(json!({ "user_id": revoked.user_id, "reason": "admin_revoke" })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.session_revoked")
            .organization(current.user.organization_id)
            .actor(Some(current.user.id))
            .payload(json!({
                "session_id": revoked.id,
                "user_id": revoked.user_id,
                "reason": "admin_revoke",
            })),
    )
    .await;

    let now = OffsetDateTime::now_utc();
    let idle_minutes =
        sessions::idle_minutes_for_organization(state.db().pool(), current.user.organization_id)
            .await?;
    let filter = SessionFilter {
        user_id: Some(revoked.user_id),
        organization_id: current.user.organization_id,
        include_inactive: true,
        ..SessionFilter::default()
    };
    let row = sessions::list_sessions(state.db().pool(), &filter)
        .await?
        .into_iter()
        .find(|candidate| candidate.id == revoked.id);
    let Some(row) = row else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "this session is already gone",
        ));
    };

    Ok(Json(SessionBody::from_row(
        &row,
        now,
        idle_minutes,
        current.session.id,
    )))
}

/// Sign an account out everywhere: every live session of theirs is revoked, one event each.
pub async fn sign_out_all(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(user_id): Path<Uuid>,
    client: ClientAddress,
) -> Result<Json<serde_json::Value>, ApiError> {
    let organization_id = organization_of(&state, user_id).await?;
    if organization_id.is_none()
        && sqlx::query_scalar::<_, i64>("select count(*) from users where id = $1")
            .bind(user_id)
            .fetch_one(state.db().pool())
            .await
            .map_err(omnion_identity::IdentityError::Database)?
            == 0
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "user_not_found",
            "no such account",
        ));
    }
    ensure_same_organization(&current, organization_id)?;
    if current.user.organization_id.is_some() && organization_id.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "user_not_found",
            "no such account in this organization",
        ));
    }

    let revoked = sessions::sign_out_all(
        state.db().pool(),
        user_id,
        Some(current.user.id),
        "sign_out_all",
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.sign_out_all")
            .organization(current.user.organization_id)
            .target("user", user_id.to_string())
            .ip_address(client.as_text())
            .metadata(json!({ "revoked": revoked.len() })),
    )
    .await?;

    for session_id in &revoked {
        emit(
            &state,
            NewEvent::new("iam.session_revoked")
                .organization(current.user.organization_id)
                .actor(Some(current.user.id))
                .payload(json!({
                    "session_id": session_id,
                    "user_id": user_id,
                    "reason": "sign_out_all",
                })),
        )
        .await;
    }

    Ok(Json(json!({
        "user_id": user_id,
        "revoked": revoked.len(),
        "session_ids": revoked,
    })))
}

// ---------------------------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------------------------

/// One row of the device list.
#[derive(Debug, Serialize)]
pub struct DeviceBody {
    /// Device id.
    pub id: Uuid,
    /// Owning account.
    pub user_id: Uuid,
    /// Account address.
    pub user_email: String,
    /// Account display name.
    pub user_display_name: String,
    /// Human label.
    pub label: String,
    /// Operating system family.
    pub platform: String,
    /// Browser family.
    pub browser: String,
    /// Short fingerprint prefix (safe to show, not reversible).
    pub fingerprint_hint: String,
    /// First observation.
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen_at: OffsetDateTime,
    /// Last observation.
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    /// Trust window end.
    #[serde(with = "time::serde::rfc3339::option")]
    pub trusted_until: Option<OffsetDateTime>,
    /// Whether the device was forgotten.
    pub revoked: bool,
    /// Live sessions on this device.
    pub session_count: i64,
    /// Whether the device is trusted right now.
    pub trusted: bool,
}

impl From<omnion_identity::devices::DeviceView> for DeviceBody {
    fn from(view: omnion_identity::devices::DeviceView) -> Self {
        let trusted = view
            .trusted_until
            .is_some_and(|until| until > OffsetDateTime::now_utc());
        Self {
            id: view.id,
            user_id: view.user_id,
            user_email: view.user_email,
            user_display_name: view.user_display_name,
            label: view.label,
            platform: view.platform,
            browser: view.browser,
            fingerprint_hint: view.fingerprint_hint,
            first_seen_at: view.first_seen_at,
            last_seen_at: view.last_seen_at,
            trusted_until: view.trusted_until,
            revoked: view.revoked,
            session_count: view.session_count,
            trusted,
        }
    }
}

/// Query of `GET /api/v1/iam/devices`.
#[derive(Debug, Deserialize)]
pub struct DevicesQuery {
    /// Only devices of this account.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Only devices of this organization.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Free text over label, platform, browser and account address.
    #[serde(default)]
    pub search: Option<String>,
    /// Include forgotten devices.
    #[serde(default)]
    pub include_revoked: Option<bool>,
}

/// List known devices, newest observation first.
pub async fn list_devices(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<DevicesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_same_organization(&current, query.organization_id)?;
    let organization_id = match current.user.organization_id {
        Some(own) => Some(own),
        None => query.organization_id,
    };

    let filter = omnion_identity::devices::DeviceFilter {
        user_id: query.user_id,
        organization_id,
        search: query.search.clone(),
        include_revoked: query.include_revoked.unwrap_or(false),
    };
    let devices = omnion_identity::devices::list(state.db().pool(), &filter).await?;
    let total = devices.len();
    let device_trust_days = match organization_id {
        Some(organization_id) => {
            security::ensure_policy(state.db().pool(), organization_id)
                .await?
                .device_trust_days
        }
        None => 30,
    };

    Ok(Json(json!({
        "devices": devices.into_iter().map(DeviceBody::from).collect::<Vec<_>>(),
        "total": total,
        "device_trust_days": device_trust_days,
    })))
}

/// Body of `POST /api/v1/iam/devices/{id}/trust`.
#[derive(Debug, Default, Deserialize)]
pub struct TrustBody {
    /// Trust the device for this many days (0 clears the window).
    #[serde(default)]
    pub days: Option<i32>,
    /// Or trust it until this instant (RFC 3339).
    #[serde(default)]
    pub trusted_until: Option<String>,
}

/// Set (or clear) a device's trust window.
pub async fn trust_device(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(device_id): Path<Uuid>,
    Json(body): Json<TrustBody>,
) -> Result<Json<DeviceBody>, ApiError> {
    let device = omnion_identity::devices::find(state.db().pool(), device_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "device_not_found", "no such device")
        })?;
    let owner_organization = organization_of(&state, device.user_id).await?;
    ensure_same_organization(&current, owner_organization)?;

    let trusted_until = match (body.days, body.trusted_until.as_deref()) {
        (Some(days), _) if days <= 0 => None,
        (Some(days), _) if days > 3650 => {
            return Err(ApiError::bad_request(
                "invalid_trust_window",
                "days must be between 0 and 3650",
            ));
        }
        (Some(days), _) => Some(OffsetDateTime::now_utc() + time::Duration::days(i64::from(days))),
        (None, Some(text)) => Some(OffsetDateTime::parse(text, &Rfc3339).map_err(|_| {
            ApiError::bad_request(
                "invalid_trust_window",
                "trusted_until must be an RFC 3339 timestamp",
            )
        })?),
        (None, None) => {
            let days = match owner_organization {
                Some(organization_id) => {
                    security::ensure_policy(state.db().pool(), organization_id)
                        .await?
                        .device_trust_days
                }
                None => 30,
            };
            (days > 0).then(|| OffsetDateTime::now_utc() + time::Duration::days(i64::from(days)))
        }
    };

    let updated = omnion_identity::devices::set_trust(state.db().pool(), device_id, trusted_until)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "device_not_found", "no such device")
        })?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.device_trusted")
            .organization(current.user.organization_id)
            .target("device", updated.id.to_string())
            .metadata(
                json!({ "trusted_until": updated.trusted_until.map(|value| value.to_string()) }),
            ),
    )
    .await?;

    let views = omnion_identity::devices::list(
        state.db().pool(),
        &omnion_identity::devices::DeviceFilter {
            user_id: Some(updated.user_id),
            include_revoked: true,
            ..omnion_identity::devices::DeviceFilter::default()
        },
    )
    .await?;
    let view = views
        .into_iter()
        .find(|candidate| candidate.id == updated.id)
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "device_not_found", "no such device")
        })?;

    Ok(Json(DeviceBody::from(view)))
}

/// Forget a device: the row is revoked and its live sessions end with it.
pub async fn forget_device(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(device_id): Path<Uuid>,
) -> Result<Json<DeviceBody>, ApiError> {
    let device = omnion_identity::devices::find(state.db().pool(), device_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "device_not_found", "no such device")
        })?;
    let owner_organization = organization_of(&state, device.user_id).await?;
    ensure_same_organization(&current, owner_organization)?;

    let forgotten = omnion_identity::devices::forget(state.db().pool(), device_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "device_not_found", "no such device")
        })?;

    // A forgotten device must not keep a live session: "forget" means the device has to sign in
    // again, which is what a reader expects when they remove a device they no longer hold.
    let session_ids: Vec<Uuid> = sqlx::query_scalar(
        "update sessions set revoked_at = now(), revoked_by = $2, revoke_reason = 'device_forgotten' \
         where device_id = $1 and revoked_at is null \
         returning id",
    )
    .bind(device_id)
    .bind(current.user.id)
    .fetch_all(state.db().pool())
    .await
    .map_err(omnion_identity::IdentityError::Database)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.device_forgotten")
            .organization(current.user.organization_id)
            .target("device", forgotten.id.to_string())
            .metadata(json!({ "sessions_revoked": session_ids.len() })),
    )
    .await?;

    for session_id in &session_ids {
        emit(
            &state,
            NewEvent::new("iam.session_revoked")
                .organization(current.user.organization_id)
                .actor(Some(current.user.id))
                .payload(json!({
                    "session_id": session_id,
                    "user_id": forgotten.user_id,
                    "reason": "device_forgotten",
                })),
        )
        .await;
    }

    let views = omnion_identity::devices::list(
        state.db().pool(),
        &omnion_identity::devices::DeviceFilter {
            user_id: Some(forgotten.user_id),
            include_revoked: true,
            ..omnion_identity::devices::DeviceFilter::default()
        },
    )
    .await?;
    let view = views
        .into_iter()
        .find(|candidate| candidate.id == forgotten.id)
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "device_not_found", "no such device")
        })?;

    Ok(Json(DeviceBody::from(view)))
}

// ---------------------------------------------------------------------------------------------
// Second factors
// ---------------------------------------------------------------------------------------------

/// Step-up guard: refuse a dangerous operation whose session has no fresh proof.
///
/// Shared with the service-account key route (`iam_subjects`), because issuing a credential is
/// the same class of action as resetting a factor.
pub(crate) fn require_step_up(
    current: &CurrentSession,
    action: &'static str,
) -> Result<(), ApiError> {
    if sessions::step_up_is_fresh(
        &current.session,
        OffsetDateTime::now_utc(),
        STEP_UP_WINDOW_MINUTES,
    ) {
        return Ok(());
    }

    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "step_up_required",
        "prove it is you again before this action: POST /api/v1/auth/step-up",
    )
    .with_details(json!({ "action": action, "window_minutes": STEP_UP_WINDOW_MINUTES })))
}

/// One factor as the panel reads it (never the secret).
#[derive(Debug, Serialize)]
pub struct FactorBody {
    /// Factor id.
    pub id: Uuid,
    /// `totp`, `webauthn` or `recovery`.
    pub kind: String,
    /// Reader-facing label.
    pub label: String,
    /// Whether the factor was confirmed with a code.
    pub confirmed: bool,
    /// Confirmation timestamp.
    #[serde(with = "time::serde::rfc3339::option")]
    pub confirmed_at: Option<OffsetDateTime>,
    /// Last verification.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    /// Creation timestamp.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<mfa::MfaFactor> for FactorBody {
    fn from(factor: mfa::MfaFactor) -> Self {
        Self {
            id: factor.id,
            kind: factor.kind,
            label: factor.label,
            confirmed: factor.confirmed_at.is_some(),
            confirmed_at: factor.confirmed_at,
            last_used_at: factor.last_used_at,
            created_at: factor.created_at,
        }
    }
}

/// The account a factor route targets, after tenancy checks.
async fn target_account(
    state: &AppState,
    current: &CurrentSession,
    user_id: Uuid,
) -> Result<(), ApiError> {
    let exists: Option<Uuid> = sqlx::query_scalar("select id from users where id = $1")
        .bind(user_id)
        .fetch_optional(state.db().pool())
        .await
        .map_err(omnion_identity::IdentityError::Database)?;
    if exists.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "user_not_found",
            "no such account",
        ));
    }
    ensure_same_organization(current, organization_of(state, user_id).await?)?;
    Ok(())
}

/// The organization an account works in — read outside the identity store's helpers, so the
/// tenancy rule can run before a handler touches the row.
async fn organization_of(state: &AppState, user_id: Uuid) -> Result<Option<Uuid>, ApiError> {
    let organization_id: Option<Option<Uuid>> =
        sqlx::query_scalar("select organization_id from users where id = $1")
            .bind(user_id)
            .fetch_optional(state.db().pool())
            .await
            .map_err(omnion_identity::IdentityError::Database)?;
    Ok(organization_id.flatten())
}

/// List an account's factors and how many recovery codes are left.
pub async fn list_factors(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(user_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    target_account(&state, &current, user_id).await?;
    let factors = mfa::list_factors(state.db().pool(), user_id).await?;
    let remaining = mfa::remaining_recovery_codes(state.db().pool(), user_id).await?;
    let confirmed = factors
        .iter()
        .filter(|factor| factor.confirmed_at.is_some())
        .count();
    Ok(Json(json!({
        "user_id": user_id,
        "factors": factors.into_iter().map(FactorBody::from).collect::<Vec<_>>(),
        "recovery_codes_remaining": remaining,
        "confirmed": confirmed,
    })))
}

/// Body of `POST /api/v1/iam/users/{id}/mfa/totp`.
#[derive(Debug, Default, Deserialize)]
pub struct EnrollBody {
    /// Label the account holder recognises.
    #[serde(default)]
    pub label: Option<String>,
}

/// Start TOTP enrolment: the secret is answered exactly once.
pub async fn enroll_totp(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(user_id): Path<Uuid>,
    Json(body): Json<EnrollBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    target_account(&state, &current, user_id).await?;
    let account = users::find_by_id(state.db().pool(), user_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "user_not_found", "no such account"))?;

    let secret_box = SecretBox::from_env();
    let enrollment = mfa::enroll_totp(
        state.db().pool(),
        user_id,
        body.label.as_deref(),
        TOTP_ISSUER,
        &account.email,
        &secret_box,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.mfa_enrolment_started")
            .organization(account.organization_id)
            .target("user", user_id.to_string())
            .metadata(json!({ "factor_id": enrollment.factor.id, "kind": "totp" })),
    )
    .await?;

    Ok(Json(json!({
        "factor": FactorBody::from(enrollment.factor),
        "secret": enrollment.secret,
        "otpauth_uri": enrollment.otpauth_uri,
    })))
}

/// Body of the confirmation route.
#[derive(Debug, Deserialize)]
pub struct ConfirmBody {
    /// The six-digit code the authenticator app shows.
    pub code: String,
}

/// Confirm a pending TOTP factor and issue the recovery codes (shown once).
pub async fn confirm_totp(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((user_id, factor_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ConfirmBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    target_account(&state, &current, user_id).await?;
    let secret_box = SecretBox::from_env();
    let codes = mfa::confirm_totp(
        state.db().pool(),
        user_id,
        factor_id,
        &body.code,
        &secret_box,
        OffsetDateTime::now_utc().unix_timestamp(),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.mfa_enrolled")
            .organization(current.user.organization_id)
            .target("user", user_id.to_string())
            .metadata(
                json!({ "factor_id": factor_id, "kind": "totp", "recovery_codes": codes.len() }),
            ),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.mfa_enrolled")
            .organization(current.user.organization_id)
            .actor(Some(current.user.id))
            .payload(json!({ "user_id": user_id, "kind": "totp" })),
    )
    .await;

    Ok(Json(json!({
        "factor_id": factor_id,
        "confirmed": true,
        "recovery_codes": codes,
    })))
}

/// Remove one factor (the account holder dropping a device they no longer have).
pub async fn revoke_factor(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((user_id, factor_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    target_account(&state, &current, user_id).await?;
    let factor = mfa::find_factor(state.db().pool(), user_id, factor_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "factor_not_found",
                "this account has no such second factor",
            )
        })?;

    // Removing a confirmed factor is as dangerous as resetting: require the step-up unless the
    // caller is retiring their own unconfirmed enrolment.
    let own = current.user.id == user_id;
    if factor.confirmed_at.is_some() || !own {
        require_step_up(&current, "factor_revoke")?;
    }

    let revoked = mfa::revoke_factor(state.db().pool(), user_id, factor_id).await?;
    if !revoked {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "factor_not_found",
            "this account has no such second factor",
        ));
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.mfa_removed")
            .organization(current.user.organization_id)
            .target("user", user_id.to_string())
            .metadata(json!({ "factor_id": factor_id })),
    )
    .await?;

    Ok(Json(json!({ "factor_id": factor_id, "revoked": true })))
}

/// Clear every factor of an account (admin reset), after a step-up.
pub async fn reset_mfa(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(user_id): Path<Uuid>,
    client: ClientAddress,
) -> Result<Json<serde_json::Value>, ApiError> {
    target_account(&state, &current, user_id).await?;
    require_step_up(&current, "mfa_reset")?;

    let revoked = mfa::reset(state.db().pool(), user_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.mfa_reset")
            .organization(current.user.organization_id)
            .target("user", user_id.to_string())
            .ip_address(client.as_text())
            .metadata(json!({ "factors_revoked": revoked })),
    )
    .await?;

    Ok(Json(
        json!({ "user_id": user_id, "factors_revoked": revoked }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Step-up and the second factor at sign-in
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/auth/step-up`.
#[derive(Debug, Default, Deserialize)]
pub struct StepUpBody {
    /// The caller's own password.
    #[serde(default)]
    pub password: Option<String>,
    /// Or a code from an enrolled factor.
    #[serde(default)]
    pub code: Option<String>,
}

/// Prove identity again for a dangerous operation; the session records the moment.
pub async fn step_up(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<StepUpBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut verified = false;

    if let Some(password) = body.password.as_deref() {
        if !password.is_empty() {
            let credentials = users::find_credentials(state.db().pool(), &current.user.email)
                .await?
                .ok_or_else(|| {
                    ApiError::unauthorized("invalid_credentials", "the password did not match")
                })?;
            if !verify_password(password.to_owned(), credentials.password_hash).await? {
                return Err(ApiError::unauthorized(
                    "invalid_credentials",
                    "the password did not match",
                ));
            }
            verified = true;
        }
    }

    if !verified {
        if let Some(code) = body.code.as_deref() {
            let secret_box = SecretBox::from_env();
            if mfa::verify_code(
                state.db().pool(),
                current.user.id,
                code,
                &secret_box,
                OffsetDateTime::now_utc().unix_timestamp(),
            )
            .await?
            .is_none()
            {
                return Err(ApiError::bad_request(
                    "invalid_factor_code",
                    "that code does not match",
                ));
            }
            verified = true;
        }
    }

    if !verified {
        return Err(ApiError::bad_request(
            "step_up_required",
            "send your password or a code from an enrolled factor",
        ));
    }

    sessions::mark_step_up(state.db().pool(), current.session.id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.step_up")
            .organization(current.user.organization_id)
            .target("session", current.session.id.to_string())
            .metadata(json!({ "window_minutes": STEP_UP_WINDOW_MINUTES })),
    )
    .await?;

    Ok(Json(json!({
        "session_id": current.session.id,
        "step_up": true,
        "window_minutes": STEP_UP_WINDOW_MINUTES,
    })))
}

/// Body of `POST /api/v1/auth/mfa/verify`.
#[derive(Debug, Deserialize)]
pub struct MfaVerifyBody {
    /// The challenge token a password check answered with.
    pub challenge: String,
    /// The code from the enrolled factor (or a recovery code).
    pub code: String,
}

/// Response of a completed sign-in.
#[derive(Debug, Serialize)]
pub struct VerifiedLoginBody {
    /// The signed-in account.
    pub user: crate::dto::UserBody,
    /// How the second factor was proven (`totp` or `recovery`).
    pub method: &'static str,
    /// Recovery codes left afterwards.
    pub recovery_codes_remaining: i64,
}

/// Finish a sign-in with the second factor; the response carries the session cookie.
///
/// The session this creates is a full session — device, policy lifetimes, auth methods — so it
/// is indistinguishable from a password-only sign-in except by the factor it took to get here.
pub async fn verify_mfa_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    client: ClientAddress,
    Json(body): Json<MfaVerifyBody>,
) -> Result<Response, ApiError> {
    let secret_box = SecretBox::from_env();
    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let ip_address = client.as_text();

    let Some((user, verification)) = omnion_identity::signin::complete_mfa_login(
        state.db().pool(),
        &secret_box,
        &body.challenge,
        &body.code,
        ip_address.as_deref(),
        user_agent.as_deref(),
    )
    .await?
    else {
        return Err(ApiError::unauthorized(
            "invalid_challenge",
            "this sign-in challenge is no longer valid — sign in again",
        ));
    };

    let policy =
        omnion_identity::signin::session_policy_for(state.db().pool(), user.organization_id)
            .await?;
    let device = omnion_identity::devices::upsert(
        state.db().pool(),
        user.id,
        user_agent.as_deref(),
        match user.organization_id {
            Some(organization_id) => {
                security::ensure_policy(state.db().pool(), organization_id)
                    .await?
                    .device_trust_days
            }
            None => 30,
        },
    )
    .await?;

    let method = match verification {
        mfa::Verification::Totp { .. } => "totp",
        mfa::Verification::Recovery => "recovery",
    };

    let (session, token) = sessions::create_session_with_policy(
        state.db().pool(),
        user.id,
        sessions::NewSession {
            user_agent: user_agent.clone(),
            ip_address: ip_address.clone(),
            device_id: Some(device.id),
            auth_methods: vec!["password".to_owned(), method.to_owned()],
        },
        policy,
    )
    .await?;

    tracing::info!(user_id = %user.id, session_id = %session.id, method, "session created after a second factor");

    let secure = !state.config().env.is_development();
    let cookie = cookies::session_cookie(&token, sessions::SESSION_TTL_SECONDS, secure);
    let remaining = mfa::remaining_recovery_codes(state.db().pool(), user.id).await?;

    let mut response = Json(VerifiedLoginBody {
        user: crate::dto::UserBody::from(&user),
        method,
        recovery_codes_remaining: remaining,
    })
    .into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("session cookie is valid header text"),
    );
    Ok(response)
}
