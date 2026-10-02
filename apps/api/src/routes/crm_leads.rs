//! `/api/v1/crm/leads/*` — the form → lead ingress inbox and its routing settings
//! (docs/requests/REQ-051, slice 4 part seven; REQ-117).
//!
//! Thin by design, like the rest of the CRM surface. The **rules** — what a submission may
//! become, how a repeat is matched, what the deal is called — live in
//! `modules::crm::leads`, and the drain that reads the bus is driven by
//! `apps/api`'s runner. What the HTTP layer owns here is only the three things a module cannot
//! know:
//!
//! * **The permission.** Reading the ingress log is `crm.leads.read`; deciding what a
//!   submission becomes is `crm.leads.manage`. They are separate keys because they are separate
//!   decisions: a sales manager needs to see that leads arrived without being able to silence
//!   the pipeline, and the person who configures routing is rarely the person who reads it.
//! * **The audit row.** Changing the routing policy changes what every future submission does,
//!   so it is a change with a name and an actor.
//! * **The settings round trip.** `GET` must not *write*: a screen that creates the row it is
//!   about to display turns a read into a change. The read therefore uses
//!   `read_settings_row`, and the drain uses `load_settings`, which does create it — a feature
//!   that only works after somebody has opened this page is a hidden feature.

use axum::Json;
use axum::extract::{Query, State};
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_crm::leads::{
    self, LeadIngest, LeadQuery, LeadSettings, FORM_SUBMITTED,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::crm::{emit, organization_of};
use crate::routes::iam::record;
use crate::routes::crm_deals::OrganizationParam;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests and responses
// ---------------------------------------------------------------------------------------------

/// The inbox's query.
#[derive(Debug, Default, Deserialize)]
pub struct LeadParams {
    /// One of the five outcomes, or nothing for all of them.
    #[serde(default)]
    pub outcome: Option<String>,
    /// Free text over the name, the address, the form key or the answers.
    #[serde(default)]
    pub search: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// How many rows to skip.
    #[serde(default)]
    pub offset: Option<i64>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// What the inbox screen renders: the rows, the counters and where the drain got to.
///
/// The cursor is in the payload rather than in a header because a person reads it as "last
/// checked 4 minutes ago" next to the list, not as a diagnostic.
#[derive(Debug, Serialize)]
pub struct LeadInbox {
    /// The submissions, newest first.
    pub items: Vec<LeadIngest>,
    /// (outcome, count) for the filter chips.
    pub counts: Vec<OutcomeCount>,
    /// The highest bus id the drain has read.
    pub cursor: i64,
    /// When this response was built.
    pub generated_at: OffsetDateTime,
}

/// One filter chip's label and total.
#[derive(Debug, Serialize)]
pub struct OutcomeCount {
    /// The outcome's key.
    pub outcome: String,
    /// How many rows carry it.
    pub count: i64,
}

/// The routing policy as the API carries it.
///
/// Every field is `Option` in the request so an absent key is distinguishable from `false`: a
/// settings form that only sends the two toggles must not blank the two stage pickers.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateLeadSettings {
    /// Turn submissions into contacts.
    #[serde(default)]
    pub create_contact: Option<bool>,
    /// Turn submissions into deals.
    #[serde(default)]
    pub create_deal: Option<bool>,
    /// The stage a **new** lead lands in; `null` means the first open stage.
    #[serde(default)]
    pub stage_id: Option<Option<Uuid>>,
    /// The stage a **repeat** is parked in; `null` means the same as a new lead.
    #[serde(default)]
    pub repeat_stage_id: Option<Option<Uuid>>,
    /// The tag and the deal source a submission is labelled with.
    #[serde(default)]
    pub source_label: Option<String>,
}

/// The settings as read, plus whether a row exists yet — a screen needs to tell "not configured"
/// from "configured to the defaults", and the two look identical without it.
#[derive(Debug, Serialize)]
pub struct LeadSettingsView {
    /// The policy that is in force.
    pub settings: LeadSettings,
    /// `false` when the organization has no row and will get the defaults on the first drain.
    pub configured: bool,
}

// ---------------------------------------------------------------------------------------------
// GET /api/v1/crm/leads
// ---------------------------------------------------------------------------------------------

/// The ingress inbox: every submission the CRM has read, and what became of it.
pub async fn list_leads(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<LeadParams>,
) -> Result<Json<LeadInbox>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;

    let query = LeadQuery {
        outcome: params.outcome,
        search: params.search,
        limit: params.limit.unwrap_or(50),
        offset: params.offset.unwrap_or(0),
    };

    let items = leads::list_leads(state.db().pool(), organization_id, &query).await?;
    let counts = leads::lead_counts(state.db().pool(), organization_id)
        .await?
        .into_iter()
        .map(|(outcome, count)| OutcomeCount { outcome, count })
        .collect();

    Ok(Json(LeadInbox {
        items,
        counts,
        cursor: leads::cursor(state.db().pool()).await?,
        generated_at: OffsetDateTime::now_utc(),
    }))
}

