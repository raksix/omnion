//! `/api/v1/hr/onboarding/*` — the templates, the boards and the tick (docs/requests/REQ-055, slice 4)
//!
//! Thin in the same way as the rest of this module, with three decisions the HTTP layer owns:
//!
//! * **Who may tick an item.** The request's own rule: an item can be completed by the assigned
//!   role or by HR. So the route is behind `hr.onboarding.manage`, and the *self-service* twin
//!   (the employee ticking their own "collect identification") lives in `hr_me` with no key at
//!   all — the same split `/hr/me/leave` uses, for the same reason.
//! * **The two events carry ids and titles, never notes.** `hr.onboarding.applied` and
//!   `hr.onboarding.completed` travel to webhooks that may be a third party's, so a note somebody
//!   typed about their own onboarding never goes on the bus. The completion event fires on the
//!   **transition** — see the module's `just_completed` — because firing it on every tick would
//!   send one event per step and an automation waiting for "onboarding finished" would run three
//!   times on a three-step checklist.
//! * **The audit row names the item and its new state**, because "somebody ticked something" is
//!   the one entry an HR audit is ever asked to explain.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_hr::onboarding::{
    self, Checklist, NewTemplate, Template, TemplateChanges, TemplateItem,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::crm::{emit, organization_of};
use crate::routes::iam::record;
use crate::state::AppState;

/// The board's query string.
#[derive(Debug, Default, Deserialize)]
pub struct BoardParams {
    /// The organization, for an instance operator.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// One template's payload, as the editor sends it.
#[derive(Debug, Deserialize)]
pub struct TemplateBody {
    /// What the picker calls it.
    pub name: String,
    /// The ordered steps.
    pub items: Vec<TemplateItem>,
}

/// One item's payload, as the editor sends it.
#[derive(Debug, Deserialize)]
pub struct ItemBody {
    /// The step's text.
    pub title: String,
    /// Who owns it.
    #[serde(default)]
    pub owner_role: Option<String>,
    /// Days after the start date.
    #[serde(default)]
    pub due_offset_days: Option<i32>,
    /// Whether the step wants a file.
    #[serde(default)]
    pub requires_file: bool,
}

impl From<ItemBody> for TemplateItem {
    fn from(body: ItemBody) -> Self {
        Self {
            title: body.title,
            owner_role: body.owner_role,
            due_offset_days: body.due_offset_days,
            requires_file: body.requires_file,
        }
    }
}

/// A template editor's corrections — every field optional, because a PATCH that requires all of
/// them is a PUT with worse manners and a client that sends `{"active": false}` gets a 400 for
/// fields it never meant to touch.
#[derive(Debug, Default, Deserialize)]
pub struct TemplatePatch {
    /// A new name.
    #[serde(default)]
    pub name: Option<String>,
    /// New steps.
    #[serde(default)]
    pub items: Option<Vec<ItemBody>>,
    /// Whether the picker offers it.
    #[serde(default)]
    pub active: Option<bool>,
}

/// A tick's payload.
#[derive(Debug, Deserialize)]
pub struct TickBody {
    /// Whether the item is now done.
    pub done: bool,
    /// The note beside it. Optional, and the only way a note is ever written.
    #[serde(default)]
    pub note: Option<String>,
}

/// `GET /api/v1/hr/onboarding/templates` — the catalogue the picker offers.
pub async fn list_templates(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<BoardParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let templates = onboarding::list_templates(state.db().pool(), organization_id).await?;
    Ok(Json(json!({ "items": templates, "total": templates.len() })))
}

/// `POST /api/v1/hr/onboarding/templates` — a new template.
pub async fn create_template(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<TemplateBody>,
) -> Result<(StatusCode, Json<Template>), ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let template = onboarding::create_template(
        state.db().pool(),
        organization_id,
        &NewTemplate {
            name: body.name,
            items: body.items,
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.onboarding.template_created")
            .organization(organization_id)
            .target("hr_onboarding_template", template.id.to_string())
            .metadata(json!({ "name": template.name, "items": template.items.len() })),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(template)))
}

/// `PATCH /api/v1/hr/onboarding/templates/{id}` — the template editor.
pub async fn update_template(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<TemplatePatch>,
) -> Result<Json<Template>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let changes = TemplateChanges {
        name: body.name,
        items: body.items.map(|items| items.into_iter().map(TemplateItem::from).collect()),
        active: body.active,
    };
    let template = onboarding::update_template(state.db().pool(), organization_id, id, &changes).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.onboarding.template_updated")
            .organization(organization_id)
            .target("hr_onboarding_template", template.id.to_string())
            .metadata(json!({
                "name": template.name,
                "items": template.items.len(),
                "active": template.active,
            })),
    )
    .await?;

    Ok(Json(template))
}

