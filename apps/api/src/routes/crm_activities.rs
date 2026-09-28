//! `/api/v1/crm/*` slice 4, part one: the activity feed and the merged record timeline
//! (docs/requests/REQ-051).
//!
//! Thin by design, exactly like slices 1 and 3. What the HTTP layer owns here is only the things
//! a module cannot know:
//!
//! * **The audit row and the event.** Logging an activity is a write a person made on purpose, so
//!   it is audited and it emits `crm.activity.logged` — which is what an automation subscribes
//!   to in order to react to a call being logged.
//! * **The audit row and the event for closing a task**, the same two.
//!
//! The payload carries ids, the kind and the changed field list — **never the subject or the
//! body**, which are the record's own words about a person. An event is a thing a third party
//! may receive, and a webhook subscriber does not need the contents of someone's call note to
//! know that a call happened.
//!
//! The **rules** stay in `modules/crm::activities`: an activity hangs off exactly one record, a
//! task needs a date, and a rule that exists in two places is a rule that will be true in one of
//! them.

use axum::Json;
use axum::extract::{Path, Query, State};
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_crm::activities::{
    self, Activity, ActivityChanges, TimelineEntry, set_activity_done,
};
use omnion_module_crm::query::{ListQuery, Page};
use serde::Deserialize;
use serde_json::json;
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

/// The feed's query: the shared list contract plus the two filters activities add.
#[derive(Debug, Default, Deserialize)]
pub struct ActivityParams {
    /// Free text over the subject and the body.
    #[serde(default)]
    pub search: Option<String>,
    /// One of the four kinds.
    #[serde(default)]
    pub kind: Option<String>,
    /// `open`, `done`, or neither.
    #[serde(default)]
    pub done: Option<String>,
    /// `me`, `unassigned` or a user id.
    #[serde(default)]
    pub owner: Option<String>,
    /// The company an activity hangs off.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// The contact an activity hangs off.
    #[serde(default)]
    pub contact_id: Option<Uuid>,
    /// The deal an activity hangs off.
    #[serde(default)]
    pub deal_id: Option<Uuid>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

impl From<ActivityParams> for ListQuery {
    fn from(params: ActivityParams) -> Self {
        Self {
            search: params.search,
            owner: params.owner,
            company_id: params.company_id,
            contact_id: params.contact_id,
            deal_id: params.deal_id,
            limit: params.limit,
            cursor: params.cursor,
            ..Self::default()
        }
    }
}

/// The timeline's query: how much of it the screen wants.
#[derive(Debug, Default, Deserialize)]
pub struct TimelineParams {
    /// How many entries, capped by the module at 200.
    #[serde(default)]
    pub limit: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// GET /api/v1/crm/activities
// ---------------------------------------------------------------------------------------------

/// The activity feed: every activity the caller may see, newest first.
pub async fn list_activities(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ActivityParams>,
) -> Result<Json<Page<Activity>>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;

    // `kind` and `done` are *this screen's* filters, not part of the shared `ListQuery` the
    // contacts, deals and companies lists also send, so they travel beside it rather than in it.
    // They are read before the conversion, because `From<ActivityParams>` consumes the struct and
    // the two fields are not part of what it produces.
    let kind = params.kind.clone();
    let done = params.done.clone();
    let query: ListQuery = params.into();

    let page =
        activities::list_activities_with(state.db().pool(), &scope, &query, kind.as_deref(), done.as_deref())
            .await?;
    Ok(Json(page))
}

// ---------------------------------------------------------------------------------------------
// POST /api/v1/crm/activities
// ---------------------------------------------------------------------------------------------

/// Log an activity.
pub async fn create_activity(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<ActivityChanges>,
) -> Result<(axum::http::StatusCode, Json<Activity>), ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;

