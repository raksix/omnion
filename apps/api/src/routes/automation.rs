//! `/api/v1/automations` — the automation surface (docs/requests/REQ-003, phase P13).
//!
//! An automation is **trigger → condition → action**, and it is stored as what it is: a workflow
//! whose trigger is an event (`omnion_workflows`), with the conditions an event payload must
//! satisfy and the actions to run. This module is the panel side of that layer — the matcher and
//! the actions themselves live in `omnion-automation`, and the runs are advanced by the same
//! background engine that drives manual and scheduled workflows.
//!
//! Two rules the handlers enforce on top of the permission guard (`crate::guards`):
//!
//! * tenancy — a rule belongs to one organization and, when it is site-scoped, to one site; an
//!   account with a primary organization only ever touches its own (`crate::scope`);
//! * vocabulary — the event name is checked against the bus's own rule, the conditions against
//!   the automation layer's comparison set and the action parameters (including their
//!   `{{event.field}}` bindings) against the layer's binding rules. A rule the matcher could not
//!   run is refused when it is written, not when its event arrives.
//!
//! Every definition change is audited; a match and a run's lifecycle are audited by the matcher
//! and the engine (`automation.rule.matched` / `workflow.execution.*`).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_automation::matcher::update_from_rule;
use omnion_automation::model::{validate_description, validate_name};
use omnion_automation::{AutomationRule, NewRule};
use omnion_workflows::actions::{ACTIONS, ActionDef, HOST_ACTIONS};
use omnion_workflows::definition::StepDefinition;
use omnion_workflows::model::NewWorkflow;
use omnion_workflows::store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One automation rule.
#[derive(Debug, Serialize)]
pub struct AutomationBody {
    /// Rule id (the workflow id behind it — the run history is addressed by it).
    pub id: Uuid,
    /// Organization that owns the rule.
    pub organization_id: Uuid,
    /// Site the rule is bound to, when it is.
    pub site_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Whether the rule fires.
    pub enabled: bool,
    /// Event the rule listens for — for a webhook trigger, the reserved hook event.
    pub event: String,
    /// How the rule starts: `event`, `schedule`, `manual` or `inbound_webhook`.
    pub trigger: &'static str,
    /// The condition tree as stored: a group object, or the flat array of an older rule.
    pub conditions: Value,
    /// How many comparisons the tree carries — what the editor's counter shows.
    pub condition_count: usize,
    /// Actions to run, in order.
    pub actions: Value,
    /// The rule's own failure policy; a step that inherits takes this.
    pub on_error: &'static str,
    /// Whose authority the rule's host actions run with; `null` means the author.
    pub run_as_user_id: Option<Uuid>,
    /// Which of the two it is, in a sentence the panel shows beside the picker.
    pub run_as_description: &'static str,
    /// Runs this rule may start in a rolling hour, and the closed policies a second
    /// trigger may take — read from the layer rather than written into a `select`, so the
    /// picker cannot offer something the guard does not implement.
    pub rate_limit_per_hour: i32,
    pub concurrency: &'static str,
    pub concurrency_description: &'static str,
    /// The rate window as it stands: runs counted in the current hour against the limit,
    /// and when it rolls over. Read *without* the guard's lock — this is a counter for a
    /// person to read, not a decision, and the decision is the one that locks.
    pub window_used: i32,
    pub window_limit: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub window_resets_at: OffsetDateTime,
    /// The last message a bound produced when it refused a run, or `null` when the rule
    /// has never been refused. Cleared the moment a run is admitted again.
    pub last_error: Option<String>,
    /// What each host action of this platform needs, so the panel can show what a run-as
    /// account is being asked for rather than an opaque id.
    pub action_permissions: &'static [(&'static str, &'static str)],
    /// How many runs the trigger has started.
    pub trigger_count: i32,
    /// When the rule last fired.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_triggered_at: Option<OffsetDateTime>,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last change.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// The hook surface of a webhook-triggered rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook: Option<HookBody>,
}

/// What a webhook-triggered rule's URL surface looks like from the outside.
///
/// The token is the only credential and it is **never** part of a read: `GET` on a rule
/// reports that a hook exists, where to mint one and how many calls the current window has
/// spent — never the URL itself. Minting is a separate, explicit `POST …/rotate-hook`,
/// which is the only time a token appears in a response.
#[derive(Debug, Serialize)]
pub struct HookBody {
    /// `true` when the rule has a live token.
    pub configured: bool,
    /// How many inbound calls the current rate window has spent.
    pub window_used: i64,
    /// The window's ceiling.
    pub window_limit: i64,
    /// When the window rolls over.
    #[serde(with = "time::serde::rfc3339")]
    pub window_resets_at: OffsetDateTime,
    /// The path template the panel shows, with the token left out.
    pub path_template: &'static str,
}

impl AutomationBody {
    /// Describe one rule.
    fn build(
        rule: &AutomationRule,
        actions: Value,
        hook: Option<HookBody>,
        window: (i32, OffsetDateTime),
    ) -> Self {
        let condition_count = rule
            .condition_group()
            .map(|group| group.comparison_count())
            .unwrap_or_default();

        Self {
            id: rule.id,
            organization_id: rule.organization_id,
            site_id: rule.site_id,
            name: rule.name.clone(),
            description: rule.description.clone(),
            enabled: rule.enabled,
            event: rule.event.clone(),
            trigger: if rule.hook_triggered {
                "inbound_webhook"
            } else {
                "event"
            },
            conditions: rule.stored_conditions.clone(),
            condition_count,
            actions,
            on_error: rule.on_error.as_str(),
            run_as_user_id: rule.run_as_user_id,
            run_as_description: rule.authority().describe(),
            rate_limit_per_hour: rule.rate_limit_per_hour,
            concurrency: rule.concurrency.as_str(),
            concurrency_description: rule.concurrency.describe(),
            window_used: window.0,
            window_limit: rule.rate_limit_per_hour,
            // The window rolls over a rolling hour from where it starts, so the answer is
            // the *read* time plus the window — the same arithmetic the guard does, and
            // the one the panel needs to say "try again after".
            window_resets_at: window.1 + omnion_automation::limits::WINDOW,
            last_error: rule.last_error.clone(),
            action_permissions: omnion_automation::authority::ACTION_PERMISSIONS,
            trigger_count: rule.trigger_count,
            last_triggered_at: rule.last_triggered_at,
            created_at: rule.created_at,
            updated_at: rule.updated_at,
            hook,
        }
    }
}

/// The list payload.
#[derive(Debug, Serialize)]
pub struct AutomationListResponse {
    /// Matching rules, newest first.
    pub automations: Vec<AutomationBody>,
}

