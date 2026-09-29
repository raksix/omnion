//! `/api/v1/scim/v2` — SCIM 2.0 provisioning (docs/07-IAM.md §19; REQ-006, slice 4b).
//!
//! An identity provider (Okta, Entra ID, Keycloak …) keeps Omnion's accounts in step through
//! this surface: it presents a **provisioning token** (`omsc_…`, see
//! `omnion_identity::provisioning`) as a bearer credential, and everything it does lands in the
//! sync log the provisioning screen reads.
//!
//! The shape is the one the RFC asks for: `ListResponse` envelopes, `PatchOp` with `Operations`,
//! and error documents carrying `schemas`. Two deliberate simplifications, both documented on the
//! screen:
//!
//! * **Deactivation, not deletion** — `DELETE /Users/{id}` sets the account to `disabled`, the
//!   SCIM default, because deleting a person's history is never what a directory sync means;
//! * the supported filter surface is small and explicit (`userName`, `externalId`,
//!   `displayName` with `eq`), refused with `invalidFilter` rather than half-applied.
//!
//! `ServiceProviderConfig` and `Schemas` are read without a token: a client has to be able to
//! introspect the endpoint before it is trusted with credentials, and neither document carries
//! any tenant data.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use omnion_identity::provisioning::{self, ProvisioningToken};
use omnion_identity::sso::scim_runs;
use omnion_identity::users;
use omnion_permissions::groups;
use serde::Deserialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::state::AppState;

/// Core user schema URN.
const SCIM_USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
/// Core group schema URN.
const SCIM_GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
/// List envelope URN.
const SCIM_LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
/// Error document URN.
const SCIM_ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
/// PatchOp URN.
const SCIM_PATCH: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";

/// Largest page this endpoint answers (`count` may ask for less).
const MAX_PAGE: i64 = 200;

/// A SCIM-shaped failure: `{"schemas":["…Error"],"detail":…,"status":"4xx"}`.
#[derive(Debug)]
pub struct ScimError {
    status: StatusCode,
    scim_type: Option<&'static str>,
    detail: String,
}

impl ScimError {
    /// Build one with a status and a readable sentence.
    fn new(status: StatusCode, scim_type: Option<&'static str>, detail: impl Into<String>) -> Self {
        Self {
            status,
            scim_type,
            detail: detail.into(),
        }
    }

    /// `400` — the request is not usable.
    fn bad_request(scim_type: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some(scim_type), detail)
    }

    /// `401` — the token is missing, unknown or revoked.
    fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, None, detail)
    }

    /// `404` — no such resource in the token's organization.
    fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, None, detail)
    }
}

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "schemas": [SCIM_ERROR],
            "detail": self.detail,
            "status": self.status.as_u16().to_string(),
        });
        if let Some(scim_type) = self.scim_type {
            body["scimType"] = json!(scim_type);
        }
        (self.status, Json(body)).into_response()
    }
}

/// The provisioning token a request presented.
#[derive(Debug, Clone)]
pub struct ProvisioningPrincipal {
    /// The token row (its secret is not stored, so it cannot be read back).
    pub token: ProvisioningToken,
}

impl ProvisioningPrincipal {
    /// Organization this token provisions into.
    fn organization_id(&self) -> Uuid {
        self.token.organization_id
    }
}

impl axum::extract::FromRequestParts<AppState> for ProvisioningPrincipal {
    type Rejection = ScimError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let presented = crate::guards::bearer_token(&parts.headers).ok_or_else(|| {
            ScimError::unauthorized("present a provisioning token as `Authorization: Bearer`")
        })?;

        let token = provisioning::verify_token(state.db().pool(), &presented)
            .await
            .map_err(|error| {
                tracing::warn!(error = %error, "the provisioning token could not be verified");
                ScimError::unauthorized("the provisioning token could not be verified")
            })?
            .ok_or_else(|| {
                ScimError::unauthorized("this provisioning token is unknown, revoked or wrong")
            })?;

        Ok(Self { token })
    }
}

/// `GET /scim/v2/ServiceProviderConfig` — what this endpoint supports.
pub async fn service_provider_config() -> Json<Value> {
    Json(json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
        "documentationUri": "https://omnion.dev/docs/07-IAM.md",
        "patch": { "supported": true },
        "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
        "filter": { "supported": true, "maxResults": MAX_PAGE },
        "changePassword": { "supported": false },
        "sort": { "supported": false },
        "etag": { "supported": false },
        "authenticationSchemes": [{
            "type": "oauthbearertoken",
            "name": "Provisioning token",
            "description": "A token minted on the provisioning screen (prefix `omsc_`).",
            "specUri": "https://www.rfc-editor.org/rfc/rfc6750",
            "primary": true,
        }],
    }))
}

/// `GET /scim/v2/Schemas` — the two resource schemas this endpoint speaks.
pub async fn schemas() -> Json<Value> {
    Json(json!({
        "schemas": [SCIM_LIST],
        "totalResults": 2,
        "Resources": [
            {
                "id": SCIM_USER,
                "name": "User",
                "description": "An Omnion account: userName is the e-mail address, active mirrors the account status.",
                "attributes": [
                    { "name": "userName", "type": "string", "required": true, "uniqueness": "server" },
                    { "name": "displayName", "type": "string" },
                    { "name": "externalId", "type": "string" },
                    { "name": "active", "type": "boolean" },
                ],
            },
            {
                "id": SCIM_GROUP,
                "name": "Group",
                "description": "A group (team) of the organization; members are account ids.",
                "attributes": [
                    { "name": "displayName", "type": "string", "required": true },
                    { "name": "members", "type": "complex", "multiValued": true },
                ],
            },
        ],
    }))
}

/// Query of a list endpoint.
#[derive(Debug, Deserialize)]
pub struct ScimListQuery {
    /// Attribute filter (`userName eq "…"` / `displayName eq "…"` / `externalId eq "…"`).
    #[serde(default)]
    pub filter: Option<String>,
    /// 1-based index of the first row.
    #[serde(default, rename = "startIndex")]
    pub start_index: Option<i64>,
    /// Page size.
    #[serde(default)]
    pub count: Option<i64>,
}