// ---------------------------------------------------------------------------------------------
// GET · PUT /api/v1/crm/leads/settings
// ---------------------------------------------------------------------------------------------

/// Read the routing policy, without creating the row.
pub async fn get_lead_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<OrganizationParam>,
) -> Result<Json<LeadSettingsView>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let existing = leads::read_settings_row(state.db().pool(), organization_id).await?;

    Ok(Json(LeadSettingsView {
        settings: existing.clone().unwrap_or_default(),
        configured: existing.is_some(),
    }))
}

/// Save the routing policy.
pub async fn update_lead_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(params): Query<OrganizationParam>,
    body: Json<UpdateLeadSettings>,
) -> Result<Json<LeadSettingsView>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let pool = state.db().pool();

    // The current row is read first so an absent key keeps what it has. `load_settings` would
    // also create it, and this endpoint must be able to save onto a row that does not exist yet.
    let before = leads::load_settings(pool, organization_id).await?;

    let requested = body.0;
    let after = LeadSettings {
        create_contact: requested.create_contact.unwrap_or(before.create_contact),
        create_deal: requested.create_deal.unwrap_or(before.create_deal),
        stage_id: requested.stage_id.unwrap_or(before.stage_id),
        repeat_stage_id: requested.repeat_stage_id.unwrap_or(before.repeat_stage_id),
        source_label: requested
            .source_label
            .map(|label| label.trim().to_lowercase())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| before.source_label.clone()),
    };

    let saved = leads::save_settings(pool, organization_id, &after).await?;

    // The audit row names the *fields* that changed, not the values: a routing policy is not
    // sensitive, but a diff of booleans is what an operator wants to read back in six months.
    let mut changed: Vec<&str> = Vec::new();
    if before.create_contact != saved.create_contact {
        changed.push("create_contact");
    }
    if before.create_deal != saved.create_deal {
        changed.push("create_deal");
    }
    if before.stage_id != saved.stage_id {
        changed.push("stage_id");
    }
    if before.repeat_stage_id != saved.repeat_stage_id {
        changed.push("repeat_stage_id");
    }
    if before.source_label != saved.source_label {
        changed.push("source_label");
    }

    if !changed.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "crm.lead_settings.updated")
                .organization(organization_id)
                .target("crm_lead_settings", organization_id.to_string())
                .metadata(json!({ "fields": changed }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("crm.lead_settings.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({ "fields": changed })),
        )
        .await;
    }

    Ok(Json(LeadSettingsView {
        settings: saved,
        configured: true,
    }))
}

// ---------------------------------------------------------------------------------------------
// POST /api/v1/crm/leads/drain
// ---------------------------------------------------------------------------------------------

/// Run one drain now, and report what it did.
///
/// The runner does this every few seconds; the endpoint exists so an operator who has just
/// changed the routing policy does not have to wait, and so the QA pass can drive the consumer
/// without a timer in it. It is a **read** of the bus plus the writes the routing implies, so it
/// is guarded by `crm.leads.manage` — the same key that decides what a submission becomes.
pub async fn drain_now(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    // The platform Owner has no primary organization, and the drain is global by construction —
    // it reads the bus, not one tenant's rows. Refusing it there would leave a single-tenant
    // installation with nobody who can press the button.
    if current.user.organization_id.is_none() {
        return Err(ApiError::forbidden(
            "crm.leads.manage",
            "the lead drain is an organization setting",
        ));
    }

    let report = leads::drain(state.db().pool(), leads::DEFAULT_BATCH).await?;

    Ok(Json(json!({
        "cursor": report.cursor,
        "advanced_to": report.advanced_to,
        "created": report.created,
        "merged": report.merged,
        "rejected": report.rejected,
        "orphaned": report.orphaned,
        "disabled": report.disabled,
        "failures": report.failures.len(),
        "idle": report.is_idle(),
        "event": FORM_SUBMITTED,
    })))
}

/// The event name the consumer listens for, as a constant the admin client and the tests both
/// read rather than each spelling it out.
#[must_use]
pub fn subscribed_event() -> &'static str {
    FORM_SUBMITTED
}

/// A path segment the router mounts, kept here so `mod.rs` and the walkthrough agree.
pub const SETTINGS_PATH: &str = "/crm/leads/settings";