/// One action of the catalogue, as the builder offers it.
#[derive(Debug, Serialize)]
pub struct CatalogueAction {
    /// Stable action key.
    pub key: &'static str,
    /// What it does, in product language.
    pub description: &'static str,
    /// `true` when the action touches the world (an email, a comment) rather than the run only.
    pub host: bool,
}

/// One condition operator of the catalogue.
#[derive(Debug, Serialize)]
pub struct CatalogueOperator {
    /// Stable operator key.
    pub key: &'static str,
    /// `true` when the operator compares against a value.
    pub needs_value: bool,
}

/// The vocabulary a rule is written in.
#[derive(Debug, Serialize)]
pub struct CatalogueResponse {
    /// The event library, with the payload fields each event carries.
    pub events: Vec<&'static omnion_automation::catalogue::EventDef>,
    /// The closed comparison set.
    pub condition_operators: Vec<CatalogueOperator>,
    /// The closed action set.
    pub actions: Vec<CatalogueAction>,
    /// How the rule starts.
    pub trigger_kinds: Vec<&'static str>,
    /// The condition group modes the editor offers.
    pub group_modes: Vec<&'static str>,
    /// How deep groups may nest.
    pub max_group_depth: usize,
    /// How many conditions and groups a rule may carry in total.
    pub max_conditions: usize,
    /// The event a webhook trigger listens for.
    pub hook_event: &'static str,
    /// The reserved hook name, as the bus records it.
    pub hook_path_template: &'static str,
    /// How a payload field is named inside a condition or a binding.
    pub binding_syntax: &'static str,
    /// The comparison operators a `branch` step offers — the same nine the conditions use.
    pub branch_operators: Vec<CatalogueOperator>,
    /// The step kinds a definition may carry, in the order the editor lists them.
    pub step_kinds: Vec<&'static str>,
    /// The permission that may decide a parked `approval` step, and the default lifetime of
    /// a gate in hours — the editor offers both, and both come from the engine rather than
    /// from a constant written in the panel.
    pub approval_permission: &'static str,
    /// The default and the ceiling of a gate's lifetime, in hours.
    pub approval_ttl_hours: i32,
    pub max_approval_ttl_hours: i32,
    /// What a step's own failure may do; `inherit` takes the rule's policy.
    pub on_error_policies: Vec<&'static str>,
    /// The longest a step may block, in milliseconds, and the default.
    pub max_step_timeout_ms: i32,
    pub default_step_timeout_ms: i32,
    /// The methods an outbound call may use.
    pub outbound_methods: Vec<&'static str>,
    /// How many rules deep a `run_workflow` chain may go before it is refused.
    pub max_chain_depth: usize,
    /// An example of the payload an inbound call produces.
    pub hook_sample: Value,
}

/// Build the catalogue of the closed vocabulary.
#[must_use]
pub fn catalogue() -> CatalogueResponse {
    let catalogue_action = |action: &ActionDef, host: bool| CatalogueAction {
        key: action.key,
        description: action.description,
        host,
    };

    CatalogueResponse {
        events: omnion_automation::catalogue::EVENTS.iter().collect(),
        condition_operators: omnion_automation::groups::operators()
            .into_iter()
            .map(|(key, needs_value)| CatalogueOperator { key, needs_value })
            .collect(),
        actions: ACTIONS
            .iter()
            .map(|action| catalogue_action(action, false))
            .chain(
                HOST_ACTIONS
                    .iter()
                    .map(|action| catalogue_action(action, true)),
            )
            .collect(),
        trigger_kinds: omnion_automation::catalogue::TriggerKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect(),
        group_modes: vec!["all", "any"],
        max_group_depth: omnion_automation::groups::MAX_GROUP_DEPTH,
        max_conditions: omnion_automation::groups::MAX_GROUP_NODES,
        hook_event: omnion_automation::catalogue::HOOK_EVENT,
        hook_path_template: "/api/v1/hooks/<token>",
        binding_syntax: "{{event.field}} — the field is read from the event payload",
        // A branch reads what a *step* produced or what the event carried, so it offers
        // the same nine operators plus the two namespaces it can actually read.
        branch_operators: omnion_workflows::branch::OPERATORS
            .iter()
            .map(|key| CatalogueOperator {
                key,
                needs_value: !matches!(*key, "exists" | "not_exists"),
            })
            .collect(),
        // `approval` joins the four: it is an engine kind (the engine decides it, no action
        // is named) and it is the one that *parks on a person* rather than on a clock.
        step_kinds: vec!["task", "wait", "branch", "stop", "approval"],
        on_error_policies: vec!["inherit", "stop", "continue"],
        approval_permission: omnion_workflows::APPROVAL_PERMISSION,
        approval_ttl_hours: omnion_workflows::approval::DEFAULT_TTL_HOURS,
        max_approval_ttl_hours: omnion_workflows::approval::MAX_TTL_HOURS,
        max_step_timeout_ms: omnion_workflows::MAX_STEP_TIMEOUT_MS,
        default_step_timeout_ms: omnion_workflows::DEFAULT_STEP_TIMEOUT_MS,
        outbound_methods: omnion_workflows::actions::OUTBOUND_METHODS.to_vec(),
        max_chain_depth: omnion_automation::outbound::MAX_CHAIN_DEPTH,
        hook_sample: omnion_automation::hooks::sample_payload(),
    }
}

// ---------------------------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------------------------

/// Query of a rule list.
#[derive(Debug, Deserialize)]
pub struct AutomationListQuery {
    /// Organization to read; a platform account must name one to narrow the list.
    pub organization_id: Option<Uuid>,
    /// Site to narrow the list to.
    pub site_id: Option<Uuid>,
}

/// A rule to create or replace.
#[derive(Debug, Deserialize)]
pub struct AutomationInput {
    /// Organization that owns the rule.
    pub organization_id: Option<Uuid>,
    /// Site the rule is bound to.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Free-form description.
    #[serde(default)]
    pub description: String,
    /// Whether the rule fires straight away.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Event the rule listens for. Ignored (and overwritten) for a webhook trigger, which
    /// always listens for the reserved hook event.
    pub event: String,
    /// The condition tree: `{"all": [ … ]}` / `{"any": [ … ]}`, or the flat array of an
    /// older rule, which reads as one `all` group.
    #[serde(default = "empty_conditions")]
    pub conditions: Value,
    /// Whether the rule is triggered by its own inbound webhook URL.
    #[serde(default)]
    pub hook_triggered: bool,
    /// The rule's own error policy — what a step's failure does when the step says
    /// `inherit`. Absent means `stop`, which is what every rule did before the policy
    /// existed.
    #[serde(default = "default_on_error")]
    pub on_error: RuleOnError,
    /// Whose authority the rule's host actions run with (REQ-003 slice 3).
    ///
    /// Absent (or `null`) follows the rule's author. A named account is the case the copy
    /// could not express: a shared "content publisher" service identity that keeps working
    /// after the person who wrote the rule leaves. It is resolved at *run* time, so handing a
    /// rule to an account changes what happens from the next run, not from a redeploy.
    #[serde(default)]
    pub run_as_user_id: Option<Uuid>,
    /// Runs this rule may start in a rolling hour (REQ-003 slice 4).
    ///
    /// Absent takes the default. The bound is checked here, in words, rather than only by
    /// the column's check constraint: a `0` is what an author reaches for when they mean
    /// "off", and the switch is the thing that means off.
    #[serde(default)]
    pub rate_limit_per_hour: Option<i32>,
    /// What a second trigger does while a run of this rule is going: `queue` or `skip`.
    #[serde(default)]
    pub concurrency: Option<RuleConcurrency>,
    /// Actions to run, in order.
    pub actions: Vec<StepDefinition>,
}