/// A parsed `attribute eq "value"` filter.
struct ScimFilter {
    attribute: String,
    value: String,
}

/// Parse the small filter surface this endpoint supports.
fn parse_filter(raw: Option<&str>) -> Result<Option<ScimFilter>, ScimError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    // `attribute eq "value"` — attribute names are case-insensitive, values keep their case.
    let mut parts = raw.splitn(3, char::is_whitespace);
    let attribute = parts.next().unwrap_or_default().to_ascii_lowercase();
    let operator = parts.next().unwrap_or_default().to_ascii_lowercase();
    let value = parts.next().unwrap_or_default().trim();

    let value = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(|| {
            ScimError::bad_request(
                "invalidFilter",
                "the filter value must be quoted (`userName eq \"ada@example.com\"`)",
            )
        })?;

    if operator != "eq" {
        return Err(ScimError::bad_request(
            "invalidFilter",
            format!("only `eq` is supported, not {operator:?}"),
        ));
    }
    if !matches!(
        attribute.as_str(),
        "username" | "externalid" | "displayname"
    ) {
        return Err(ScimError::bad_request(
            "invalidFilter",
            format!(
                "only userName, externalId and displayName can be filtered on, not {attribute:?}"
            ),
        ));
    }

    Ok(Some(ScimFilter {
        attribute,
        value: value.to_owned(),
    }))
}

/// A user document sent by a client.
#[derive(Debug, Deserialize)]
pub struct ScimUserPayload {
    /// `userName` — the e-mail address.
    #[serde(default, rename = "userName")]
    pub user_name: Option<String>,
    /// `displayName`.
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
    /// `name.formatted` (when the client sends the structured name instead).
    #[serde(default)]
    pub name: Option<ScimName>,
    /// `externalId` — the provider's own id for this person.
    #[serde(default, rename = "externalId")]
    pub external_id: Option<String>,
    /// `active`; absent means active.
    #[serde(default)]
    pub active: Option<bool>,
    /// `emails` — used as a fallback for `userName`.
    #[serde(default)]
    pub emails: Option<Vec<ScimEmail>>,
}

/// The structured name of a user document.
#[derive(Debug, Deserialize)]
pub struct ScimName {
    /// Full formatted name.
    #[serde(default)]
    pub formatted: Option<String>,
    /// Given name.
    #[serde(default, rename = "givenName")]
    pub given_name: Option<String>,
    /// Family name.
    #[serde(default)]
    pub family_name: Option<String>,
}

/// One e-mail entry of a user document.
#[derive(Debug, Deserialize)]
pub struct ScimEmail {
    /// The address.
    #[serde(default)]
    pub value: Option<String>,
    /// Primary flag.
    #[serde(default)]
    pub primary: Option<bool>,
}

impl ScimUserPayload {
    /// The address this document is about, from `userName` or the primary e-mail.
    fn email(&self) -> Option<String> {
        if let Some(user_name) = self.user_name.as_deref().map(str::trim) {
            if !user_name.is_empty() {
                return Some(user_name.to_owned());
            }
        }
        let emails = self.emails.as_ref()?;
        let primary = emails
            .iter()
            .find(|email| email.primary == Some(true))
            .or_else(|| emails.first())?;
        primary.value.as_deref().map(str::trim).map(str::to_owned)
    }

    /// The display name this document carries, from any of the three places it can sit.
    fn display_name(&self) -> Option<String> {
        if let Some(display_name) = self.display_name.as_deref().map(str::trim) {
            if !display_name.is_empty() {
                return Some(display_name.to_owned());
            }
        }
        let name = self.name.as_ref()?;
        if let Some(formatted) = name.formatted.as_deref().map(str::trim) {
            if !formatted.is_empty() {
                return Some(formatted.to_owned());
            }
        }
        match (
            name.given_name.as_deref().map(str::trim),
            name.family_name.as_deref().map(str::trim),
        ) {
            (Some(given), Some(family)) if !given.is_empty() && !family.is_empty() => {
                Some(format!("{given} {family}"))
            }
            (Some(given), _) if !given.is_empty() => Some(given.to_owned()),
            (_, Some(family)) if !family.is_empty() => Some(family.to_owned()),
            _ => None,
        }
    }
}

/// A `PatchOp` body.
#[derive(Debug, Deserialize)]
pub struct ScimPatch {
    /// Schema URNs the client sent; the PatchOp URN is required when the list is present.
    #[serde(default)]
    pub schemas: Vec<String>,
    /// Operations to apply, in order. SCIM clients send the key as `Operations`.
    #[serde(default, alias = "Operations")]
    pub operations: Vec<ScimOperation>,
}

impl ScimPatch {
    /// Refuse a document that declares itself as something other than a PatchOp.
    fn check_schemas(&self) -> Result<(), ScimError> {
        if self.schemas.is_empty() {
            return Ok(());
        }
        if self.schemas.iter().any(|schema| schema == SCIM_PATCH) {
            return Ok(());
        }
        Err(ScimError::bad_request(
            "invalidSyntax",
            format!("`schemas` must name {SCIM_PATCH}"),
        ))
    }
}

/// One patch operation.
#[derive(Debug, Deserialize)]
pub struct ScimOperation {
    /// `add`, `replace` or `remove` (case-insensitive).
    pub op: String,
    /// Attribute path, when the value is scalar.
    #[serde(default)]
    pub path: Option<String>,
    /// New value: a scalar for a path, an object when the path is absent.
    #[serde(default)]
    pub value: Option<Value>,
}

/// A group document sent by a client.
#[derive(Debug, Deserialize)]
pub struct ScimGroupPayload {
    /// `displayName`.
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
    /// Members, each carrying a user id in `value`.
    #[serde(default)]
    pub members: Option<Vec<ScimMember>>,
}

