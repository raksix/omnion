//! `/api/v1/crm/*` slice 3: deals, pipelines and the board (docs/requests/REQ-051).
//!
//! Thin by design, exactly like slice 1's file. What the HTTP layer owns here is only the
//! things a module cannot know:
//!
//! * **Which pipelines a caller may see** — the organization's, never one the URL names from
//!   another tenant; and the board's default pipeline when the caller names none.
//! * **The events.** A stage move emits `crm.deal.stage_changed` for every move, plus the
//!   `crm.deal.won` / `crm.deal.lost` outcome events, because those are what the automation
//!   engine and the webhook subscribers key on. The payload carries ids and the changed field
//!   list — never the deal's title, which is the record's free text.
//! * **The audit rows**, with the before/after the platform's audit screen reads.
//!
//! The **rules** stay in `modules/crm::deals`: a lost deal needs a reason whether it arrived
//! through the drag, the keyboard or the JSON, and a rule that exists in two places is a rule
//! that will be true in one of them.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_crm::deals::{
    self, Board, Deal, DealChanges, DealPatch, Pipeline, StageMove, StageSet,
};
use omnion_module_crm::query::{ListQuery, Page, Scope};
use serde::Deserialize;
use serde_json::{Value, json};
use time::Date;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::crm::{emit, organization_of, scope_of};
use crate::routes::iam::record;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// The deal list's query, and the board's `?view=board`.
#[derive(Debug, Default, Deserialize)]
pub struct DealParams {
    /// Free text: title, company or contact.
    #[serde(default)]
    pub search: Option<String>,
    /// `me`, `unassigned` or a user id.
    #[serde(default)]
    pub owner: Option<String>,
    /// A pipeline id.
    #[serde(default)]
    pub pipeline_id: Option<Uuid>,
    /// A company id.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// A contact id.
    #[serde(default)]
    pub contact_id: Option<Uuid>,
    /// Expected close, on or after.
    #[serde(default)]
    pub created_from: Option<Date>,
    /// Expected close, on or before.
    #[serde(default)]
    pub created_to: Option<Date>,
    /// Sort key.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc`.
    #[serde(default)]
    pub direction: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Cursor of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Include the archived deals.
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// `board` (the default) or `list`.
    #[serde(default)]
    pub view: Option<String>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl DealParams {
    fn into_query(self) -> ListQuery {
        ListQuery {
            search: self.search,
            owner: self.owner,
            pipeline_id: self.pipeline_id,
            company_id: self.company_id,
            contact_id: self.contact_id,
            // The deal list's date range filters the **expected close**, which is what a
            // pipeline is planned around: "closing this month" is a close-date question, not a
            // created-at one.
            created_from: self.created_from,
            created_to: self.created_to,
            sort: self.sort,
            direction: self.direction,
            limit: self.limit,
            cursor: self.cursor,
            include_archived: self.include_archived,
            ..ListQuery::default()
        }
    }
}

/// The board: the pipeline, its columns and the cards.
#[derive(Debug, Deserialize)]
pub struct BoardParams {
    /// Which pipeline; the organization's default when absent.
    #[serde(default)]
    pub pipeline_id: Option<Uuid>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of a new deal.
#[derive(Debug, Deserialize)]
pub struct NewDeal {
    /// Headline (required).
    pub title: String,
    /// Pipeline; the organization's default when absent.
    #[serde(default)]
    pub pipeline_id: Option<Uuid>,
    /// Stage; the first open stage when absent.
    #[serde(default)]
    pub stage_id: Option<Uuid>,
    /// Company.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// Contact.
    #[serde(default)]
    pub contact_id: Option<Uuid>,
    /// Owner (defaults to the caller).
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// Value, as text.
    #[serde(default)]
    pub amount: Option<String>,
    /// Currency.
    #[serde(default)]
    pub currency: Option<String>,
    /// Probability.
    #[serde(default)]
    pub probability: Option<i32>,
    /// Expected close.
    #[serde(default)]
    pub expected_close_on: Option<Date>,
    /// Source.
    #[serde(default)]
    pub source: Option<String>,
    /// Required when the chosen stage is a lost one.
    #[serde(default)]
    pub lost_reason: Option<String>,
    /// Organization to write in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of a stage save: the whole ordered set, because a drag reorder is the set itself.
#[derive(Debug, Deserialize)]
pub struct SaveStages {
    /// The stages, in board order.
    pub stages: Vec<StageChangesBody>,
}

/// One stage of the pipeline editor.
#[derive(Debug, Deserialize)]
pub struct StageChangesBody {
    /// Column name.
    pub name: String,
    /// `open`, `won` or `lost`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Default probability.
    #[serde(default)]
    pub probability: Option<i32>,
}

impl From<StageChangesBody> for deals::StageChanges {
    fn from(body: StageChangesBody) -> Self {
        deals::StageChanges {
            name: body.name,
            kind: body.kind,
            probability: body.probability,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The identity of a deal for an event payload: ids and money, never the title.
///
/// A title is a person's own words about a customer, and an automation is a subscriber that may
/// be a third party's webhook — so the payload carries what a rule needs to act and nothing a
/// rule could exfiltrate.
fn deal_ref(deal: &Deal) -> Value {
    json!({
        "deal_id": deal.id,
        "organization_id": deal.organization_id,
        "pipeline_id": deal.pipeline_id,
        "stage_id": deal.stage_id,
        "stage_kind": deal.stage_kind,
        "company_id": deal.company_id,
        "contact_id": deal.contact_id,
        "owner_user_id": deal.owner_user_id,
        "amount": deal.amount,
        "currency": deal.currency,
        "expected_close_on": deal.expected_close_on,
    })
}

/// The pipeline a board is showing, or the organization's default.
async fn resolve_board_pipeline(
    pool: &sqlx::PgPool,
    scope: &Scope,
    requested: Option<Uuid>,
) -> Result<Uuid, ApiError> {
    match requested {
        // A pipeline id from another organization resolves to that organization's own default
        // rather than a 403: the URL named a thing this caller cannot see, and saying so would
        // confirm the pipeline exists.
        Some(id) => Ok(deals::get_pipeline(pool, scope.organization_id, id)
            .await
            .map(|pipeline| pipeline.id)
            .unwrap_or(deals::default_pipeline(pool, scope.organization_id).await?.id)),
        None => Ok(deals::default_pipeline(pool, scope.organization_id).await?.id),
    }
}

/// The deal a caller is acting on, read inside the caller's scope.
async fn scoped_deal(pool: &sqlx::PgPool, scope: &Scope, deal_id: Uuid) -> Result<Deal, ApiError> {
    Ok(deals::get_deal(pool, scope, deal_id).await?)
}

// ---------------------------------------------------------------------------------------------
// Deals
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/crm/deals` — the board (default) or a page of deals (`?view=list`).
pub async fn list_deals(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<DealParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&current, params.organization_id)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let view = params.view.clone().unwrap_or_else(|| "board".to_owned());

    match view.as_str() {
        "board" => {
            let pipeline_id = resolve_board_pipeline(state.db().pool(), &scope, params.pipeline_id).await?;
            let board: Board = deals::board_payload(state.db().pool(), &scope, pipeline_id).await?;
            Ok(Json(json!({ "view": "board", "board": board })))
        }
        // An unknown view is refused rather than silently answered as a list: a screen that
        // asked for a board and got a table would show the table and say nothing about why.
        "list" => {
            let query = params.into_query();
            let page: Page<Deal> = deals::list_deals(state.db().pool(), &scope, &query).await?;
            Ok(Json(json!({ "view": "list", "page": page })))
        }
        other => Err(ApiError::bad_request(
            "invalid_crm_query",
            format!("view is board or list, not \"{other}\""),
        )),
    }
}

/// `GET /api/v1/crm/deals/{id}` — one deal.
pub async fn get_deal(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(deal_id): Path<Uuid>,
) -> Result<Json<Deal>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;
    Ok(Json(scoped_deal(state.db().pool(), &scope, deal_id).await?))
}

/// `POST /api/v1/crm/deals` — create a deal.
pub async fn create_deal(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<NewDeal>,
) -> Result<(StatusCode, Json<Deal>), ApiError> {
    let organization_id = organization_of(&current, body.0.organization_id)?;
    let body = body.0;

    let changes = DealChanges {
        title: body.title,
        pipeline_id: body.pipeline_id,
        stage_id: body.stage_id,
        company_id: body.company_id,
        contact_id: body.contact_id,
        owner_user_id: Some(body.owner_user_id.unwrap_or(current.user.id)),
        amount: body.amount,
        currency: body.currency,
        probability: body.probability,
        expected_close_on: body.expected_close_on,
        source: body.source,
        lost_reason: body.lost_reason,
    };

    let after = deals::create_deal(state.db().pool(), organization_id, current.user.id, &changes).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.deal.created")
            .organization(organization_id)
            .target("crm_deal", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "after": deal_ref(&after),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("crm.deal.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(deal_ref(&after)),
    )
    .await;

    Ok((StatusCode::CREATED, Json(after)))
}

/// `PATCH /api/v1/crm/deals/{id}` — update a deal's own fields.
pub async fn update_deal(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(deal_id): Path<Uuid>,
    body: Json<DealPatch>,
) -> Result<Json<Deal>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;

    let before = scoped_deal(state.db().pool(), &scope, deal_id).await?;
    let after = deals::patch_deal(state.db().pool(), &scope, deal_id, &body.0).await?;

    let changed = deal_changes(&before, &after);
    if !changed.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "crm.deal.updated")
                .organization(organization_id)
                .target("crm_deal", after.id.to_string())
                .metadata(json!({
                    "request_id": after.id,
                    "changed": changed,
                    "before": deal_ref(&before),
                    "after": deal_ref(&after),
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("crm.deal.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({ "deal_id": after.id, "changed": changed })),
        )
        .await;
    }

    Ok(Json(after))
}

/// `DELETE /api/v1/crm/deals/{id}` — archive a deal.
pub async fn archive_deal(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(deal_id): Path<Uuid>,
) -> Result<Json<Deal>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;

    let before = scoped_deal(state.db().pool(), &scope, deal_id).await?;
    let after = deals::archive_deal(state.db().pool(), &scope, deal_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.deal.archived")
            .organization(organization_id)
            .target("crm_deal", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "before": deal_ref(&before),
                "after": deal_ref(&after),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("crm.deal.archived")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(deal_ref(&after)),
    )
    .await;

    Ok(Json(after))
}

/// `POST /api/v1/crm/deals/{id}/stage` — move a deal: the drag, and `ctrl + ←/→`.
///
/// Three events can leave here, and which one is decided by the **target stage's kind** rather
/// than by the route: a move always emits `crm.deal.stage_changed`, and reaching an outcome
/// column additionally emits `crm.deal.won` or `crm.deal.lost`. A subscriber that wants "a deal
/// was lost" therefore does not have to inspect the payload — and a subscriber that only wants
/// "it moved" still gets every move.
pub async fn move_deal_stage(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(deal_id): Path<Uuid>,
    body: Json<StageMove>,
) -> Result<Json<Deal>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;

    let before = scoped_deal(state.db().pool(), &scope, deal_id).await?;
    let after = deals::move_deal_stage(
        state.db().pool(),
        &scope,
        deal_id,
        &body.0,
        today(),
    )
    .await?;

    // A move to the stage the deal is already in is a no-op that must not write an audit row:
    // the board's keyboard path sends the request on every arrow key press, and a person
    // holding `→` in an open column would otherwise wake every automation subscribed to
    // `crm.deal.stage_changed`.
    if before.stage_id != after.stage_id {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "crm.deal.stage_changed")
                .organization(organization_id)
                .target("crm_deal", after.id.to_string())
                .metadata(json!({
                    "request_id": after.id,
                    "from_stage_id": before.stage_id,
                    "to_stage_id": after.stage_id,
                    "from_stage_kind": before.stage_kind,
                    "to_stage_kind": after.stage_kind,
                    "amount": after.amount,
                    "currency": after.currency,
                    "owner_user_id": after.owner_user_id,
                    "before": deal_ref(&before),
                    "after": deal_ref(&after),
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("crm.deal.stage_changed")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "deal_id": after.id,
                    "from_stage_id": before.stage_id,
                    "to_stage_id": after.stage_id,
                    "from_stage_kind": before.stage_kind,
                    "to_stage_kind": after.stage_kind,
                    "amount": after.amount,
                    "currency": after.currency,
                    "owner_user_id": after.owner_user_id,
                })),
        )
        .await;

        if after.stage_kind == "won" {
            emit(
                &state,
                NewEvent::new("crm.deal.won")
                    .organization(organization_id)
                    .actor(current.user.id)
                    .payload(json!({
                        "deal_id": after.id,
                        "stage_id": after.stage_id,
                        "amount": after.amount,
                        "currency": after.currency,
                        "owner_user_id": after.owner_user_id,
                        "close_on": after.expected_close_on,
                    })),
            )
            .await;
        }
        if after.stage_kind == "lost" {
            emit(
                &state,
                NewEvent::new("crm.deal.lost")
                    .organization(organization_id)
                    .actor(current.user.id)
                    .payload(json!({
                        "deal_id": after.id,
                        "stage_id": after.stage_id,
                        "amount": after.amount,
                        "currency": after.currency,
                        "owner_user_id": after.owner_user_id,
                        "lost_reason": after.lost_reason,
                    })),
            )
            .await;
        }
    }

    Ok(Json(after))
}

/// Today in UTC — the day a deal is won when the caller confirms no other date.
fn today() -> Date {
    time::OffsetDateTime::now_utc().date()
}

/// Which deal fields a patch actually changed.
///
/// Typed rather than "diff the two JSON bodies", for the same reason the contact diff is: a
/// patch that touched the title must not report a change to `stage_id` just because the row was
/// re-read.
fn deal_changes(before: &Deal, after: &Deal) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    if before.title != after.title {
        changed.push("title".to_owned());
    }
    if before.company_id != after.company_id {
        changed.push("company_id".to_owned());
    }
    if before.contact_id != after.contact_id {
        changed.push("contact_id".to_owned());
    }
    if before.owner_user_id != after.owner_user_id {
        changed.push("owner_user_id".to_owned());
    }
    if before.amount != after.amount {
        changed.push("amount".to_owned());
    }
    if before.currency != after.currency {
        changed.push("currency".to_owned());
    }
    if before.probability != after.probability {
        changed.push("probability".to_owned());
    }
    if before.expected_close_on != after.expected_close_on {
        changed.push("expected_close_on".to_owned());
    }
    if before.source != after.source {
        changed.push("source".to_owned());
    }
    changed
}

// ---------------------------------------------------------------------------------------------
// Pipelines
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/crm/pipelines` — every pipeline with its stages.
pub async fn list_pipelines(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<Vec<Pipeline>>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    Ok(Json(
        deals::list_pipelines(state.db().pool(), organization_id).await?,
    ))
}

/// `PUT /api/v1/crm/pipelines/{id}/stages` — replace the stages with the editor's set.
pub async fn save_pipeline_stages(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(pipeline_id): Path<Uuid>,
    body: Json<SaveStages>,
) -> Result<Json<Pipeline>, ApiError> {
    let organization_id = organization_of(&current, None)?;

    // The set is validated in the module *before* anything is written, so a refused edit leaves
    // the pipeline exactly as it was rather than half-reordered.
    let set = StageSet {
        stages: body.0.stages.into_iter().map(Into::into).collect(),
    };

    let after = deals::replace_stages(state.db().pool(), organization_id, pipeline_id, &set).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.pipeline.stages_saved")
            .organization(organization_id)
            .target("crm_pipeline", pipeline_id.to_string())
            .metadata(json!({
                "request_id": pipeline_id,
                "stages": after
                    .iter()
                    .map(|stage| json!({
                        "id": stage.id,
                        "name": stage.name,
                        "kind": stage.kind,
                        "position": stage.position,
                        "probability": stage.probability,
                    }))
                    .collect::<Vec<Value>>(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("crm.pipeline.updated")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "pipeline_id": pipeline_id,
                "stage_count": after.len(),
            })),
    )
    .await;

    Ok(Json(deals::get_pipeline(state.db().pool(), organization_id, pipeline_id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    /// A deal row to diff and to build an event payload from.
    fn deal_fixture() -> Deal {
        Deal {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            pipeline_id: Uuid::nil(),
            stage_id: Uuid::nil(),
            stage_name: "New".to_owned(),
            stage_kind: "open".to_owned(),
            title: "A confidential renewal".to_owned(),
            company_id: None,
            company_name: None,
            contact_id: None,
            contact_name: None,
            owner_user_id: None,
            owner_name: None,
            amount: "1000.00".to_owned(),
            currency: "USD".to_owned(),
            probability: Some(10),
            expected_close_on: None,
            source: None,
            lost_reason: None,
            stage_changed_at: time::OffsetDateTime::UNIX_EPOCH,
            days_in_stage: 0,
            stale: false,
            archived_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_event_payload_carries_the_identifier_not_the_title() {
        let deal = deal_fixture();
        let payload = deal_ref(&deal);

        assert_eq!(payload["deal_id"], json!(Uuid::nil()));
        assert_eq!(payload["amount"], json!("1000.00"));
        assert_eq!(payload["currency"], json!("USD"));
        // The title is the record's own words about a customer, and an event subscriber may be
        // a third party's webhook endpoint.
        assert!(
            payload.get("title").is_none(),
            "the payload must not carry the deal's title"
        );
    }

    #[test]
    fn the_diff_names_only_the_fields_a_patch_really_changed() {
        let before = deal_fixture();
        let mut after = before.clone();
        after.amount = "2000.00".to_owned();
        assert_eq!(deal_changes(&before, &after), vec!["amount".to_owned()]);

        let mut after = before.clone();
        after.expected_close_on = Some(date!(2026 - 12 - 01));
        after.currency = "EUR".to_owned();
        assert_eq!(
            deal_changes(&before, &after),
            vec!["currency".to_owned(), "expected_close_on".to_owned()]
        );

        // A patch that changed nothing reports nothing: every opened form would otherwise wake
        // the automations subscribed to `crm.deal.updated`.
        assert!(deal_changes(&before, &before.clone()).is_empty());
    }

    #[test]
    fn the_stage_move_body_reads_a_close_date_and_a_lost_reason() {
        let move_to: StageMove = serde_json::from_value(json!({
            "stage_id": Uuid::nil(),
            "lost_reason": "  chose a competitor  ",
            "close_on": "2026-09-30",
        }))
        .expect("the move body must deserialize");
        assert_eq!(move_to.lost_reason.as_deref(), Some("  chose a competitor  "));
        assert_eq!(move_to.close_on, Some(date!(2026 - 09 - 30)));

        // A bare move is legal: an open stage needs nothing.
        let bare: StageMove = serde_json::from_value(json!({ "stage_id": Uuid::nil() }))
            .expect("a bare move must deserialize");
        assert!(bare.lost_reason.is_none());
        assert!(bare.close_on.is_none());
    }

    #[test]
    fn the_pipeline_editors_body_becomes_the_modules_own_set() {
        let body: SaveStages = serde_json::from_value(json!({
            "stages": [
                { "name": "New", "kind": "open", "probability": 10 },
                { "name": "Won", "kind": "won", "probability": 100 },
                { "name": "Lost", "kind": "lost", "probability": 0 }
            ]
        }))
        .expect("the editor body must deserialize");

        let set = StageSet {
            stages: body.stages.into_iter().map(Into::into).collect(),
        };
        assert_eq!(set.stages.len(), 3);
        assert_eq!(set.stages[0].name, "New");
        assert_eq!(set.stages[2].kind.as_deref(), Some("lost"));
        assert!(deals::validate_stage_set(&set).is_ok());
    }
}