/// The concurrency policy, as the wire spells it.
///
/// A two-variant enum rather than a `String`, so a typo is a `400` from serde instead of a
/// row the guard has to guess at — and the guard's fallback (`queue`) is then a repair path
/// for a hand-edited row rather than a way to save one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleConcurrency {
    /// Triggers that arrive while a run is going wait their turn.
    Queue,
    /// A trigger that arrives while a run is going is dropped and reported.
    Skip,
}

impl From<RuleConcurrency> for omnion_automation::limits::Concurrency {
    fn from(policy: RuleConcurrency) -> Self {
        match policy {
            RuleConcurrency::Queue => Self::Queue,
            RuleConcurrency::Skip => Self::Skip,
        }
    }
}

/// Serde default for [`AutomationInput::on_error`]: v0's behaviour, a failure ends the run.
fn default_on_error() -> RuleOnError {
    RuleOnError::Stop
}

/// The rule-level error policy, as the wire spells it.
///
/// The engine's own [`omnion_workflows::OnError`] also carries a step-level `inherit`,
/// which a *rule* may not choose — so the two are separate types rather than one with a
/// spare variant, and a rule cannot send `inherit` and end up deferring to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleOnError {
    /// A failing step ends the run.
    Stop,
    /// A failing step is recorded and the run carries on.
    Continue,
}

impl From<RuleOnError> for omnion_workflows::OnError {
    fn from(policy: RuleOnError) -> Self {
        match policy {
            RuleOnError::Stop => Self::Stop,
            RuleOnError::Continue => Self::Continue,
        }
    }
}

/// Serde default for [`AutomationInput::enabled`]: a new rule is armed.
fn enabled_by_default() -> bool {
    true
}

/// Serde default for [`AutomationInput::conditions`]: a rule with no conditions.
fn empty_conditions() -> Value {
    Value::Array(Vec::new())
}

impl AutomationInput {
    /// The rule this request describes, checked.
    fn rule(&self, organization_id: Uuid) -> Result<NewRule, ApiError> {
        let rule = NewRule {
            name: validate_name(&self.name)?,
            description: validate_description(&self.description)?,
            enabled: self.enabled,
            site_id: self.site_id,
            event: self.event.clone(),
            stored_conditions: self.conditions.clone(),
            actions: self.actions.clone(),
            hook_triggered: self.hook_triggered,
            on_error: self.on_error.into(),
            run_as_user_id: self.run_as_user_id,
            // Both bounds are checked here rather than only by the column's constraint, so
            // a rule with a limit the panel cannot explain is refused with the *bound's*
            // message instead of with whatever the definition check notices first.
            rate_limit_per_hour: match self.rate_limit_per_hour {
                Some(limit) => Some(omnion_automation::limits::Policy::check_rate_limit(limit)?),
                None => None,
            },
            concurrency: self.concurrency.map(Into::into),
        };

        // The definition check is the full one: the event name against the bus's rule, the
        // condition tree and the bindings against the layer's, and the trigger/actions/steps
        // against the engine's. A definition the matcher could not run never reaches the store.
        rule.definition()?;

        // The organization travels with the definition, not inside it; this only proves the
        // caller asked for one the scope check accepted.
        let _ = organization_id;

        Ok(rule)
    }
}

/// Check every `http_request` step's host against the installation's allow-list.
///
/// This is the *write-time* half of the bound; the action re-checks at call time because
/// the list can change between writing a rule and running it. Refusing here is what the
/// request asks for — "refused at save time naming the host" — and it is the difference
/// between an author learning about it now and about it three weeks later when the rule
/// first fires.
async fn check_outbound_hosts(
    state: &AppState,
    actions: &[StepDefinition],
) -> Result<(), ApiError> {
    let steps: Vec<&StepDefinition> = actions
        .iter()
        .filter(|step| step.action.as_deref() == Some("http_request"))
        .collect();
    if steps.is_empty() {
        return Ok(());
    }

    let allowed = omnion_automation::outbound::allowed_hosts(state.db().pool()).await?;
    for step in steps {
        // A URL the engine itself refused never reaches here; a URL that parses but names
        // a host outside the list does.
        let Ok(target) = omnion_automation::outbound::parse_target(
            step.params
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ) else {
            continue;
        };
        if !omnion_automation::outbound::host_allowed(&target.host, &allowed) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "host_not_allowed",
                omnion_automation::outbound::host_refusal(&target.host, &allowed),
            ));
        }
    }

    Ok(())
}

/// A test payload for a dry run.
#[derive(Debug, Deserialize)]
pub struct TestRequest {
    /// The payload to evaluate the rule against.
    pub payload: Value,
}

/// The answer of a dry run: what the rule would do, and what it did not do.
#[derive(Debug, Serialize)]
pub struct TestResponse {
    /// The report, row by row.
    pub report: omnion_automation::testing::DryRunReport,
    /// The stored report, so the panel can re-read it after a reload.
    pub recorded: TestEventBody,
}

/// One stored test or listener row, as the panel reads it.
#[derive(Debug, Serialize)]
pub struct TestEventBody {
    /// Row id.
    pub id: Uuid,
    /// `test` or `listen`.
    pub kind: String,
    /// The payload the row carries — the hand-written one, or the captured one.
    pub payload: Option<Value>,
    /// Event id, when a listener captured one.
    pub event_id: Option<i64>,
    /// Event name, when a listener captured one.
    pub event_name: Option<String>,
    /// `true` while a listener waits for its next event.
    pub armed: bool,
    /// When the row was written.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When a listener filled in.
    #[serde(with = "time::serde::rfc3339::option")]
    pub captured_at: Option<OffsetDateTime>,
}

impl From<&omnion_automation::testing::TestEvent> for TestEventBody {
    fn from(row: &omnion_automation::testing::TestEvent) -> Self {
        Self {
            id: row.id,
            kind: row.kind.clone(),
            payload: row.payload.clone(),
            event_id: row.event_id,
            event_name: row.event_name.clone(),
            armed: row.is_armed(),
            created_at: row.created_at,
            captured_at: row.captured_at,
        }
    }
}