/// One member entry of a group document.
#[derive(Debug, Deserialize)]
pub struct ScimMember {
    /// Account id.
    #[serde(default)]
    pub value: Option<String>,
    /// `display` — informational, ignored.
    #[serde(default)]
    pub display: Option<String>,
}

/// Read the member ids out of a member list, refusing anything that is not a uuid.
fn member_ids(members: &[ScimMember]) -> Result<Vec<Uuid>, ScimError> {
    let mut ids = Vec::new();
    for member in members {
        let raw = member.value.as_deref().map(str::trim).unwrap_or_default();
        let id = Uuid::parse_str(raw).map_err(|_| {
            ScimError::bad_request("invalidValue", format!("{raw:?} is not an account id"))
        })?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Record that a group write changed who belongs to it.
///
/// The event is the whole point of a group-based role rule: a `when_group` rule grants its role
/// through `group_members`, so a person who joins a directory group has their effective
/// permissions change **without any sign-in happening**. Automation (REQ-003) and the security
/// centre subscribe to this to re-evaluate a subject, and a cached rule result has to be dropped
/// when it fires — otherwise a revoked group still reads as granted until the next sign-in.
///
/// Two decisions, each with a test saying so:
///
/// * **It fires on a change, not on a write.** A PATCH that re-sends the same member list
///   changes nothing, and an event for it would train a subscriber to ignore the name. A
///   connector re-sending a full group on a timer would otherwise produce a fire every time.
/// * **The counts are the diff, not the size.** `added`/`removed` are how many people crossed in
///   each direction; `members` alone would make a subscriber that only cares about removals
///   diff two snapshots itself, and a snapshot has no "before".
///
/// The payload names ids and counts only. A group's member list is the directory's most
/// personal export, and an event is delivered to subscribers outside the tenant.
async fn announce_membership(
    state: &AppState,
    organization_id: Uuid,
    group: &groups::Group,
    before: &[Uuid],
    after: &[Uuid],
) {
    let added = after.iter().filter(|id| !before.contains(id)).count();
    let removed = before.iter().filter(|id| !after.contains(id)).count();
    if added == 0 && removed == 0 {
        return;
    }

    if let Err(error) = omnion_events::bus::emit(
        state.db().pool(),
        omnion_events::NewEvent::new("iam.group_membership_synced")
            .organization(Some(organization_id))
            .payload(json!({
                "group_id": group.id,
                "group_name": group.name,
                "added": added,
                "removed": removed,
                "members": after.len(),
            })),
    )
    .await
    {
        tracing::warn!(error = %error, "the group membership event could not be recorded");
    }
}

/// The SCIM document of one account.
fn user_json(user: &users::User, external_id: Option<&str>, location_base: &str) -> Value {
    json!({
        "schemas": [SCIM_USER],
        "id": user.id,
        "externalId": external_id,
        "userName": user.email,
        "name": { "formatted": user.display_name },
        "displayName": user.display_name,
        "active": user.status == "active",
        "meta": {
            "resourceType": "User",
            "created": user.created_at.format(&Rfc3339).unwrap_or_default(),
            "location": format!("{location_base}/Users/{}", user.id),
        },
    })
}

/// The SCIM document of one group.
fn group_json(
    group: &groups::Group,
    members: &[groups::GroupMember],
    location_base: &str,
) -> Value {
    json!({
        "schemas": [SCIM_GROUP],
        "id": group.id,
        "displayName": group.name,
        "meta": {
            "resourceType": "Group",
            "created": group.created_at.format(&Rfc3339).unwrap_or_default(),
            "lastModified": group.updated_at.format(&Rfc3339).unwrap_or_default(),
            "location": format!("{location_base}/Groups/{}", group.id),
        },
        "members": members.iter().map(|member| json!({
            "value": member.user_id,
            "display": member.display_name,
            "type": "User",
        })).collect::<Vec<_>>(),
    })
}

/// The external id stored for an account, when a provider sent one.
async fn external_id_of(state: &AppState, user_id: Uuid) -> Result<Option<String>, ScimError> {
    let value: Option<String> =
        sqlx::query_scalar("select attributes ->> 'scim_external_id' from users where id = $1")
            .bind(user_id)
            .fetch_optional(state.db().pool())
            .await
            .map_err(internal)?
            .flatten();
    Ok(value)
}

/// Remember the provider's own id for an account.
async fn set_external_id(
    state: &AppState,
    user_id: Uuid,
    external_id: Option<&str>,
) -> Result<(), ScimError> {
    sqlx::query(
        "update users set attributes = attributes || jsonb_build_object('scim_external_id', $2::text) \
         where id = $1",
    )
    .bind(user_id)
    .bind(external_id)
    .execute(state.db().pool())
    .await
    .map_err(internal)?;
    Ok(())
}

/// Map any store error onto a `500` SCIM document (the client can do nothing about it).
fn internal(error: impl std::fmt::Display) -> ScimError {
    tracing::warn!(error = %error, "a SCIM operation failed");
    ScimError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        None,
        "the provisioning operation could not be completed",
    )
}

/// The base URL of this surface, as it appears in `meta.location`.
fn location_base(headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("https");
    format!("{scheme}://{host}/api/v1/scim/v2")
}

/// One line of the sync log as this module writes it.
struct SyncLine<'a> {
    /// `user` or `group`.
    resource: &'a str,
    /// What was attempted (`create`, `update`, `deactivate`, `delete`).
    action: &'a str,
    /// What happened.
    outcome: &'a str,
    /// Id the provider uses, when it sent one.
    external_id: Option<&'a str>,
    /// Our own id of the entity.
    entity_id: Option<Uuid>,
    /// One readable line.
    detail: String,
    /// Live sessions this line ended (`0126`).
    ///
    /// `None` is the default for every other write path, and `None` means the row records zero
    /// rather than "unknown" — see `NewSyncEntry::revoked_sessions`.
    revoked_sessions: Option<u64>,
}