    let activity = activities::log_activity(
        state.db().pool(),
        &scope,
        current.user.id,
        &body.0,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.activity.logged")
            .organization(organization_id)
            .target("crm_activity", activity.id.to_string())
            .metadata(json!({
                "request_id": activity.id,
                "kind": activity.kind,
                "company_id": activity.company_id,
                "contact_id": activity.contact_id,
                "deal_id": activity.deal_id,
                "occurred_at": activity.occurred_at,
                "due_at": activity.due_at,
                "fields": body.0.changed_fields(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    // The payload names what happened, never what was said: a webhook subscriber needs to know a
    // call was logged on a deal, not read the note somebody took during it.
    emit(
        &state,
        NewEvent::new("crm.activity.logged")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "activity_id": activity.id,
                "kind": activity.kind,
                "company_id": activity.company_id,
                "contact_id": activity.contact_id,
                "deal_id": activity.deal_id,
                "occurred_at": activity.occurred_at,
                "due_at": activity.due_at,
            })),
    )
    .await;

    Ok((axum::http::StatusCode::CREATED, Json(activity)))
}

// ---------------------------------------------------------------------------------------------
// POST /api/v1/crm/activities/{id}/done
// ---------------------------------------------------------------------------------------------

/// Close a task, or open it again.
pub async fn complete_activity(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(activity_id): Path<Uuid>,
    body: Json<CompleteBody>,
) -> Result<Json<Activity>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;

    let activity = set_activity_done(state.db().pool(), &scope, activity_id, body.0.done).await?;

    record(
        &state,
        NewAuditEntry::by_user(
            current.user.id,
            if body.0.done {
                "crm.activity.completed"
            } else {
                "crm.activity.reopened"
            },
        )
        .organization(organization_id)
        .target("crm_activity", activity.id.to_string())
        .metadata(json!({
            "request_id": activity.id,
            "kind": activity.kind,
            "contact_id": activity.contact_id,
            "company_id": activity.company_id,
            "deal_id": activity.deal_id,
            "done_at": activity.done_at,
        }))
        .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(activity))
}

/// The body of a close/reopen.
#[derive(Debug, Default, Deserialize)]
pub struct CompleteBody {
    /// `true` closes the task, `false` opens it again.
    #[serde(default)]
    pub done: bool,
}

// ---------------------------------------------------------------------------------------------
// GET /api/v1/crm/{record}/{id}/timeline
// ---------------------------------------------------------------------------------------------

/// The merged timeline of one contact.
pub async fn contact_timeline(
    state: State<AppState>,
    current: CurrentSession,
    id: Path<Uuid>,
    params: Query<TimelineParams>,
) -> Result<Json<Page<TimelineEntry>>, ApiError> {
    timeline(state, current, id, params, "contact").await
}

/// The merged timeline of one company.
pub async fn company_timeline(
    state: State<AppState>,
    current: CurrentSession,
    id: Path<Uuid>,
    params: Query<TimelineParams>,
) -> Result<Json<Page<TimelineEntry>>, ApiError> {
    timeline(state, current, id, params, "company").await
}

/// The merged timeline of one deal.
pub async fn deal_timeline(
    state: State<AppState>,
    current: CurrentSession,
    id: Path<Uuid>,
    params: Query<TimelineParams>,
) -> Result<Json<Page<TimelineEntry>>, ApiError> {
    timeline(state, current, id, params, "deal").await
}

/// The one handler the three share.
///
/// The screen asks the same question of all three records and the answer has the same shape, so
/// the difference is the record's kind and nothing else. The three routes are declared
/// separately in the router rather than as `/crm/{record}/{id}/timeline`: a param next to the
/// existing `/crm/deals/{id}/stage` is a route table where only one of the two can win, and the
/// loser is a 404 nobody can explain.
async fn timeline(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<TimelineParams>,
    record: &'static str,
) -> Result<Json<Page<TimelineEntry>>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;

    let page =
        activities::record_timeline(state.db().pool(), &scope, record, id, params.limit.unwrap_or(50))
            .await?;
    Ok(Json(page))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use serde_json::Value;

    /// The body an `ApiError` produces, read through its own `IntoResponse` — the shape a client
    /// actually receives, nested under `error`.
    async fn body_of(error: ApiError) -> Value {
        let response = error.into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("an error body is small");
        serde_json::from_slice(&bytes).expect("the error body is JSON")
    }

    #[tokio::test]
    async fn an_activity_with_no_record_names_the_field_the_form_has() {
        // The form posts an empty attachment; the refusal has to arrive *at* the contact field or
        // the person reads it in a banner and has to guess which of three fields is wrong.
        let error = ApiError::from(omnion_module_crm::CrmError::invalid(
            "activity",
            "contact_id",
            "an activity needs a company, a contact or a deal to hang off",
        ));
        let body = body_of(error).await;
        let details = &body["error"]["details"];
        assert_eq!(details["entity"], "activity", "{body}");
        assert_eq!(details["field"], "contact_id", "{body}");
    }

    #[tokio::test]
    async fn a_timeline_for_something_that_is_not_a_record_is_a_400() {
        let error = ApiError::from(omnion_module_crm::CrmError::InvalidQuery(
            "a timeline is read for a contact, a company or a deal".to_owned(),
        ));
        let body = body_of(error).await;
        assert_eq!(body["error"]["code"], "invalid_list_query", "{body}");
    }

    #[test]
    fn the_feed_query_carries_the_two_shared_filters_and_the_limit() {
        let params: ActivityParams = serde_json::from_value(json!({
            "search": "call", "kind": "call", "done": "open", "limit": 25
        }))
        .expect("the feed's query deserialises");
        let query: ListQuery = params.into();
        assert_eq!(query.search.as_deref(), Some("call"));
        assert_eq!(query.limit, Some(25));
    }

    #[test]
    fn a_default_complete_body_opens_rather_than_closes() {
        // `{}` must not silently close somebody's task: the body is only sent by the button that
        // means "close", and a malformed one should not flip state in the other direction.
        let body: CompleteBody = serde_json::from_value(json!({})).expect("an empty body parses");
        assert!(!body.done);
        let open: CompleteBody =
            serde_json::from_value(json!({ "done": false })).expect("explicit false parses");
        assert!(!open.done);
    }
}