/// The rows of a rule's test history, newest first.
#[derive(Debug, Serialize)]
pub struct TestListResponse {
    /// The rows.
    pub tests: Vec<TestEventBody>,
}

/// A newly minted hook token — the only response that ever carries one.
#[derive(Debug, Serialize)]
pub struct RotateHookResponse {
    /// The URL to give the caller, token included.
    pub url: String,
    /// The token on its own, for a caller that builds the URL itself.
    pub token: String,
    /// The rule the URL belongs to.
    pub automation_id: Uuid,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/automations/catalogue` — the closed vocabulary a rule is written in.
pub async fn get_catalogue(
    State(_state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<CatalogueResponse>, ApiError> {
    Ok(Json(catalogue()))
}

/// `GET /api/v1/automations` — the rules of one scope.
pub async fn list_automations(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<AutomationListQuery>,
) -> Result<Json<AutomationListResponse>, ApiError> {
    let organization_id = match current.user.organization_id {
        Some(own) => {
            ensure_same_organization(&current, query.organization_id)?;
            Some(own)
        }
        None => query.organization_id,
    };

    let workflows =
        store::list_event_workflows(state.db().pool(), organization_id, query.site_id).await?;

    let mut automations = Vec::with_capacity(workflows.len());
    for workflow in &workflows {
        let Some(rule) = AutomationRule::from_workflow(workflow)? else {
            continue;
        };
        let hook = hook_body(state.db().pool(), &rule).await?;
        let window = rate_window(state.db().pool(), &rule).await;
        automations.push(AutomationBody::build(
            &rule,
            workflow.steps.clone(),
            hook,
            window,
        ));
    }

    Ok(Json(AutomationListResponse { automations }))
}

/// The rule's rate window as it stands: runs counted against the limit.
///
/// Read without the guard's lock and deliberately so — the guard's decision is the one
/// that has to be exact, and this is a number a person is reading off a list. Adding the
/// lock here would put a `for update` on every row of the rule list for a counter.
async fn rate_window(pool: &sqlx::PgPool, rule: &AutomationRule) -> (i32, OffsetDateTime) {
    omnion_automation::limits::window_state(pool, rule.id)
        .await
        .unwrap_or((0, OffsetDateTime::now_utc()))
}

/// The hook surface of a rule, or `None` for a rule that is not webhook-triggered.
///
/// Every rule that *has* a token is a webhook rule; a rule that declares a webhook trigger
/// but has not minted a token yet still reports its surface, with `configured: false` — the
/// panel then shows "your URL is not created yet" instead of pretending there is one.
async fn hook_body(
    pool: &sqlx::PgPool,
    rule: &AutomationRule,
) -> Result<Option<HookBody>, ApiError> {
    if !rule.hook_triggered {
        return Ok(None);
    }

    let (window_used, window_resets_at) =
        omnion_automation::hooks::window_state(pool, rule.id).await?;
    Ok(Some(HookBody {
        configured: rule.hook_configured,
        window_used,
        window_limit: omnion_automation::hooks::DEFAULT_HOOK_LIMIT,
        window_resets_at,
        path_template: "/api/v1/hooks/<token>",
    }))
}

/// `POST /api/v1/automations` — write one rule.
pub async fn create_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(input): Json<AutomationInput>,
) -> Result<(StatusCode, Json<AutomationBody>), ApiError> {
    let organization_id = resolve_organization(&current, input.organization_id)?;
    if let Some(site_id) = input.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }

    let rule = input.rule(organization_id)?;
    let definition = rule.definition()?;
    check_outbound_hosts(&state, &rule.actions).await?;

    let workflow = store::insert_workflow(
        state.db().pool(),
        NewWorkflow {
            on_error: rule.on_error,
            organization_id,
            site_id: rule.site_id,
            name: rule.name.clone(),
            description: rule.description.clone(),
            enabled: rule.enabled,
            trigger: definition.trigger.kind,
            schedule: None,
            trigger_event: definition.trigger.event.clone(),
            conditions: definition.conditions_json()?,
            run_as_user_id: rule.run_as_user_id,
            rate_limit_per_hour: rule.rate_limit_per_hour,
            concurrency: rule.concurrency.map(|policy| policy.as_str().to_owned()),
            next_run_at: None,
            steps: definition.steps_json()?,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.created")
            .organization(organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({
                "name": workflow.name,
                "event": workflow.trigger_event,
                "conditions": workflow.conditions.as_array().map(Vec::len).unwrap_or_default(),
                "actions": workflow.steps.as_array().map(Vec::len).unwrap_or_default(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    let stored = AutomationRule::from_workflow(&workflow)?.ok_or_else(automation_not_found)?;

    // The first version, written next to the rule rather than by a follow-up call. A rule
    // whose creation is not in its own history has no "before", so the Versions tab would
    // start at the first *edit* and the audit trail would be missing the row that explains
    // why the rule exists at all.
    let mut tx = state
        .db()
        .pool()
        .begin()
        .await
        .map_err(automation_db_error)?;
    let version = omnion_automation::versions::record(
        &mut tx,
        &stored,
        omnion_automation::versions::Change::Created,
        Some(current.user.id),
        None,
    )
    .await?;
    tx.commit().await.map_err(automation_db_error)?;
    let _ = version;

    let hook = hook_body(state.db().pool(), &stored).await?;
    Ok((
        StatusCode::CREATED,
        Json(AutomationBody::build(
            &stored,
            workflow.steps.clone(),
            hook,
            rate_window(state.db().pool(), &stored).await,
        )),
    ))
}

/// `GET /api/v1/automations/{id}` — one rule.
pub async fn get_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
) -> Result<Json<AutomationBody>, ApiError> {
    let workflow = automation_in_scope(&state, &current, automation_id).await?;
    let rule = AutomationRule::from_workflow(&workflow)?.ok_or_else(automation_not_found)?;
    let hook = hook_body(state.db().pool(), &rule).await?;
    let window = rate_window(state.db().pool(), &rule).await;
    Ok(Json(AutomationBody::build(
        &rule,
        workflow.steps.clone(),
        hook,
        window,
    )))
}

/// `PUT /api/v1/automations/{id}` — replace a rule.
///
/// A rule is replaced, not patched: its conditions and actions are edited as a whole, and the
/// runs that already started keep the steps they materialised — editing never rewrites history.
pub async fn update_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(automation_id): Path<Uuid>,
    Json(input): Json<AutomationInput>,
) -> Result<Json<AutomationBody>, ApiError> {
    let existing = automation_in_scope(&state, &current, automation_id).await?;
    ensure_same_organization(
        &current,
        input.organization_id.or(Some(existing.organization_id)),
    )?;

    if let Some(site_id) = input.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }

    let rule = input.rule(existing.organization_id)?;
    let definition = rule.definition()?;
    check_outbound_hosts(&state, &rule.actions).await?;

    // The concurrency policy is the one bound that has to be resolved *before* the rule is
    // built, because the rule carries a settled policy while the request may carry none.
    // An absent policy keeps what the rule already had, so a `PUT` from a client that has
    // not learned about the bound cannot silently reset it to the default — the fallback
    // reads the stored text, and the stored text is a repair path for a hand-edited row
    // rather than a way to save one.
    let concurrency = rule.concurrency.unwrap_or_else(|| {
        omnion_automation::limits::Concurrency::parse_or_default(&existing.concurrency)
    });

    let update = update_from_rule(&AutomationRule {
        id: existing.id,
        organization_id: existing.organization_id,
        created_by: existing.created_by,
        site_id: rule.site_id,
        name: rule.name.clone(),
        description: rule.description.clone(),
        enabled: rule.enabled,
        event: rule.event.clone(),
        stored_conditions: rule.stored_conditions.clone(),
        actions: rule.actions.clone(),
        hook_triggered: rule.hook_triggered,
        hook_configured: existing.hook_token_hash.is_some(),
        on_error: rule.on_error,
        run_as_user_id: rule.run_as_user_id,
        // The bounds the request sent, or the ones the rule already had: a `PUT` from a
        // client that has not learned about them yet must not reset them to the default.
        rate_limit_per_hour: rule
            .rate_limit_per_hour
            .unwrap_or(existing.rate_limit_per_hour),
        // The two live on different types on purpose: the wire speaks the two-variant enum
        // (a typo is a 400, not a row the guard has to guess at) while the row keeps the
        // stored text, and the update takes the text. So the write converts once, here, and
        // an absent policy keeps whatever the rule already had rather than resetting it.
        concurrency,
        last_error: existing.last_error.clone(),
        trigger_count: existing.trigger_count,
        last_triggered_at: existing.last_triggered_at,
        created_at: existing.created_at,
        updated_at: existing.updated_at,
    })
    .ok_or_else(|| ApiError::bad_request("invalid_request", "the rule could not be stored"))?;

    let workflow = store::update_workflow(state.db().pool(), existing.id, update)
        .await?
        .ok_or_else(automation_not_found)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.updated")
            .organization(existing.organization_id)
            .target("workflow", existing.id.to_string())
            .metadata(json!({
                "name": workflow.name,
                "event": workflow.trigger_event,
                "enabled": workflow.enabled,
                "conditions": workflow.conditions.as_array().map(Vec::len).unwrap_or_default(),
                "actions": workflow.steps.as_array().map(Vec::len).unwrap_or_default(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    let stored = AutomationRule::from_workflow(&workflow)?.ok_or_else(automation_not_found)?;
    let _ = definition;

    // One row per edit, carrying the definition *as it was written* — which is why this
    // runs after the update and reads the stored rule rather than the request: a snapshot
    // of what the caller sent would be a snapshot of their intent, not of the rule.
    let mut tx = state
        .db()
        .pool()
        .begin()
        .await
        .map_err(automation_db_error)?;
    let version = omnion_automation::versions::record(
        &mut tx,
        &stored,
        omnion_automation::versions::Change::Updated,
        Some(current.user.id),
        None,
    )
    .await?;
    tx.commit().await.map_err(automation_db_error)?;
    let _ = version;

    let hook = hook_body(state.db().pool(), &stored).await?;
    let window = rate_window(state.db().pool(), &stored).await;
    Ok(Json(AutomationBody::build(
        &stored,
        workflow.steps.clone(),
        hook,
        window,
    )))
}

/// `POST /api/v1/automations/{id}/test` — evaluate a hand-written payload, touch nothing.
///
/// The report is the whole answer: which conditions held against this payload, what each
/// action *would* do, and `simulated: true` on every row. Nothing is sent, published or
/// called — which is what makes it safe to press on a rule that is already armed.
pub async fn test_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
    address: ClientAddress,
    Json(input): Json<TestRequest>,
) -> Result<Json<TestResponse>, ApiError> {
    let workflow = automation_in_scope(&state, &current, automation_id).await?;
    let rule = AutomationRule::from_workflow(&workflow)?.ok_or_else(automation_not_found)?;

    omnion_automation::testing::validate_payload(&input.payload)?;

    let report = omnion_automation::testing::dry_run(&rule, &input.payload);
    let recorded = omnion_automation::testing::record_test(
        state.db().pool(),
        workflow.organization_id,
        workflow.id,
        &input.payload,
        current.user.id,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.tested")
            .organization(workflow.organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({
                "would_run": report.would_run,
                "actions": report.actions.len(),
                "test_id": recorded.id,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(TestResponse {
        report,
        recorded: TestEventBody::from(&recorded),
    }))
}

/// `POST /api/v1/automations/{id}/listen` — arm a one-shot listener for the next real event.
///
/// The row is written empty; the matcher fills it with the payload of the first event the
/// rule actually matches. One shot, and one listener per rule: a second press replaces the
/// first, so the panel never shows two armed rows and the matcher only has one to fill.
pub async fn listen_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<(StatusCode, Json<TestEventBody>), ApiError> {
    let workflow = automation_in_scope(&state, &current, automation_id).await?;

    let armed = omnion_automation::testing::arm_listener(
        state.db().pool(),
        workflow.organization_id,
        workflow.id,
        current.user.id,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.listener_armed")
            .organization(workflow.organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({ "event": workflow.trigger_event, "test_id": armed.id }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(TestEventBody::from(&armed))))
}

/// `GET /api/v1/automations/{id}/tests` — the rule's test reports and captured payloads.
pub async fn list_tests(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
) -> Result<Json<TestListResponse>, ApiError> {
    let workflow = automation_in_scope(&state, &current, automation_id).await?;

    let rows =
        omnion_automation::testing::list_for_workflow(state.db().pool(), workflow.id, 20).await?;

    Ok(Json(TestListResponse {
        tests: rows.iter().map(TestEventBody::from).collect(),
    }))
}

/// `POST /api/v1/automations/{id}/rotate-hook` — mint a fresh inbound-webhook token.
///
/// The only response in the surface that carries a token, and the reason is simple: the
/// server keeps a hash, so a token that was never read is a token nobody knows. Rotation
/// invalidates the previous URL immediately, which is the answer to "this URL leaked".
///
/// A rule that is not webhook-triggered is refused: minting a URL for an event rule would
/// create a trigger that never fires, which is a dead feature rather than a mistake.
pub async fn rotate_hook(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<(StatusCode, Json<RotateHookResponse>), ApiError> {
    let workflow = automation_in_scope(&state, &current, automation_id).await?;

    if !AutomationRule::from_workflow(&workflow)?
        .ok_or_else(automation_not_found)?
        .hook_triggered
    {
        return Err(ApiError::bad_request(
            "not_a_webhook_trigger",
            "this rule is not triggered by an inbound webhook; switch its trigger first",
        ));
    }

    let issued = omnion_automation::hooks::issue_token();
    omnion_automation::hooks::set_token(state.db().pool(), workflow.id, &issued.hash).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.hook_rotated")
            .organization(workflow.organization_id)
            .target("workflow", workflow.id.to_string())
            // The token itself is never audited: an audit row is read by more people than
            // the URL, and the token is a credential.
            .metadata(json!({ "event": workflow.trigger_event }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(RotateHookResponse {
            url: format!("/api/v1/hooks/{}", issued.token),
            token: issued.token,
            automation_id: workflow.id,
        }),
    ))
}

/// `POST /api/v1/hooks/{token}` — an inbound webhook call, answered without a session.
///
/// The token **is** the credential, and every way of failing answers `404` with the same
/// body: an unknown token, a rotated one, a paused rule's, and a token that is not shaped
/// like one. The caller learns nothing about which rules exist or whether this platform has
/// hooks at all, and the rule's name is never echoed back — the only thing returned is the
/// rule id, so a caller can correlate its own call with its own logs.
///
/// The call is recorded on the bus as `automation.hook.received`, and from there it is the
/// same matcher, the same conditions and the same durable run as any other event. The rate
/// window is checked *before* the event is written, so a caller over its allowance produces
/// no events at all.
pub async fn receive_hook(
    State(state): State<AppState>,
    Path(token): Path<String>,
    address: ClientAddress,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Some(rule) = omnion_automation::hooks::find_rule(state.db().pool(), &token).await? else {
        return Err(hook_not_found());
    };

    match omnion_automation::hooks::count_hit(
        state.db().pool(),
        rule.workflow_id,
        omnion_automation::hooks::DEFAULT_HOOK_LIMIT,
        OffsetDateTime::now_utc(),
    )
    .await?
    {
        omnion_automation::hooks::RateVerdict::Allowed { .. } => {}
        omnion_automation::hooks::RateVerdict::Limited {
            limit, resets_at, ..
        } => {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "automation_hook_limited",
                format!(
                    "this webhook accepted {limit} calls in its window; the window resets at \
                     {resets_at}"
                ),
            ));
        }
    }

    let event_id = omnion_automation::hooks::record_call(
        state.db().pool(),
        &rule,
        body,
        "POST",
        address.as_text().unwrap_or_default().as_str(),
    )
    .await?;

    // `202`: the event is recorded and a run will be started by the matcher on its next
    // tick. Answering `200` would claim the work already happened, which it has not.
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "automation_id": rule.workflow_id, "event_id": event_id })),
    ))
}

/// `DELETE /api/v1/automations/{id}` — remove a rule and its run history.
pub async fn delete_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let existing = automation_in_scope(&state, &current, automation_id).await?;

    if !store::delete_workflow(state.db().pool(), existing.id).await? {
        return Err(automation_not_found());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.deleted")
            .organization(existing.organization_id)
            .target("workflow", existing.id.to_string())
            .metadata(json!({ "name": existing.name, "event": existing.trigger_event }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Running a rule and repairing a run
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/automations/{id}/run` — start exactly one run, now, in the real world.
///
/// "Run now" is the one button that touches the world, so it is deliberately different from
/// the dry run beside it: the payload is not the author's, the actions really send, publish
/// and call. A rule on `user.created` run from here acts on an *empty* payload — its
/// conditions are re-evaluated against `{}` and, unless they all hold, the run ends
/// immediately having done nothing. That is the honest answer: there is no event to invent
/// one for. The response names it, so the panel can say "the conditions did not hold, so
/// nothing ran" instead of showing a green run.
pub async fn run_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<(StatusCode, Json<RunResponse>), ApiError> {
    let workflow = automation_in_scope(&state, &current, automation_id).await?;
    let rule = AutomationRule::from_workflow(&workflow)?.ok_or_else(automation_not_found)?;

    // A manual run has no event, so its steps carry no `{{event.*}}` to resolve. Rather than
    // write an empty string into somebody's published page, the run is refused with the
    // reason — the same reason the matcher would give, and in the same words.
    if let Err(err) = omnion_automation::matcher::resolve_steps(&rule, &json!({})) {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "automation.run_refused")
                .organization(workflow.organization_id)
                .target("workflow", workflow.id.to_string())
                .metadata(json!({ "reason": err.to_string() }))
                .ip_address(address.as_text()),
        )
        .await?;
        return Err(ApiError::bad_request(
            "automation_run_refused",
            format!(
                "this rule needs the event it was written for, so it cannot be run by hand: {}",
                err
            ),
        ));
    }

    let steps = omnion_automation::matcher::resolve_steps(&rule, &json!({}))?;
    let (execution, rows) = store::create_execution(
        state.db().pool(),
        &workflow,
        omnion_workflows::TriggerKind::Manual,
        Some(current.user.id),
        &steps,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.run_started")
            .organization(workflow.organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({
                "execution_id": execution.id,
                "steps": rows.len(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(RunResponse {
            execution_id: execution.id,
            workflow_id: workflow.id,
            steps: rows.len(),
        }),
    ))
}

/// The answer of "run now": what started, so the panel can link to it.
#[derive(Debug, Serialize)]
pub struct RunResponse {
    /// The run that started.
    pub execution_id: Uuid,
    /// The rule it belongs to.
    pub workflow_id: Uuid,
    /// How many steps it carries.
    pub steps: usize,
}

/// Which step a retry or a resume addresses.
#[derive(Debug, Deserialize)]
pub struct StepRef {
    /// 1-based position in the run, as the trace numbers it.
    pub step_no: i32,
}

/// The answer of a retry or a resume: what is queued again.
#[derive(Debug, Serialize)]
pub struct RetryResponse {
    /// The run that was re-opened.
    pub execution_id: Uuid,
    /// The step the operator pointed at.
    pub step_no: i32,
    /// How many steps went back on the queue (the chosen one and everything after it).
    pub requeued: u64,
}

/// `POST /api/v1/workflow-executions/{id}/retry-step` — try a failed run again from a step.
///
/// **Retry** and **Resume from here** are the same write, and deliberately so: re-running
/// only the failed step would let a run whose middle failed march on to completion, which
/// is not what "try that again" means to anybody reading a trace. The chosen step and
/// everything after it go back on the queue; the steps that already succeeded are left
/// exactly as they are, and every outbound call carries the run id as its idempotency key,
/// so a receiver that honours it drops the duplicate.
pub async fn retry_step(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(execution_id): Path<Uuid>,
    address: ClientAddress,
    Json(input): Json<StepRef>,
) -> Result<Json<RetryResponse>, ApiError> {
    requeue(
        state,
        current,
        execution_id,
        input.step_no,
        "workflow.execution.retried",
        address,
    )
    .await
}

/// `POST /api/v1/workflow-executions/{id}/resume-from` — the same write, named for what the
/// panel's button says.
pub async fn resume_from(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(execution_id): Path<Uuid>,
    address: ClientAddress,
    Json(input): Json<StepRef>,
) -> Result<Json<RetryResponse>, ApiError> {
    requeue(
        state,
        current,
        execution_id,
        input.step_no,
        "workflow.execution.resumed",
        address,
    )
    .await
}

/// The shared body of retry and resume.
async fn requeue(
    state: AppState,
    current: CurrentSession,
    execution_id: Uuid,
    step_no: i32,
    audit_action: &'static str,
    address: ClientAddress,
) -> Result<Json<RetryResponse>, ApiError> {
    if step_no < 1 {
        return Err(ApiError::bad_request(
            "invalid_step_no",
            "a step is numbered from 1",
        ));
    }

    let execution = store::find_execution(state.db().pool(), execution_id)
        .await?
        .ok_or_else(execution_not_found)?;
    ensure_same_organization(&current, Some(execution.organization_id))?;

    // A cancelled run was closed on purpose; retrying it silently would undo a decision.
    if execution.status == "cancelled" {
        return Err(ApiError::bad_request(
            "execution_cancelled",
            "this run was cancelled on purpose, so it cannot be retried; start a new one instead",
        ));
    }
    if execution.status == "running" {
        return Err(ApiError::bad_request(
            "execution_running",
            "this run is still going; wait for it to finish or cancel it first",
        ));
    }

    store::find_step(state.db().pool(), execution_id, step_no)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "step_not_found",
                format!("this run has no step {step_no}"),
            )
        })?;

    let requeued = store::retry_step_from(state.db().pool(), execution_id, step_no).await?;
    if requeued == 0 {
        return Err(ApiError::bad_request(
            "nothing_to_retry",
            "this step and everything after it already ran; there is nothing to try again",
        ));
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, audit_action)
            .organization(execution.organization_id)
            .target("workflow_execution", execution_id.to_string())
            .metadata(json!({ "step_no": step_no, "requeued": requeued }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(RetryResponse {
        execution_id,
        step_no,
        requeued,
    }))
}