/// Write one line of the sync log, ignoring a log failure (the operation itself already landed).
///
/// This is also where a provider-scoped push is folded into the run ledger (REQ-065, slice 4
/// part 3), and it is here rather than at fifteen call sites for the reason the log helper
/// exists: every SCIM outcome passes through this function, so a run cannot miss a line and
/// cannot double-count one.
///
/// A token that provisions a whole *organization* has no provider to hang a run off, and that is
/// not a degraded case — an organization-scoped connector is a legitimate configuration whose
/// work is answered by the provisioning screen it already appears on. Inventing a provider row
/// for it would put a sync ledger entry on a tenant that never configured a directory.
async fn log(state: &AppState, organization_id: Uuid, line: SyncLine<'_>) {
    // The detail is cloned rather than moved: `touch_run` reads it afterwards, and taking it by
    // reference here would make the struct partially moved, which the borrow checker refuses at
    // the next line. One sentence of duplication, and the alternative is a log entry and a run
    // entry that can say different things about the same failure.
    let entry = provisioning::NewSyncEntry {
        direction: "inbound".to_owned(),
        resource: line.resource.to_owned(),
        external_id: line.external_id.map(str::to_owned),
        entity_id: line.entity_id,
        action: line.action.to_owned(),
        outcome: line.outcome.to_owned(),
        detail: line.detail.clone(),
        revoked_sessions: line.revoked_sessions,
    };

    if let Err(error) = provisioning::log_sync(state.db().pool(), organization_id, &entry).await {
        tracing::warn!(error = %error, "the sync-log line could not be written");
        return;
    }

    touch_run(state, organization_id, &line).await;
}

