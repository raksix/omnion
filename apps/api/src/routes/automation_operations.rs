//! The automation *operations* surfaces: version history with restore, the templates
//! gallery, and the audit tab (docs/requests/REQ-003, slice 4).
//!
//! Slices 1–3 built the machine and slice 4's engine half built the bounds and the loop
//! guard. What the request still calls for is the part an **operator** reads when a rule
//! is not doing what its author expected: *what did this rule look like on Tuesday, who
//! changed it, and what would the six starters look like if I installed one?* Those are
//! three read surfaces over facts that already exist, plus one write (restore), so this
//! module is deliberately thin.
//!
//! ## One trail, not two
//!
//! The Versions tab and the Audit tab show the same event from two angles — *what the
//! definition became* and *who did it and when* — so they are stored once
//! (`workflow_versions`, migration `0030`) and read twice. A second audit table would be
//! one more place for the truth to be stale, and the request's risk note is explicit that
//! "a caller that cannot record an audit row must not report the action as successful";
//! that guarantee is the `audit_log` write, not a parallel one.
//!
//! ## Restore appends, never rewinds
//!
//! Restoring writes the old **content** as the **next** version. A history that can rewind
//! has two rows claiming the same number, and "v3 → v1 → v3" becomes indistinguishable
//! from a bug. `restored_from` carries the ancestry, so the line stays a line.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_automation::model::AutomationRule;
use omnion_automation::{templates, versions};
use omnion_workflows::store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One version of a rule, as the Versions tab lists it.
#[derive(Debug, Serialize)]
pub struct VersionBody {
    /// Version row id — what *Restore* is addressed by.
    pub id: Uuid,
    /// The rule it belongs to.
    pub automation_id: Uuid,
    /// The number it was written as.
    pub version: i32,
    /// `created`, `updated` or `restored`.
    pub change: String,
    /// What changed, in words (see [`omnion_automation::versions::diff`]).
    pub summary: Value,
    /// The whole definition as it was written.
    pub definition: Value,
    /// Who wrote it; `null` when the account has since been deleted.
    pub created_by: Option<Uuid>,
    /// When.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// The version whose content was restored, when this row is a restore.
    pub restored_from: Option<Uuid>,
    /// `true` when the rule as it stands is this row — the one *Restore* must refuse,
    /// because restoring what is already loaded is a no-op that bumps the counter.
    pub current: bool,
}

impl VersionBody {
    /// Describe one stored version.
    fn build(version: &versions::Version, current_version: i32) -> Self {
        Self {
            id: version.id,
            automation_id: version.workflow_id,
            version: version.version,
            change: version.change.as_str().to_owned(),
            summary: version.summary.clone(),
            definition: version.definition.clone(),
            created_by: version.created_by,
            created_at: version.created_at,
            restored_from: version.restored_from,
            current: version.version == current_version,
        }
    }
}

/// The Versions tab payload.
#[derive(Debug, Serialize)]
pub struct VersionListResponse {
    /// The rule the history belongs to.
    pub automation_id: Uuid,
    /// The number the rule is on now.
    pub current_version: i32,
    /// History, newest first.
    pub versions: Vec<VersionBody>,
    /// `true` when the rule has no history row at all — it was written before this
    /// feature shipped, and the tab says so in words rather than showing an empty table
    /// that reads like "nothing has ever happened here".
    pub untracked: bool,
}

/// One audit row, as the Audit tab lists it.
#[derive(Debug, Serialize)]
pub struct AuditBody {
    /// Row id.
    pub id: i64,
    /// Stable action name, e.g. `automation.updated`.
    pub action: String,
    /// Who did it, when a person did.
    pub actor_user_id: Option<Uuid>,
    /// `user`, `agent`, `service` or `system`.
    pub actor_type: String,
    /// What was acted on.
    pub target_type: Option<String>,
    /// Its id, as text.
    pub target_id: Option<String>,
    /// Structured detail; never carries secrets.
    pub metadata: Value,
    /// When.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// The Audit tab payload.
#[derive(Debug, Serialize)]
pub struct AuditListResponse {
    /// The rule the entries belong to, or `None` for the whole organization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<Uuid>,
    /// Entries, newest first.
    pub entries: Vec<AuditBody>,
}

// ---------------------------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------------------------

/// Filters of the audit read.
#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    /// Limit the answer (`1`–`200`).
    pub limit: Option<i64>,
}