fn execution_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "execution_not_found", "no such run")
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Turn a database failure into the answer the rest of this module gives.
fn automation_db_error(err: sqlx::Error) -> ApiError {
    ApiError::from(omnion_audit::AuditError::Database(err))
}

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Load a site and refuse it when it lives outside the caller's organization.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<(), ApiError> {
    let site = omnion_identity::sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))?;
    ensure_same_organization(current, Some(site.organization_id))
}

/// Load a rule and refuse it when its organization is out of the caller's scope.
///
/// A workflow that is not event-triggered is `404` here rather than `403`: it is not this
/// surface's object at all, and answering "no such automation" keeps the two surfaces apart.
async fn automation_in_scope(
    state: &AppState,
    current: &CurrentSession,
    automation_id: Uuid,
) -> Result<omnion_workflows::Workflow, ApiError> {
    let workflow = store::find_workflow(state.db().pool(), automation_id)
        .await?
        .ok_or_else(automation_not_found)?;

    if workflow.trigger() != omnion_workflows::TriggerKind::Event {
        return Err(automation_not_found());
    }

    ensure_same_organization(current, Some(workflow.organization_id))?;
    Ok(workflow)
}

fn automation_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "automation_not_found",
        "no such automation",
    )
}

/// The one answer every failed inbound call gets.
///
/// Unknown token, rotated token, paused rule and a token that is not shaped like one are
/// deliberately indistinguishable: a hook URL is a credential, and an endpoint that says
/// "this token existed but is paused" is an oracle for anyone probing one.
fn hook_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "not found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// A comparison inside an `all` group, as the panel sends it.
    fn equals(field: &str, value: &str) -> Value {
        json!({ "all": [{ "field": field, "operator": "equals", "value": value }] })
    }

    /// A rule that mails somebody, which is enough to make a dry run say something.
    fn input() -> AutomationInput {
        AutomationInput {
            organization_id: None,
            run_as_user_id: None,
            site_id: None,
            name: "  Welcome the editor  ".to_owned(),
            description: "  announces a publication  ".to_owned(),
            enabled: true,
            event: "page.published".to_owned(),
            conditions: equals("status", "published"),
            hook_triggered: false,
            on_error: RuleOnError::Stop,
            rate_limit_per_hour: None,
            concurrency: None,
            actions: vec![StepDefinition::task(
                "tell the editor",
                "send_email",
                json!({
                    "to": "editor@example.com",
                    "subject": "Published: {{event.title}}",
                    "body": "{{event.slug}} is live.",
                }),
            )],
        }
    }

    #[test]
    fn the_catalogue_lists_the_closed_vocabulary() {
        let catalogue = catalogue();

        // The event library carries its payload fields, which is what the condition and
        // binding pickers offer — a rule cannot be written against a field the event lacks.
        let names: Vec<&str> = catalogue.events.iter().map(|event| event.name).collect();
        for name in [
            "page.published",
            "user.created",
            "user.updated",
            "media.created",
            "media.deleted",
            "workflow.execution.completed",
            "ai.run.completed",
        ] {
            assert!(names.contains(&name), "{name} is documented");
        }
        let published = catalogue
            .events
            .iter()
            .find(|event| event.name == "page.published")
            .expect("page.published");
        assert!(
            published
                .fields
                .iter()
                .any(|field| field.key == "revision_id")
        );
        assert!(!published.fields.is_empty());

        let operators: Vec<&str> = catalogue
            .condition_operators
            .iter()
            .map(|operator| operator.key)
            .collect();
        assert_eq!(
            operators,
            vec![
                "equals",
                "not_equals",
                "contains",
                "not_contains",
                "starts_with",
                "ends_with",
                "in",
                "exists",
                "not_exists"
            ]
        );
        assert!(
            catalogue
                .condition_operators
                .iter()
                .any(|operator| operator.key == "exists" && !operator.needs_value)
        );

        let actions: Vec<&str> = catalogue.actions.iter().map(|action| action.key).collect();
        assert_eq!(
            actions,
            vec![
                "noop",
                "echo",
                "fail",
                "transient",
                "send_email",
                "comment_revision",
                "http_request",
                "publish_page",
                "run_workflow"
            ]
        );
        assert!(
            catalogue
                .actions
                .iter()
                .any(|action| action.key == "send_email" && action.host),
            "the two real actions are marked as host actions"
        );
        for action in &catalogue.actions {
            assert!(!action.description.trim().is_empty(), "{}", action.key);
        }

        // The editor's own bounds, and the webhook surface it needs to render.
        assert_eq!(
            catalogue.trigger_kinds,
            vec!["event", "schedule", "manual", "inbound_webhook"]
        );
        assert_eq!(catalogue.group_modes, vec!["all", "any"]);
        assert_eq!(catalogue.max_group_depth, 3);
        assert!(catalogue.max_conditions >= 10);
        assert_eq!(catalogue.hook_event, "automation.hook.received");
        assert_eq!(catalogue.hook_path_template, "/api/v1/hooks/<token>");
        assert!(catalogue.hook_sample["hook"]["body"].is_object());
        assert!(catalogue.binding_syntax.contains("{{event."));

        // The editor can only offer what the catalogue names, so a `branch` step, a step
        // timeout and an outbound method all have to be *here* — otherwise the panel would
        // have to hard-code a second, drifting copy of the vocabulary.
        assert_eq!(
            catalogue
                .branch_operators
                .iter()
                .map(|operator| operator.key)
                .collect::<Vec<_>>(),
            operators,
            "a branch uses the same operators the conditions do"
        );
        assert_eq!(
            catalogue.step_kinds,
            vec!["task", "wait", "branch", "stop", "approval"],
            "the editor offers every kind the engine can run, and no more"
        );
        assert_eq!(
            catalogue.on_error_policies,
            vec!["inherit", "stop", "continue"]
        );
        assert_eq!(catalogue.default_step_timeout_ms, 30_000);
        assert_eq!(catalogue.max_step_timeout_ms, 120_000);
        assert!(catalogue.outbound_methods.contains(&"POST"));
        assert!(!catalogue.outbound_methods.contains(&"TRACE"));
        assert_eq!(catalogue.max_chain_depth, 3);
    }

    #[test]
    fn a_request_becomes_a_checked_rule() {
        let rule = input().rule(Uuid::nil()).expect("the rule is valid");
        assert_eq!(rule.name, "Welcome the editor");
        assert_eq!(rule.description, "announces a publication");
        assert!(rule.definition().is_ok());

        // The stored conditions are the one-key group object, so a rule saved by this layer
        // and a rule saved before it both read the same way.
        let definition = rule.definition().expect("valid");
        let conditions = definition.conditions_json().expect("stores");
        assert!(conditions["all"].is_array(), "{conditions}");
    }

    #[test]
    fn a_rules_own_policy_travels_with_it_in_both_directions() {
        // The body has to carry it: a rule saved with `continue` and read back as `stop`
        // would let the editor reset the policy on the next whole-rule write, and the panel
        // would show a lie in the meantime.
        let rule = input().rule(Uuid::nil()).expect("the rule is valid");
        assert_eq!(
            rule.on_error,
            omnion_workflows::OnError::Stop,
            "the default"
        );

        let mut continuing = input();
        continuing.on_error = RuleOnError::Continue;
        assert_eq!(
            continuing.rule(Uuid::nil()).expect("valid").on_error,
            omnion_workflows::OnError::Continue,
            "and it survives the round trip into the rule the store writes"
        );
    }

    #[test]
    fn a_webhook_rule_ignores_the_event_it_was_sent() {
        let mut request = input();
        request.event = "page.published".to_owned();
        request.hook_triggered = true;

        let rule = request.rule(Uuid::nil()).expect("a webhook rule is valid");
        let definition = rule.definition().expect("valid");
        assert_eq!(
            definition.trigger.event.as_deref(),
            Some("automation.hook.received"),
            "a webhook rule always listens for the reserved name"
        );
    }

    #[test]
    fn a_rule_the_matcher_could_not_run_is_refused_when_it_is_written() {
        let base = |event: &str, conditions: Value, actions: Vec<StepDefinition>| AutomationInput {
            organization_id: None,
            run_as_user_id: None,
            site_id: None,
            name: "rule".to_owned(),
            description: String::new(),
            enabled: true,
            event: event.to_owned(),
            conditions,
            hook_triggered: false,
            on_error: RuleOnError::Stop,
            rate_limit_per_hour: None,
            concurrency: None,
            actions,
        };
        let action = || {
            vec![StepDefinition::task(
                "comment",
                "comment_revision",
                json!({ "revision_id": "{{event.revision_id}}", "body": "live" }),
            )]
        };

        // An event the bus could never record.
        let error = base("Page.Published", json!([]), action())
            .rule(Uuid::nil())
            .expect_err("event name");
        assert_eq!(error.code(), "invalid_event");

        // The reserved hook name, claimed by an event rule.
        let error = base("automation.hook.received", json!([]), action())
            .rule(Uuid::nil())
            .expect_err("reserved");
        assert_eq!(error.code(), "invalid_event");

        // A comparison missing its value, and a group the panel could not lay out.
        let error = base(
            "page.published",
            json!({ "all": [{ "field": "status", "operator": "equals" }] }),
            action(),
        )
        .rule(Uuid::nil())
        .expect_err("condition");
        assert_eq!(error.code(), "invalid_conditions");

        let too_deep = json!({ "all": [{ "all": [{ "all": [{ "all": [{
            "field": "status", "operator": "equals", "value": "published"
        }] }] }] }] });
        let error = base("page.published", too_deep, action())
            .rule(Uuid::nil())
            .expect_err("depth");
        assert_eq!(error.code(), "invalid_conditions");

        // A binding that names something other than the event.
        let error = base(
            "page.published",
            json!([]),
            vec![StepDefinition::task(
                "comment",
                "comment_revision",
                json!({ "revision_id": "{{site.revision_id}}", "body": "live" }),
            )],
        )
        .rule(Uuid::nil())
        .expect_err("binding");
        assert_eq!(error.code(), "invalid_binding");

        // A rule without actions, and one with a blank name.
        let error = base("page.published", json!([]), vec![])
            .rule(Uuid::nil())
            .expect_err("no actions");
        assert_eq!(error.code(), "invalid_steps");

        let mut named = base("page.published", json!([]), action());
        named.name = "   ".to_owned();
        assert_eq!(
            named.rule(Uuid::nil()).expect_err("name").code(),
            "invalid_name"
        );
    }

    #[test]
    fn a_failed_hook_call_answers_the_same_404_every_time() {
        // One body for an unknown token, a rotated one, a paused rule's and a token that is
        // not shaped like one — an endpoint that distinguishes them is an oracle.
        let error = hook_not_found();
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
        assert_eq!(error.code(), "not_found");
        // The body says nothing about rules, tokens or state: it is the same sentence a
        // request to a path that does not exist gets.
        let body = error.into_response();
        assert_eq!(body.status(), StatusCode::NOT_FOUND);
    }
}