/// Fold one logged line into the provider's open SCIM run, when there is a provider to fold it
/// into.
///
/// The action is namespaced as `scim/<action>` so the run's recount can tell a provisioning line
/// from anything else that may share the table, and so the log stays readable: `scim/create` says
/// which surface wrote the line, and a bare `create` would be ambiguous the moment a second
/// surface writes one.
async fn touch_run(state: &AppState, organization_id: Uuid, line: &SyncLine<'_>) {
    let provider: Option<Uuid> = sqlx::query_scalar(
        "select p.id from auth_providers p \
         where p.organization_id = $1 and p.kind in ('ldap', 'active_directory') \
           and exists (select 1 from provisioning_tokens t \
                       where t.organization_id = p.organization_id and t.revoked_at is null) \
         order by p.created_at limit 1",
    )
    .bind(organization_id)
    .fetch_optional(state.db().pool())
    .await
    .ok()
    .flatten();

    let Some(provider_id) = provider else { return };

    match scim_runs::current_run(state.db().pool(), provider_id, time::OffsetDateTime::now_utc())
        .await
    {
        Ok(Some(run_id)) => {
            if line.outcome == "failed" {
                // A failure has no retry button in the protocol, but it has an operator: the
                // person who configured the connector. Without this row the failure lives only
                // in a log line nobody reads, and a run that reports `error_count: 0` over three
                // refusals is a run lying by omission.
                if let Err(error) = scim_runs::record_failure(
                    state.db().pool(),
                    run_id,
                    line.external_id.unwrap_or(""),
                    "scim_refused",
                    &line.detail,
                )
                .await
                {
                    tracing::warn!(error = %error, "the SCIM run failure could not be recorded");
                }
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(error = %error, "the SCIM run could not be opened"),
    }
}

// ---------------------------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------------------------

/// `GET /scim/v2/Users`.
pub async fn list_users(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    Query(query): Query<ScimListQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ScimError> {
    let filter = parse_filter(query.filter.as_deref())?;
    let organization_id = principal.organization_id();

    let mut matches: Vec<(users::User, Option<String>)> = Vec::new();

    match &filter {
        Some(ScimFilter { attribute, value }) if attribute == "username" => {
            if let Some(user) = users::find_by_email(state.db().pool(), value)
                .await
                .map_err(internal)?
            {
                if user.organization_id == Some(organization_id) {
                    let external = external_id_of(&state, user.id).await?;
                    matches.push((user, external));
                }
            }
        }
        Some(ScimFilter { attribute, value }) if attribute == "externalid" => {
            let rows: Vec<(Uuid, String, String, String, time::OffsetDateTime)> = sqlx::query_as(
                "select id, email, display_name, status, created_at from users \
                 where organization_id = $1 and attributes ->> 'scim_external_id' = $2 \
                 order by created_at asc",
            )
            .bind(organization_id)
            .bind(value)
            .fetch_all(state.db().pool())
            .await
            .map_err(internal)?;
            for (id, email, display_name, status, created_at) in rows {
                matches.push((
                    users::User {
                        id,
                        organization_id: Some(organization_id),
                        email,
                        display_name,
                        status,
                        created_at,
                    },
                    Some(value.clone()),
                ));
            }
        }
        _ => {
            let rows: Vec<(
                Uuid,
                String,
                String,
                String,
                time::OffsetDateTime,
                Option<String>,
            )> = sqlx::query_as(
                "select id, email, display_name, status, created_at, \
                            attributes ->> 'scim_external_id' \
                     from users where organization_id = $1 order by created_at asc",
            )
            .bind(organization_id)
            .fetch_all(state.db().pool())
            .await
            .map_err(internal)?;
            for (id, email, display_name, status, created_at, external) in rows {
                matches.push((
                    users::User {
                        id,
                        organization_id: Some(organization_id),
                        email,
                        display_name,
                        status,
                        created_at,
                    },
                    external,
                ));
            }
        }
    }

    // `displayName eq` narrows after the read (the column is not indexed for it).
    if let Some(ScimFilter { attribute, value }) = &filter {
        if attribute == "displayname" {
            matches.retain(|(user, _)| user.display_name.eq_ignore_ascii_case(value));
        }
    }

    let total = matches.len() as i64;
    let start = query.start_index.unwrap_or(1).max(1) as usize - 1;
    let count = query.count.unwrap_or(100).clamp(1, MAX_PAGE) as usize;
    let base = location_base(&headers);

    let resources: Vec<Value> = matches
        .iter()
        .skip(start)
        .take(count)
        .map(|(user, external)| user_json(user, external.as_deref(), &base))
        .collect();

    Ok(Json(json!({
        "schemas": [SCIM_LIST],
        "totalResults": total,
        "startIndex": start + 1,
        "itemsPerPage": resources.len(),
        "Resources": resources,
    })))
}

/// `POST /scim/v2/Users`.
pub async fn create_user(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    headers: HeaderMap,
    Json(payload): Json<ScimUserPayload>,
) -> Result<Response, ScimError> {
    let organization_id = principal.organization_id();

    let email = payload.email().ok_or_else(|| {
        ScimError::bad_request(
            "invalidValue",
            "`userName` (or a primary e-mail) is required",
        )
    })?;

    let display_name = payload
        .display_name()
        .unwrap_or_else(|| email.split('@').next().unwrap_or(&email).to_owned());

    // An address that already exists inside this organization is the same person: answer their
    // document instead of failing the sync (idempotent create, as the RFC intends).
    if let Some(existing) = users::find_by_email(state.db().pool(), &email)
        .await
        .map_err(internal)?
    {
        if existing.organization_id == Some(organization_id) {
            if let Some(external_id) = payload.external_id.as_deref() {
                set_external_id(&state, existing.id, Some(external_id)).await?;
            }
            let external = external_id_of(&state, existing.id).await?;
            log(
                &state,
                organization_id,
                SyncLine {
                    resource: "user",
                    action: "create",
                    outcome: "skipped",
                    external_id: payload.external_id.as_deref(),
                    entity_id: Some(existing.id),
                    detail: format!("{} already exists in this organization", existing.email),
                    revoked_sessions: None,
                },
            )
            .await;
            return Ok((
                StatusCode::OK,
                Json(user_json(
                    &existing,
                    external.as_deref(),
                    &location_base(&headers),
                )),
            )
                .into_response());
        }
    }

    // A provisioned account has no password of its own: it signs in through the directory, so
    // the stored secret is a random value nobody ever sees.
    let password = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());

    let created = users::create_user(
        state.db().pool(),
        users::NewUser {
            email: email.clone(),
            password,
            display_name: display_name.clone(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .map_err(|error| match error {
        omnion_identity::IdentityError::EmailTaken => ScimError::new(
            StatusCode::CONFLICT,
            Some("uniqueness"),
            format!("{email} is already registered"),
        ),
        omnion_identity::IdentityError::InvalidEmail(message) => {
            ScimError::bad_request("invalidValue", format!("{email:?}: {message}"))
        }
        other => internal(other),
    })?;

    if let Some(external_id) = payload.external_id.as_deref() {
        set_external_id(&state, created.id, Some(external_id)).await?;
    }

    if payload.active == Some(false) {
        // A connector may create an account already deactivated. The status setter that revokes
        // is the one used here, not the display-only one: a brand-new account has no sessions,
        // but "it has none today" is not a property that stays true, and this write path is the
        // same one that deactivates a live account further down.
        users::set_status_and_end_sessions(state.db().pool(), created.id, "disabled", "scim_deactivated")
            .await
            .map_err(internal)?;
    }

    let user = users::find_by_id(state.db().pool(), created.id)
        .await
        .map_err(internal)?
        .unwrap_or(created);
    let external = external_id_of(&state, user.id).await?;

    log(
        &state,
        organization_id,
        SyncLine {
            resource: "user",
            action: "create",
            outcome: "created",
            external_id: payload.external_id.as_deref(),
            entity_id: Some(user.id),
            detail: format!("{} was provisioned", user.email),
            revoked_sessions: None,
        },
    )
    .await;

    let base = location_base(&headers);
    let mut response = (
        StatusCode::CREATED,
        Json(user_json(&user, external.as_deref(), &base)),
    )
        .into_response();
    if let Ok(value) = format!("{base}/Users/{}", user.id).parse() {
        response
            .headers_mut()
            .insert(axum::http::header::LOCATION, value);
    }
    Ok(response)
}

/// Load an account the token's organization owns.
async fn require_user(
    state: &AppState,
    principal: &ProvisioningPrincipal,
    id: Uuid,
) -> Result<users::User, ScimError> {
    let user = users::find_by_id(state.db().pool(), id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ScimError::not_found(format!("no account {id}")))?;

    if user.organization_id != Some(principal.organization_id()) {
        return Err(ScimError::not_found(format!("no account {id}")));
    }
    Ok(user)
}

/// `GET /scim/v2/Users/{id}`.
pub async fn get_user(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ScimError> {
    let user_id =
        Uuid::parse_str(&id).map_err(|_| ScimError::not_found(format!("no account {id}")))?;
    let user = require_user(&state, &principal, user_id).await?;
    let external = external_id_of(&state, user.id).await?;

    Ok(Json(user_json(
        &user,
        external.as_deref(),
        &location_base(&headers),
    )))
}

/// `PUT /scim/v2/Users/{id}` — replace the properties this endpoint models.
pub async fn replace_user(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<ScimUserPayload>,
) -> Result<Json<Value>, ScimError> {
    let user_id =
        Uuid::parse_str(&id).map_err(|_| ScimError::not_found(format!("no account {id}")))?;
    let user = require_user(&state, &principal, user_id).await?;

    let updated = apply_user_changes(&state, &principal, &user, &payload, "replace").await?;
    let external = external_id_of(&state, updated.id).await?;

    Ok(Json(user_json(
        &updated,
        external.as_deref(),
        &location_base(&headers),
    )))
}

/// Apply a user document's changes and log the outcome; the shared tail of PUT and PATCH.
async fn apply_user_changes(
    state: &AppState,
    principal: &ProvisioningPrincipal,
    user: &users::User,
    payload: &ScimUserPayload,
    action: &str,
) -> Result<users::User, ScimError> {
    let organization_id = principal.organization_id();

    if let Some(display_name) = payload.display_name() {
        sqlx::query("update users set display_name = $2 where id = $1")
            .bind(user.id)
            .bind(&display_name)
            .execute(state.db().pool())
            .await
            .map_err(internal)?;
    }

    if let Some(external_id) = payload.external_id.as_deref() {
        set_external_id(state, user.id, Some(external_id)).await?;
    }

    let mut outcome = "updated";
    let mut revoked_sessions = 0_u64;
    if let Some(active) = payload.active {
        let status = if active { "active" } else { "disabled" };
        if status != user.status {
            // The revoking setter, because this is the path a directory takes a *live* person
            // out of service on, and `resolve_session` reading `status = 'active'` is a filter,
            // not a revocation: the tokens stay in the browser and stop working again the moment
            // a later sync re-activates the account. See `users::set_status_and_end_sessions`.
            let change = users::set_status_and_end_sessions(
                state.db().pool(),
                user.id,
                status,
                "scim_deactivated",
            )
            .await
            .map_err(internal)?
            .ok_or_else(|| ScimError::not_found(format!("no account {}", user.id)))?;
            revoked_sessions = change.1.revoked_sessions;
            if !active {
                outcome = "deactivated";
            }
        }
    }

    let updated = users::find_by_id(state.db().pool(), user.id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ScimError::not_found(format!("no account {}", user.id)))?;

    log(
        state,
        organization_id,
        SyncLine {
            resource: "user",
            action,
            outcome,
            external_id: payload.external_id.as_deref(),
            entity_id: Some(updated.id),
            detail: if revoked_sessions > 0 {
                // The count is in the log because it is the number an operator needs and cannot
                // get anywhere else: "deactivated" says the account is out, and this says how
                // many live tokens went with it. A directory that reports one deactivated account
                // while three people are still holding working sessions is the failure this
                // slice removes, and it is invisible from the account row alone.
                format!(
                    "{} now reads {} — {revoked_sessions} session(s) ended",
                    updated.email, updated.status
                )
            } else {
                format!("{} now reads {}", updated.email, updated.status)
            },
            // The sentence below is for a reader; this is the same number as a value, so the
            // panel can sum a run, sort the log, or badge the row instead of every consumer
            // re-parsing English to find out how many sessions an offboarding ended.
            revoked_sessions: Some(revoked_sessions),
        },
    )
    .await;

    Ok(updated)
}

/// `PATCH /scim/v2/Users/{id}` — a `PatchOp` with `replace`, `add` and `remove`.
pub async fn patch_user(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(patch): Json<ScimPatch>,
) -> Result<Json<Value>, ScimError> {
    let user_id =
        Uuid::parse_str(&id).map_err(|_| ScimError::not_found(format!("no account {id}")))?;
    let user = require_user(&state, &principal, user_id).await?;

    patch.check_schemas()?;

    let mut display_name: Option<String> = None;
    let mut external_id: Option<String> = None;
    let mut active: Option<bool> = None;

    for operation in &patch.operations {
        let op = operation.op.to_ascii_lowercase();
        if !matches!(op.as_str(), "add" | "replace" | "remove") {
            return Err(ScimError::bad_request(
                "invalidSyntax",
                format!("unknown patch op {:?}", operation.op),
            ));
        }

        let path = operation
            .path
            .as_deref()
            .map(str::trim)
            .map(|value| value.to_ascii_lowercase());

        match (path.as_deref(), op.as_str()) {
            (Some("active"), "remove") => active = Some(false),
            (Some("active"), _) => {
                active = Some(
                    operation
                        .value
                        .as_ref()
                        .and_then(Value::as_bool)
                        .ok_or_else(|| {
                            ScimError::bad_request("invalidValue", "`active` needs a boolean value")
                        })?,
                );
            }
            (Some("displayname") | Some("name.formatted"), "remove") => {
                display_name = Some(String::new());
            }
            (Some("displayname") | Some("name.formatted"), _) => {
                display_name = Some(
                    operation
                        .value
                        .as_ref()
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ScimError::bad_request(
                                "invalidValue",
                                "`displayName` needs a string value",
                            )
                        })?
                        .to_owned(),
                );
            }
            (Some("externalid"), "remove") => external_id = Some(String::new()),
            (Some("externalid"), _) => {
                external_id = Some(
                    operation
                        .value
                        .as_ref()
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ScimError::bad_request("invalidValue", "`externalId` needs a string")
                        })?
                        .to_owned(),
                );
            }
            (Some("username"), "remove") => {
                return Err(ScimError::bad_request(
                    "invalidValue",
                    "`userName` cannot be removed",
                ));
            }
            (Some("username"), _) => {
                let new_email = operation
                    .value
                    .as_ref()
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ScimError::bad_request("invalidValue", "`userName` needs a string")
                    })?;
                if !new_email.eq_ignore_ascii_case(&user.email) {
                    return Err(ScimError::bad_request(
                        "invalidValue",
                        "renaming an account's address is not supported by this endpoint",
                    ));
                }
            }
            // A value object without a path: read the attributes it carries.
            (None, _) => {
                if let Some(object) = operation.value.as_ref().and_then(Value::as_object) {
                    if let Some(value) = object.get("active").and_then(Value::as_bool) {
                        active = Some(value);
                    }
                    if let Some(value) = object.get("displayName").and_then(Value::as_str) {
                        display_name = Some(value.to_owned());
                    }
                    if let Some(value) = object.get("externalId").and_then(Value::as_str) {
                        external_id = Some(value.to_owned());
                    }
                    if let Some(name) = object.get("name").and_then(Value::as_object) {
                        if let Some(value) = name.get("formatted").and_then(Value::as_str) {
                            display_name = Some(value.to_owned());
                        }
                    }
                } else {
                    return Err(ScimError::bad_request(
                        "invalidSyntax",
                        "a patch operation without a path needs an object value",
                    ));
                }
            }
            (Some(other), _) => {
                return Err(ScimError::bad_request(
                    "invalidPath",
                    format!("this endpoint cannot patch {other:?}"),
                ));
            }
        }
    }

    let payload = ScimUserPayload {
        user_name: None,
        display_name,
        name: None,
        external_id,
        active,
        emails: None,
    };

    let updated = apply_user_changes(&state, &principal, &user, &payload, "update").await?;
    let external = external_id_of(&state, updated.id).await?;

    Ok(Json(user_json(
        &updated,
        external.as_deref(),
        &location_base(&headers),
    )))
}