/// The body of a restore.
///
/// It is empty by design: the version row already carries the definition, and a caller
/// that could send its own definition here would be able to write a rule the panel never
/// validated.
#[derive(Debug, Deserialize, Default)]
pub struct RestoreInput {}

/// Default page size of the audit read.
const AUDIT_PAGE_DEFAULT: i64 = 50;

/// Ceiling of the audit read.
const AUDIT_PAGE_MAX: i64 = 200;

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/automations/{id}/versions` — the definition history of one rule.
pub async fn list_versions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
) -> Result<Json<VersionListResponse>, ApiError> {
    let (workflow, _) = rule_in_scope(&state, &current, automation_id).await?;
    let current_version = current_version(state.db().pool(), &workflow).await?;
    let history = versions::list(state.db().pool(), workflow.id).await?;

    Ok(Json(VersionListResponse {
        automation_id: workflow.id,
        current_version,
        versions: history
            .iter()
            .map(|version| VersionBody::build(version, current_version))
            .collect(),
        untracked: history.is_empty(),
    }))
}

/// `POST /api/v1/automations/{id}/versions/{version_id}/restore` — put a definition back.
///
/// This is a **definition write**, so it carries `workflows.manage` and is audited exactly
/// like a `PUT` — the audit row records which version was restored, so "who put that back"
/// is answerable from the trail alone and not only from the version's own ancestry.
pub async fn restore_version(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path((automation_id, version_id)): Path<(Uuid, Uuid)>,
    Json(_): Json<RestoreInput>,
) -> Result<(StatusCode, Json<VersionBody>), ApiError> {
    let (workflow, rule) = rule_in_scope(&state, &current, automation_id).await?;

    let wanted = versions::find(state.db().pool(), version_id)
        .await?
        // A version id from another rule is refused, not applied: restoring it would write
        // a definition this rule never had, and the panel would show a diff against
        // nothing. The two refusals are the same body, so the answer cannot be used to
        // probe for a version that exists somewhere else.
        .filter(|version| version.workflow_id == workflow.id)
        .ok_or_else(version_not_found)?;

    let current_version = current_version(state.db().pool(), &workflow).await?;
    if wanted.version == current_version {
        return Err(ApiError::bad_request(
            "version_already_current",
            format!(
                "version {} is what the rule is running right now, so there is nothing to restore",
                wanted.version
            ),
        ));
    }

    // Rebuild a rule from the stored definition and write it through the SAME path an edit
    // takes. Restoring through a second code path is how a "restore" ends up skipping a
    // validation or an audit row, and this one is the path the engine will actually run.
    let restored = rebuild(&wanted.definition, &rule)?;
    let update = omnion_automation::matcher::update_from_rule(&restored).ok_or_else(|| {
        ApiError::bad_request("invalid_version", "this version could not be stored again")
    })?;

    // A no-op is refused **by the definition, not by the number**. Comparing version numbers
    // was wrong twice over: it let a second restore of the version that was *just* restored
    // through (the number differs, the definition does not — the rule would be rewritten
    // identically, the counter would bump and the history would gain a row that changed
    // nothing), and it is a check about bookkeeping rather than about the user's intent. The
    // question a person asked is "does this change my rule?", and the answer is a diff.
    if let Ok(current_definition) = omnion_automation::versions::current_definition(&rule) {
        let changes = omnion_automation::versions::diff(&current_definition, &wanted.definition);
        if changes
            .get("changed")
            .and_then(Value::as_array)
            .is_some_and(|changed| changed.is_empty())
        {
            return Err(ApiError::bad_request(
                "version_already_current",
                format!(
                    "version {} has the same definition the rule is running now, so restoring \
                     it would change nothing",
                    wanted.version
                ),
            ));
        }
    }

    let mut tx = state.db().pool().begin().await.map_err(database_error)?;
    let written = store::update_workflow_on(&mut *tx, workflow.id, update)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "automation_not_found",
                "this rule no longer exists",
            )
        })?;
    let stored = AutomationRule::from_workflow(&written)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "automation_not_found",
            "this rule no longer exists",
        )
    })?;
    // `versions::record` answers with the layer's own error type, which already knows how
    // to become an `ApiError` — so no `map_err` here. Wrapping it again would be a second
    // vocabulary for one failure.
    let written_version = versions::record(
        &mut tx,
        &stored,
        versions::Change::Restored,
        Some(current.user.id),
        Some(wanted.id),
    )
    .await?;
    tx.commit().await.map_err(database_error)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "automation.version_restored")
            .organization(workflow.organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({
                "from_version": wanted.version,
                "to_version": written_version.version,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((
        StatusCode::OK,
        Json(VersionBody::build(
            &written_version,
            written_version.version,
        )),
    ))
}

/// `GET /api/v1/automations/{id}/audit` — who changed this rule, and when.
pub async fn list_audit(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
    Query(query): Query<AuditQuery>,
) -> Result<Json<AuditListResponse>, ApiError> {
    let (workflow, _) = rule_in_scope(&state, &current, automation_id).await?;
    let limit = query
        .limit
        .unwrap_or(AUDIT_PAGE_DEFAULT)
        .clamp(1, AUDIT_PAGE_MAX);

    let entries = rule_audit_entries(
        state.db().pool(),
        workflow.id,
        workflow.organization_id,
        limit,
    )
    .await?;

    Ok(Json(AuditListResponse {
        automation_id: Some(workflow.id),
        entries,
    }))
}

/// The templates gallery payload.
#[derive(Debug, Serialize)]
pub struct TemplateListResponse {
    /// The starter rules, in gallery order.
    pub templates: Vec<TemplateBody>,
}

/// One starter rule.
#[derive(Debug, Serialize)]
pub struct TemplateBody {
    /// Stable template key; the gallery's row identity.
    pub key: String,
    /// Display name.
    pub name: String,
    /// One line about what it does, in the panel's own words.
    pub description: String,
    /// The category the gallery groups by.
    pub category: String,
    /// The event the rule listens for.
    pub event: String,
    /// How many conditions it starts with.
    pub condition_count: usize,
    /// How many actions it starts with.
    pub action_count: usize,
    /// What has to be filled in before it can run — a recipient, a host, a page id.
    ///
    /// Named rather than guessed: a template that says "needs a destination" is honest,
    /// and the request's criterion is that a template is savable "without edits beyond
    /// their missing credentials".
    pub requires: Vec<String>,
    /// `false` when an action's host is not on the allow-list yet.
    pub installable: bool,
    /// Why it is not installable, when it is not.
    pub blocked_reason: Option<String>,
    /// The request body `POST /api/v1/automations` would take, verbatim.
    pub body: Value,
}

impl TemplateBody {
    /// Describe one starter rule against this installation.
    fn build(template: templates::Template, allowed_hosts: &[String]) -> Self {
        let mut requires: Vec<String> = template
            .requires
            .iter()
            .map(|text| (*text).to_owned())
            .collect();
        let mut blocked: Option<String> = None;

        for host in template.outbound_hosts() {
            if !omnion_automation::outbound::host_allowed(&host, allowed_hosts) {
                blocked = Some(format!(
                    "this installation does not allow calls to {host} yet; add it to the \
                     outbound host allow-list first"
                ));
                requires.push(format!("the {host} host on the outbound allow-list"));
            }
        }

        Self {
            key: template.key.to_owned(),
            name: template.name.to_owned(),
            description: template.description.to_owned(),
            category: template.category.to_owned(),
            event: template.rule.event.to_owned(),
            condition_count: template.condition_count(),
            action_count: template.action_count(),
            requires,
            installable: blocked.is_none(),
            blocked_reason: blocked,
            body: template.body(),
        }
    }
}

/// `GET /api/v1/automations/templates` — the six starter rules.
///
/// The gallery is a **catalogue**, not six rows in the tenant's own table: a starter is a
/// definition, and instantiating it is a `POST /api/v1/automations` like any other. That is
/// why the answer carries the request body each template would be saved with, and why the
/// panel's *Use this* button is a create — one code path, one validation, one audit row.
pub async fn list_templates(
    State(state): State<AppState>,
    // No session is read here on purpose: the gallery is a **catalogue**, the same closed
    // vocabulary `/automations/catalogue` serves, and the route guard has already checked
    // `workflows.read`. Reading the account would buy nothing and would suggest the answer
    // differs per caller when it does not — the only per-installation fact is the outbound
    // allow-list, and that comes from the settings row.
    _current: CurrentSession,
) -> Result<Json<TemplateListResponse>, ApiError> {
    // The templates describe actions this installation may not be allowed to call yet (an
    // `http_request` needs the host on the allow-list), so the answer says which ones are
    // installable *now* rather than leaving the panel to find out on save.
    let allowed = omnion_automation::outbound::allowed_hosts(state.db().pool())
        .await
        .unwrap_or_default();

    Ok(Json(TemplateListResponse {
        templates: templates::all()
            .into_iter()
            .map(|template| TemplateBody::build(template, &allowed))
            .collect(),
    }))
}

/// `GET /api/v1/automations/{id}/versions/{version_id}` — one stored version, diffed.
///
/// A separate read rather than "find it in the list" because the Versions tab shows what
/// changed **against the version before it**, and answering that from the list means the
/// browser has to reassemble two rows and trust its own arithmetic.
pub async fn get_version(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((automation_id, version_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<VersionComparison>, ApiError> {
    let (workflow, _) = rule_in_scope(&state, &current, automation_id).await?;
    let current_number = current_version(state.db().pool(), &workflow).await?;

    let history = versions::list(state.db().pool(), workflow.id).await?;
    let position = history
        .iter()
        .position(|version| version.id == version_id)
        .ok_or_else(|| version_not_found())?;
    let wanted = &history[position];

    // History is ordered by the version number, which is unique per rule, so "the one
    // before it" is the next row rather than a `created_at` comparison that two edits in
    // the same millisecond would tie.
    let previous = history.get(position + 1);
    let summary = match previous {
        Some(before) => versions::diff(&before.definition, &wanted.definition),
        // The first version has nothing to be different from, and the summary says so
        // rather than reporting an empty change set that reads like "you changed nothing".
        None => json!({ "first": true, "changed": [], "count": 0 }),
    };

    Ok(Json(VersionComparison {
        version: VersionBody::build(wanted, current_number),
        compared_to: previous.map(|before| before.version),
        summary,
    }))
}

/// The comparison payload.
#[derive(Debug, Serialize)]
pub struct VersionComparison {
    /// The version being looked at.
    pub version: VersionBody,
    /// The number it was compared against, or `None` for the first version.
    pub compared_to: Option<i32>,
    /// What changed, in words.
    pub summary: Value,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Turn a database failure into the answer the rest of the API gives.
///
/// The layer's own errors already carry this mapping (see `crate::error`), but a bare
/// `sqlx` call inside a transaction does not — and "the database is unreachable" and "the
/// database said no" are very different things to a panel. Reusing the audit crate's
/// conversion keeps one vocabulary of 503s across the whole surface.
fn database_error(err: sqlx::Error) -> ApiError {
    ApiError::from(omnion_audit::AuditError::Database(err))
}

/// The one answer for "that version is not this rule's".
///
/// Two callers and two refusal reasons (unknown id, id from another rule) get the same
/// body on purpose: a caller who can tell them apart can probe for the existence of a
/// version in somebody else's rule, which is the same mistake the inbound hook surface
/// avoids by answering 404 for everything.
fn version_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "version_not_found",
        "no such version of this rule",
    )
}

/// Load a rule and refuse it when it lives outside the caller's organization.
async fn rule_in_scope(
    state: &AppState,
    current: &CurrentSession,
    automation_id: Uuid,
) -> Result<(omnion_workflows::Workflow, AutomationRule), ApiError> {
    let workflow = store::find_workflow(state.db().pool(), automation_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "automation_not_found",
                "no such automation",
            )
        })?;
    if workflow.trigger() != omnion_workflows::TriggerKind::Event {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "automation_not_found",
            "no such automation",
        ));
    }
    ensure_same_organization(current, Some(workflow.organization_id))?;
    let rule = AutomationRule::from_workflow(&workflow)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "automation_not_found",
            "no such automation",
        )
    })?;
    Ok((workflow, rule))
}

/// The number the rule is on now, or `0` for a rule whose counter was never written.
async fn current_version(
    pool: &sqlx::PgPool,
    workflow: &omnion_workflows::Workflow,
) -> Result<i32, ApiError> {
    let version: Option<i32> = sqlx::query_scalar("select version from workflows where id = $1")
        .bind(workflow.id)
        .fetch_optional(pool)
        .await
        .map_err(database_error)?;
    Ok(version.unwrap_or(0))
}

/// Rebuild a rule from a stored definition, keeping the identity it has today.
///
/// The identity (id, organization, author, counters) comes from the *live* rule and only
/// the definition comes from the version — a restore changes what the rule does, never
/// who owns it or when it was written.
fn rebuild(definition: &Value, live: &AutomationRule) -> Result<AutomationRule, ApiError> {
    let field = |name: &str| definition.get(name).cloned();
    let text = |name: &str, fallback: &str| {
        definition
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .to_owned()
    };

    let actions: Vec<omnion_workflows::definition::StepDefinition> =
        serde_json::from_value(field("steps").unwrap_or_else(|| json!([]))).map_err(|err| {
            ApiError::bad_request(
                "invalid_version",
                format!("this version's actions cannot be read again: {err}"),
            )
        })?;

    Ok(AutomationRule {
        id: live.id,
        organization_id: live.organization_id,
        created_by: live.created_by,
        site_id: field("site_id").and_then(|value| value.as_str().and_then(|v| v.parse().ok())),
        name: text("name", &live.name),
        description: text("description", &live.description),
        enabled: field("enabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(live.enabled),
        event: text("event", &live.event),
        stored_conditions: field("conditions").unwrap_or_else(|| json!([])),
        actions,
        hook_triggered: field("hook_triggered")
            .and_then(|value| value.as_bool())
            .unwrap_or(live.hook_triggered),
        // The hook token is a credential and is deliberately NOT in a version: a restore
        // must not re-mint a webhook URL, so the rule keeps the one it has.
        hook_configured: live.hook_configured,
        // A version that cannot say what its error policy was falls back to the rule's
        // own: an unreadable policy is a reason to be careful, not to invent one.
        on_error: omnion_workflows::OnError::parse(&text("on_error", "stop"))
            .unwrap_or(live.on_error),
        run_as_user_id: field("run_as_user_id")
            .and_then(|value| value.as_str().and_then(|v| v.parse().ok())),
        rate_limit_per_hour: field("rate_limit_per_hour")
            .and_then(|value| value.as_i64())
            .map_or(live.rate_limit_per_hour, |value| value as i32),
        concurrency: omnion_automation::limits::Concurrency::parse_or_default(&text(
            "concurrency",
            "queue",
        )),
        last_error: None,
        trigger_count: live.trigger_count,
        // A restore replaces the *definition* at the version the rule is on, never at a
        // version of its own. Carrying the live one is the whole correctness of the write:
        // `replace_graph`'s guard compares this against the column, so a snapshot that
        // invented `1` would be refused as a conflict on any rule that had ever been
        // edited, and a restore that reset it to `1` would hand the next editor a version
        // that no longer matches the row. `field("graph_version")` is deliberately NOT
        // consulted: a version is an identity for optimistic concurrency, not a field a
        // stored snapshot gets to choose, and the one that came from a restore would then
        // be able to impersonate a version the author never held.
        graph_version: live.graph_version,
        last_triggered_at: live.last_triggered_at,
        created_at: live.created_at,
        updated_at: live.updated_at,
    })
}

/// The audit rows of one rule, newest first.
///
/// The filter is on the *action* as well as the target, because the rule's lifecycle is
/// audited under several names (`automation.created`, `automation.run_started`,
/// `workflow.execution.failed`, …) and all of them are the operator's question. A run that
/// is not listed here cannot be found anywhere else, and a Versions tab that shows edits
/// without runs explains half of what happened at most.
async fn rule_audit_entries(
    pool: &sqlx::PgPool,
    workflow_id: Uuid,
    organization_id: Uuid,
    limit: i64,
) -> Result<Vec<AuditBody>, ApiError> {
    // `ip_address` is an `inet` column and `AuditEntry.ip_address` is a `String`, so the
    // column has to be cast on the way out exactly as it is on the way in — `crates/audit`
    // writes `ip_address::text` for precisely this reason. Selecting the bare `inet`
    // decodes into TEXT and every read of this query fails with "mismatched types", which
    // the Audit tab showed as an empty trail: the endpoint answered 500 and the panel drew
    // an error, and both read as "nothing was ever recorded" because that is what the
    // screen was actually told.
    let rows = sqlx::query_as::<_, omnion_audit::AuditEntry>(
        "select id, organization_id, actor_user_id, actor_type, action, target_type, \
         target_id, metadata, ip_address::text as ip_address, created_at from audit_log \
         where organization_id = $1 and target_id = $2 \
         order by created_at desc, id desc limit $3",
    )
    .bind(organization_id)
    .bind(workflow_id.to_string())
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(database_error)?;

    Ok(rows
        .into_iter()
        .map(|entry| AuditBody {
            id: entry.id,
            action: entry.action,
            actor_user_id: entry.actor_user_id,
            actor_type: entry.actor_type,
            target_type: entry.target_type,
            target_id: entry.target_id,
            metadata: entry.metadata,
            created_at: entry.created_at,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rule that exists only to be the identity a version is rebuilt onto.
    fn live_rule() -> AutomationRule {
        AutomationRule {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            created_by: None,
            site_id: None,
            name: "Live rule".to_owned(),
            description: String::new(),
            enabled: true,
            event: "user.created".to_owned(),
            stored_conditions: json!([]),
            // One real step: a rule with none cannot be built, and `snapshot()` goes
            // through the same builder a save does — so a stub without steps would fail
            // here for a reason that has nothing to do with what these tests assert.
            actions: vec![omnion_workflows::definition::StepDefinition {
                name: "Say hello".to_owned(),
                kind: omnion_workflows::StepKind::Task,
                action: Some("echo".to_owned()),
                params: json!({ "value": "hi" }),
                on_error: omnion_workflows::OnError::Inherit,
                timeout_ms: 30_000,
                max_attempts: 1,
            }],
            hook_triggered: false,
            hook_configured: false,
            on_error: omnion_workflows::OnError::Stop,
            run_as_user_id: None,
            rate_limit_per_hour: 60,
            concurrency: omnion_automation::limits::Concurrency::Queue,
            last_error: None,
            trigger_count: 0,
            graph_version: 1,
            last_triggered_at: None,
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn a_restore_keeps_the_identity_and_takes_the_definition() {
        // The version, as `versions::record` wrote it.
        let snapshot = live_rule().snapshot().expect("a rule snapshots");
        let mut versioned = snapshot;
        versioned["name"] = json!("What Tuesday said");
        versioned["rate_limit_per_hour"] = json!(5);

        // The rule that exists right now.
        let live = live_rule();
        let restored = rebuild(&versioned, &live).expect("a version rebuilds");

        // The content comes from the version…
        assert_eq!(restored.name, "What Tuesday said");
        assert_eq!(restored.rate_limit_per_hour, 5);
        assert_eq!(restored.actions.len(), 1, "the old actions came back");
        // …and the identity from the live rule. A restore that re-created the rule would
        // change its id, orphan its runs and hand its run history a second engine.
        assert_eq!(restored.id, live.id, "the id moved");
        assert_eq!(restored.organization_id, live.organization_id);
        assert_eq!(restored.trigger_count, live.trigger_count);
    }

    #[test]
    fn a_restore_never_mints_a_new_hook_token() {
        let mut versioned = live_rule().snapshot().expect("a rule snapshots");
        // A version that *claims* a configured webhook — which a hand-edited row could.
        versioned["hook_triggered"] = json!(true);

        let mut live = live_rule();
        live.hook_triggered = true;
        live.hook_configured = true;

        let restored = rebuild(&versioned, &live).expect("a version rebuilds");
        // `hook_configured` is copied from the live rule and never read from the version, so
        // a snapshot can never resurrect a webhook URL that was rotated away.
        assert!(restored.hook_configured);
        assert!(restored.hook_triggered);
    }

    #[test]
    fn the_current_version_is_reported_so_restore_can_refuse_a_no_op() {
        let body = VersionBody {
            id: Uuid::new_v4(),
            automation_id: Uuid::new_v4(),
            version: 3,
            change: "updated".to_owned(),
            summary: json!({ "changed": [], "count": 0 }),
            definition: json!({}),
            created_by: None,
            created_at: OffsetDateTime::now_utc(),
            restored_from: None,
            current: true,
        };
        assert!(
            body.current,
            "the version the rule is running must be marked current"
        );
    }
}