/// `GET /api/v1/hr/onboarding` — the board: everybody with a checklist, with their bar.
pub async fn board(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<BoardParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let checklists = onboarding::board(state.db().pool(), organization_id).await?;

    // The board's own totals, computed here rather than in the browser: "12 of 40 started, 3
    // finished" is a fact about the tenant, and a screen that sums its own cards gets it wrong
    // the moment the board is filtered.
    let total_people = i64::try_from(checklists.len()).unwrap_or_default();
    let in_progress = checklists
        .iter()
        .filter(|checklist| checklist.done > 0 && !checklist.just_completed())
        .count();
    let finished = checklists.iter().filter(|c| c.just_completed()).count();
    let items_total: i64 = checklists.iter().map(|checklist| checklist.total).sum();
    let items_done: i64 = checklists.iter().map(|checklist| checklist.done).sum();

    Ok(Json(json!({
        "items": checklists,
        "totals": {
            "people": total_people,
            "in_progress": in_progress,
            "finished": finished,
            "items_total": items_total,
            "items_done": items_done,
        }
    })))
}

/// `GET /api/v1/hr/onboarding/employees/{id}` — one employee's checklist.
pub async fn checklist(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(employee_id): Path<Uuid>,
) -> Result<Json<Checklist>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let found = onboarding::checklist_of(state.db().pool(), organization_id, employee_id)
        .await?
        .ok_or(ApiError::new(
            StatusCode::NOT_FOUND,
            "employee_not_found",
            "no such employee in this organization",
        ))?;
    Ok(Json(found))
}

/// `POST /api/v1/hr/employees/{id}/onboarding` — apply a template to an employee.
pub async fn apply_template(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(employee_id): Path<Uuid>,
    Json(body): Json<ApplyBody>,
) -> Result<(StatusCode, Json<Checklist>), ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let checklist = onboarding::apply_template(
        state.db().pool(),
        organization_id,
        employee_id,
        body.template_id,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.onboarding.applied")
            .organization(organization_id)
            .target("hr_onboarding_items", employee_id.to_string())
            .metadata(json!({
                "employee_id": employee_id,
                "template_id": body.template_id,
                "items": checklist.total,
            })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("hr.onboarding.applied")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "employee_id": employee_id,
                "template_id": body.template_id,
                "items": checklist.total,
            })),
    )
    .await;

    Ok((StatusCode::CREATED, Json(checklist)))
}

/// An apply's payload.
#[derive(Debug, Deserialize)]
pub struct ApplyBody {
    /// The template to materialise.
    pub template_id: Uuid,
}

/// `PATCH /api/v1/hr/onboarding/items/{id}` — tick or untick one item.
pub async fn tick_item(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(item_id): Path<Uuid>,
    Json(body): Json<TickBody>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;

    let before = onboarding::checklist_for_item(state.db().pool(), organization_id, item_id).await?;
    let checklist = onboarding::tick_item(
        state.db().pool(),
        organization_id,
        item_id,
        body.done,
        body.note.as_deref(),
        current.user.id,
    )
    .await?;
    let after_completed = checklist.just_completed();

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.onboarding.item_updated")
            .organization(organization_id)
            .target("hr_onboarding_item", item_id.to_string())
            .metadata(json!({
                "employee_id": checklist.employee_id,
                "done": body.done,
                "progress": format!("{}/{}", checklist.done, checklist.total),
            })),
    )
    .await?;

    // The completion event fires on the TRANSITION, not on "the checklist is complete now".
    // Ticking an already ticked item, or unticking the last one, are both changes and neither is
    // somebody finishing their onboarding — and an automation waiting for this event would
    // otherwise run once per tick on a checklist that is already finished. `before` is the
    // checklist as it stood before this write, so a tick that finds it already complete stays
    // silent and the untick that breaks it does not claim a completion either.
    if after_completed && before.as_ref().map(Checklist::just_completed) != Some(true) {
        emit(
            &state,
            NewEvent::new("hr.onboarding.completed")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "employee_id": checklist.employee_id,
                    "items": checklist.total,
                })),
        )
        .await;
    }

    Ok(Json(json!({
        "checklist": checklist,
        "completed": after_completed,
    })))
}