/// `DELETE /scim/v2/Users/{id}` — deactivate (the SCIM default), never delete.
pub async fn delete_user(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    Path(id): Path<String>,
) -> Result<StatusCode, ScimError> {
    let user_id =
        Uuid::parse_str(&id).map_err(|_| ScimError::not_found(format!("no account {id}")))?;
    let user = require_user(&state, &principal, user_id).await?;

    // SCIM's delete is a deactivation: the row stays, so the catalogue calls it what it is.
    // `user.deleted` is the name a receiver of the platform's own bus reads \u2014 the account is
    // no longer usable \u2014 and it is emitted here because this is the one write that takes an
    // account out of service on an identity provider's instruction.
    omnion_events::bus::emit(
        state.db().pool(),
        omnion_events::NewEvent::new("user.deleted")
            .organization(principal.organization_id())
            .payload(json!({
                "user_id": user.id,
                "email": user.email,
                "source": "scim",
            })),
    )
    .await
    .map_err(internal)?;

    // The `?` and the `.ok_or_else` are the same handling the PATCH path above uses, and for the
    // same reason: the account was read a few lines ago, so a row that has since disappeared is
    // not a client error to be reported as "no such user" on a DELETE that already found one —
    // but it is not a 500 either, and silently logging "deactivated" for an account this call
    // never touched would be a log line about nothing.
    let deactivate = users::set_status_and_end_sessions(
        state.db().pool(),
        user.id,
        "disabled",
        "scim_deactivated",
    )
    .await
    .map_err(internal)?
    .ok_or_else(|| ScimError::not_found(format!("no account {}", user.id)))?;

    log(
        &state,
        principal.organization_id(),
        SyncLine {
            resource: "user",
            action: "deactivate",
            outcome: "deactivated",
            external_id: None,
            entity_id: Some(user.id),
            // DELETE is what connectors send last for somebody who left, and it is the one path
            // where "the account is disabled" is the whole meaning. Ending the sessions is not an
            // extra nicety here: a DELETE that left the tokens working would hand a departed
            // employee's browser a working session until the token expired on its own.
            detail: format!(
                "{} was deactivated (the SCIM default — the account stays, its sessions do not)",
                user.email
            ),
            // The count is reported as a number here because `detail` is prose: this line
            // is the only record of how many live tokens went with a departed colleague, and
            // "the account is disabled" does not say whether the tokens in their browser
            // still work. An offboarding that disabled the account without this figure is the
            // failure the number exists to make visible.
            revoked_sessions: Some(deactivate.1.revoked_sessions),
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------------------------

/// `GET /scim/v2/Groups`.
pub async fn list_groups(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    Query(query): Query<ScimListQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ScimError> {
    let filter = parse_filter(query.filter.as_deref())?;
    if let Some(ScimFilter { attribute, .. }) = &filter {
        if attribute == "username" || attribute == "externalid" {
            return Err(ScimError::bad_request(
                "invalidFilter",
                "groups can only be filtered on displayName",
            ));
        }
    }

    let organization_id = principal.organization_id();
    let summaries = groups::list(state.db().pool(), organization_id)
        .await
        .map_err(internal)?;

    let base = location_base(&headers);
    let mut resources = Vec::new();
    for summary in summaries {
        if let Some(ScimFilter { attribute, value }) = &filter {
            if attribute == "displayname" && !summary.group.name.eq_ignore_ascii_case(value) {
                continue;
            }
        }
        let members = groups::list_members(state.db().pool(), summary.group.id)
            .await
            .map_err(internal)?;
        resources.push(group_json(&summary.group, &members, &base));
    }

    Ok(Json(json!({
        "schemas": [SCIM_LIST],
        "totalResults": resources.len(),
        "startIndex": 1,
        "itemsPerPage": resources.len(),
        "Resources": resources,
    })))
}

/// Find a group by its display name inside the token's organization.
async fn find_group_by_name(
    state: &AppState,
    organization_id: Uuid,
    name: &str,
) -> Result<Option<groups::Group>, ScimError> {
    let summaries = groups::list(state.db().pool(), organization_id)
        .await
        .map_err(internal)?;
    Ok(summaries
        .into_iter()
        .map(|summary| summary.group)
        .find(|group| group.name.eq_ignore_ascii_case(name)))
}

/// `POST /scim/v2/Groups` — create, or answer the group that already carries the name.
pub async fn create_group(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    headers: HeaderMap,
    Json(payload): Json<ScimGroupPayload>,
) -> Result<Response, ScimError> {
    let organization_id = principal.organization_id();
    let name = payload
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ScimError::bad_request("invalidValue", "`displayName` is required"))?
        .to_owned();

    let ids = member_ids(payload.members.as_deref().unwrap_or(&[]))?;

    let (group, outcome) = match find_group_by_name(&state, organization_id, &name).await? {
        Some(existing) => (existing, "skipped"),
        None => {
            let created = groups::create(
                state.db().pool(),
                groups::NewGroup {
                    organization_id,
                    name: name.clone(),
                    description: "Provisioned by the directory (SCIM).".to_owned(),
                },
            )
            .await
            .map_err(internal)?;
            (created, "created")
        }
    };

    if !ids.is_empty() {
        groups::replace_members(state.db().pool(), group.id, &ids, None)
            .await
            .map_err(internal)?;
    }

    let members = groups::list_members(state.db().pool(), group.id)
        .await
        .map_err(internal)?;

    // A group created *with* members has just changed that group's membership, so the event
    // fires here with nothing as the "before". A group whose members arrive with it is the
    // case a `when_group` rule is most often waiting for, and it is the one a create-only
    // listener would otherwise miss entirely.
    if !ids.is_empty() {
        let now: Vec<Uuid> = members.iter().map(|member| member.user_id).collect();
        announce_membership(&state, organization_id, &group, &[], &now).await;
    }

    log(
        &state,
        organization_id,
        SyncLine {
            resource: "group",
            action: "create",
            outcome,
            external_id: None,
            entity_id: Some(group.id),
            detail: format!("{} now has {} member(s)", group.name, members.len()),
            revoked_sessions: None,
        },
    )
    .await;

    let base = location_base(&headers);
    let status = if outcome == "created" {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let mut response = (status, Json(group_json(&group, &members, &base))).into_response();
    if let Ok(value) = format!("{base}/Groups/{}", group.id).parse() {
        response
            .headers_mut()
            .insert(axum::http::header::LOCATION, value);
    }
    Ok(response)
}

/// Load a group the token's organization owns.
async fn require_group(
    state: &AppState,
    principal: &ProvisioningPrincipal,
    id: Uuid,
) -> Result<groups::Group, ScimError> {
    let group = groups::find(state.db().pool(), id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ScimError::not_found(format!("no group {id}")))?;

    if group.organization_id != principal.organization_id() {
        return Err(ScimError::not_found(format!("no group {id}")));
    }
    Ok(group)
}

/// `GET /scim/v2/Groups/{id}`.
pub async fn get_group(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ScimError> {
    let group_id =
        Uuid::parse_str(&id).map_err(|_| ScimError::not_found(format!("no group {id}")))?;
    let group = require_group(&state, &principal, group_id).await?;
    let members = groups::list_members(state.db().pool(), group.id)
        .await
        .map_err(internal)?;

    Ok(Json(group_json(&group, &members, &location_base(&headers))))
}

/// `PATCH /scim/v2/Groups/{id}` — `add`, `remove` and `replace` over the member list.
pub async fn patch_group(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(patch): Json<ScimPatch>,
) -> Result<Json<Value>, ScimError> {
    let group_id =
        Uuid::parse_str(&id).map_err(|_| ScimError::not_found(format!("no group {id}")))?;
    let group = require_group(&state, &principal, group_id).await?;

    patch.check_schemas()?;

    let current: Vec<Uuid> = groups::list_members(state.db().pool(), group.id)
        .await
        .map_err(internal)?
        .into_iter()
        .map(|member| member.user_id)
        .collect();

    let mut next = current.clone();
    let mut action = "update";

    for operation in &patch.operations {
        let op = operation.op.to_ascii_lowercase();
        if !matches!(op.as_str(), "add" | "replace" | "remove") {
            return Err(ScimError::bad_request(
                "invalidSyntax",
                format!("unknown patch op {:?}", operation.op),
            ));
        }

        let path = operation
            .path
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase);

        match path.as_deref() {
            Some("members") | None => {}
            Some(other) => {
                return Err(ScimError::bad_request(
                    "invalidPath",
                    format!("this endpoint cannot patch {other:?}"),
                ));
            }
        }

        // The members sit either in `value` (an array of member objects) or in a `members` key
        // of a value object — both shapes appear in the wild.
        let member_values: Vec<ScimMember> = match operation.value.as_ref() {
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    serde_json::from_value::<ScimMember>(item.clone()).map_err(|_| {
                        ScimError::bad_request("invalidValue", "a member needs a `value` id")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(Value::Object(object)) => object
                .get("members")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            serde_json::from_value::<ScimMember>(item.clone()).map_err(|_| {
                                ScimError::bad_request(
                                    "invalidValue",
                                    "a member needs a `value` id",
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default(),
            _ => {
                return Err(ScimError::bad_request(
                    "invalidValue",
                    "a member operation needs a value",
                ));
            }
        };

        let ids = member_ids(&member_values)?;
        match op.as_str() {
            "add" => {
                for id in ids {
                    if !next.contains(&id) {
                        next.push(id);
                    }
                }
            }
            "replace" => {
                next = ids;
                action = "update";
            }
            "remove" => {
                next.retain(|id| !ids.contains(id));
                action = "update";
            }
            _ => unreachable!("the op was checked above"),
        }
    }

    groups::replace_members(state.db().pool(), group.id, &next, None)
        .await
        .map_err(internal)?;

    let members = groups::list_members(state.db().pool(), group.id)
        .await
        .map_err(internal)?;

    // The diff is computed from the member list read *before* the write against the one read
    // after, so the event counts the people who actually crossed the boundary in this request
    // rather than the size of the group. `current` is the pre-image the patch already had in
    // hand; comparing against a fresh read would make a concurrent write look like this one.
    let applied: Vec<Uuid> = members.iter().map(|member| member.user_id).collect();
    announce_membership(&state, principal.organization_id(), &group, &current, &applied).await;

    log(
        &state,
        principal.organization_id(),
        SyncLine {
            resource: "group",
            action,
            outcome: "updated",
            external_id: None,
            entity_id: Some(group.id),
            detail: format!("{} now has {} member(s)", group.name, members.len()),
            revoked_sessions: None,
        },
    )
    .await;

    Ok(Json(group_json(&group, &members, &location_base(&headers))))
}

/// `DELETE /scim/v2/Groups/{id}`.
pub async fn delete_group(
    State(state): State<AppState>,
    principal: ProvisioningPrincipal,
    Path(id): Path<String>,
) -> Result<StatusCode, ScimError> {
    let group_id =
        Uuid::parse_str(&id).map_err(|_| ScimError::not_found(format!("no group {id}")))?;
    let group = require_group(&state, &principal, group_id).await?;

    groups::delete(state.db().pool(), group.id)
        .await
        .map_err(internal)?;

    log(
        &state,
        principal.organization_id(),
        SyncLine {
            resource: "group",
            action: "delete",
            outcome: "deactivated",
            external_id: None,
            entity_id: Some(group.id),
            detail: format!("{} was removed", group.name),
            revoked_sessions: None,
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}
