//! `/api/v1/hr/me/*` — the caller's own HR record (docs/requests/REQ-055, slice 2c).
//!
//! **No route in this file has a `route_layer`.** That is the design, not an oversight, and it is
//! the difference the request's own acceptance criterion names: every other `/hr/*` route answers
//! behind an `hr.*` key, which is exactly what makes them unusable for the person they exist for.
//! An employee with no HR role must still be able to read their own holiday balance, ask for leave
//! and see their contract — that is the `own` level the visibility model was built for.
//!
//! # What replaces the permission check
//!
//! Not "no check" — a different check, and it is made of three parts:
//!
//! 1. **The session.** A `CurrentSession` extractor, so an anonymous caller gets `401` from the
//!    platform rather than an empty page.
//! 2. **The tenancy scope.** [`organization_of`] resolves the caller's own organization, so a
//!    request cannot name a tenant it does not belong to.
//! 3. **The employee row.** The subject of every answer is resolved from
//!    [`omnion_module_hr::me`]'s own lookups against `user_id`, never from a path or query
//!    parameter. **There is no `employee_id` parameter in this file** — that absence is the
//!    security property, and adding one would turn "my leave" into "any leave" for anyone who can
//!    edit a URL.
//!
//! # The `403` that does happen here
//!
//! Never for the self read. An account with no employee row gets a `404` naming the *employee*:
//! a real platform state (an account created before HR, or one for a service integration) that is
//! not a permission problem, and answering it `403` would send the person to an admin when the
//! honest answer is "you are not in the directory yet".

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_hr::me::{self, MyProfile};
use omnion_module_hr::{HrError, leave as hr_leave, requests as hr_requests};
use time::Date;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::crm::{emit, organization_of};
use crate::routes::iam::record;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// The query string of the self-service reads.
///
/// Only two knobs, and both are filters over the caller's **own** rows. `year` is a year, not an
/// id: a person asking for last year's holiday is asking a question about themselves, and there
/// is no reason to make that a permission problem.
#[derive(Debug, Default, Deserialize)]
pub struct MyParams {
    /// Which year's balance and requests to read, defaulting to the current one.
    #[serde(default)]
    pub year: Option<i32>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The year a self-service read is about, defaulting to the current one.
fn year_of(value: Option<i32>) -> i32 {
    value.unwrap_or_else(|| time::OffsetDateTime::now_utc().year())
}

/// The body of a self-service leave request.
///
/// **No `employee_id` field, and that is the shape of the security model rather than an omission
/// of convenience.** The HR route's body carries one because an HR person books leave for other
/// people; this route is the employee's own button, and a subject field here would be a field the
/// caller can set to somebody else's id. Adding it later would be a one-line change that turns a
/// self-service surface into a directory read, so the type is where that decision is recorded.
#[derive(Debug, Deserialize)]
pub struct MyLeaveRequestBody {
    /// The type (required).
    pub leave_type_id: Uuid,
    /// The first day away.
    #[serde(with = "omnion_module_hr::dates")]
    pub starts_on: Date,
    /// The last day away.
    #[serde(with = "omnion_module_hr::dates")]
    pub ends_on: Date,
    /// Whether it is a half-day.
    #[serde(default)]
    pub half_day: Option<bool>,
    /// Why.
    #[serde(default)]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// The routes
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/hr/me` — the caller's own employee record.
///
/// The one route the API table names with "self (no permission)", and the reason it is a route
/// rather than a query on `/hr/employees`: a person must be able to open their own record without
/// holding the key that reads the directory. `own` is therefore a *resolution* here, not a
/// filter somebody chose.
pub async fn get_me(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<MyParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let profile: MyProfile = me::my_profile(state.db().pool(), organization_id, current.user.id)
        .await?
        .ok_or(ApiError::from(HrError::NotFound("employee")))?;
    Ok(Json(json!(profile)))
}

/// `GET /api/v1/hr/me/leave` — the caller's own balances and requests in one answer.
///
/// One document rather than two endpoints, and the reason is a person trying to reconcile: a
/// balance card reading "8 days remaining" next to an empty list is a card they cannot check
/// against anything. The screen needs both to be on screen at once, so the server ships both at
/// once and the two can never disagree by being fetched a minute apart.
pub async fn my_leave(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<MyParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let answer = me::my_leave(
        state.db().pool(),
        organization_id,
        current.user.id,
        year_of(params.year),
    )
    .await?;
    Ok(Json(json!(answer)))
}

/// `GET /api/v1/hr/me/leave/types` — the catalogue the self-service request form offers.
///
/// A person with no `hr.leave.read` still has to be able to *ask* for leave, and the form's type
/// select is the first thing it needs. The types are an organization's published catalogue —
/// names, entitlements and whether approval is needed — and they are the same rows `/hr/leave/types`
/// returns, so the self-service form and the HR screen cannot offer different holidays.
pub async fn my_leave_types(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<MyParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let types = hr_leave::list_leave_types(state.db().pool(), organization_id).await?;
    Ok(Json(json!({ "items": types })))
}

/// `GET /api/v1/hr/me/leave/requests/{id}` — one of the caller's own requests, in full.
///
/// The detail carries the balance and the decision timeline, so a person can see why a request
/// was refused without opening a screen that shows everybody's leave. The ownership test is
/// inside the store call: an id belonging to somebody else is a `404`, never a `403`, because a
/// `403` would confirm that the id exists.
pub async fn my_leave_request(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<MyParams>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let (employee_id, _) =
        me::my_leave_scope(state.db().pool(), organization_id, current.user.id).await?;
    // The ownership test is on the **row the store returned**, not on the id the caller sent: a
    // request that exists but belongs to somebody else has to read as missing, and a `403` would
    // confirm the id is real. `request_detail` itself refuses a request from another
    // organization, so this check narrows the remaining case — the same tenant, another person.
    let detail = hr_requests::request_detail(state.db().pool(), organization_id, id, false).await?;
    if detail.request.employee_id != employee_id {
        return Err(ApiError::from(HrError::NotFound("leave request")));
    }
    Ok(Json(json!(detail)))
}

/// `POST /api/v1/hr/me/leave/requests` — ask for leave, for oneself.
///
/// The body carries no `employee_id`, which is the whole point: the subject is the session. This
/// is the one **write** in the file, and it is safe for the same reason the reads are — the store
/// resolves the employee from the account, so the widest thing this route can do is book the
/// caller's own holiday.
///
/// It is a self-service route rather than a call into the HR-guarded create for two reasons: the
/// caller may hold no `hr.leave.request` key at all, and the HR route's contract allows a body to
/// name somebody else, which is a capability this surface must not have.
pub async fn create_my_leave_request(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<MyLeaveRequestBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let (employee_id, _) =
        me::my_leave_scope(state.db().pool(), organization_id, current.user.id).await?;
    let created = me::create_own_request(
        state.db().pool(),
        organization_id,
        employee_id,
        body.leave_type_id,
        body.starts_on,
        body.ends_on,
        body.half_day.unwrap_or(false),
        body.reason,
    )
    .await?;
    // **The audit row and the event are not an HR privilege.** They are written here by the same
    // two helpers the HR route uses, from the same `event_ref`, precisely because "the employee
    // has no key" must not also mean "what the employee did is invisible". The absence here was a
    // real hole: this file had two writes and zero of either, so the most common leave action in
    // any organization — a person booking and withdrawing their own holiday — left no trail an
    // auditor could read, and fired nothing an automation could wait on. Both twins
    // (`hr_leave::create_request` / `cancel_request`) do exactly this; a surface whose whole
    // argument is that it is *the same action* through a shorter path cannot be the one surface
    // that is not recorded.
    //
    // The payload is `event_ref()`, which carries ids and dates and never the `reason` — that
    // string is the employee's own words and this event travels to third-party webhooks.
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.leave.requested")
            .organization(organization_id)
            .target("hr_leave_request", created.id.to_string())
            .metadata(json!({
                "employee_id": created.employee_id,
                "leave_type_id": created.leave_type_id,
                "starts_on": created.starts_on,
                "ends_on": created.ends_on,
                "days": created.days,
                "leave_status": created.leave_status,
                "self_service": true,
            }))
            .ip_address(address.as_text()),
    )
    .await?;
    emit(
        &state,
        NewEvent::new("hr.leave.requested")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(created.event_ref()),
    )
    .await;

    // A type that needs no approval is approved on creation, and the twin emits the same second
    // event for the same reason: an automation waiting on `hr.leave.approved` must not silently
    // never fire in the organization that turned approval off. An employee who never learns they
    // must request in advance is the person most likely to be off without the record.
    if created.leave_status == "approved" {
        emit(
            &state,
            NewEvent::new("hr.leave.approved")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(created.event_ref()),
        )
        .await;
    }
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// `POST /api/v1/hr/me/leave/requests/{id}/cancel` — withdraw a pending request of one's own.
///
/// Withdrawing leave you asked for is not a privileged act and must not need HR: it is the single
/// most common correction in any leave system, and requiring a permission to unsend a request
/// means people stop asking in the first place. Only a `pending` request can be withdrawn, so this
/// can never erase an approval.
pub async fn cancel_my_leave_request(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let (employee_id, _) =
        me::my_leave_scope(state.db().pool(), organization_id, current.user.id).await?;
    let cancelled = me::cancel_own_request(state.db().pool(), organization_id, employee_id, id).await?;
    // The withdrawal is audited and announced for the same reason the request above is, and the
    // pair matters more than either alone: a request that was raised and then quietly withdrawn
    // is the one a balance dispute is actually about, and "when did they decide to cancel that?"
    // is answerable only because the cancel left a row of its own.
    //
    // The **actor is `current.user.id`**, not the employee id the store was given. Those are two
    // different rows (`hr_employees.id` versus `users.id`), and the audit trail's `actor_user_id`
    // column is about the account that acted — the same id the twin writes. The store takes the
    // employee id only to prove ownership; it discards its `cancelled_by` argument
    // (`let _ = cancelled_by;` in `requests::cancel_request`), so the identity of the actor is
    // this route's responsibility and nothing else records it.
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.leave.cancelled")
            .organization(organization_id)
            .target("hr_leave_request", cancelled.id.to_string())
            .metadata(json!({
                "employee_id": cancelled.employee_id,
                "days": cancelled.days,
                "self_service": true,
            }))
            .ip_address(address.as_text()),
    )
    .await?;
    emit(
        &state,
        NewEvent::new("hr.leave.cancelled")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(cancelled.event_ref()),
    )
    .await;
    Ok(Json(json!(cancelled)))
}

/// `GET /api/v1/hr/me/leave/preview` — the days a self-service request would charge.
///
/// This exists because the form's day counter is the one number an employee has to trust before
/// they commit, and the route that computes it (`/hr/leave/requests/preview`) sits behind
/// `hr.leave.read` — the very key this surface is built for people who do not hold. Without it the
/// form would either omit the counter or re-implement the arithmetic in TypeScript, and the second
/// is how a preview starts promising five days for a request the store then refuses for having
/// three. Both routes call the same `request_days_hundredths`, so the two answers cannot drift.
///
/// No employee id, no permission: it is a pure function of two dates.
pub async fn my_leave_preview(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<MyPreviewParams>,
) -> Result<Json<Value>, ApiError> {
    let _organization_id = organization_of(&state, &current, None).await?;
    let days = omnion_module_hr::leave::request_days_hundredths(
        params.starts_on,
        params.ends_on,
        params.half_day.unwrap_or(false),
    );
    if days <= 0 {
        return Err(ApiError::from(HrError::invalid(
            "leave request",
            "starts_on",
            "that range contains no working day, so there is nothing to request",
        )));
    }
    Ok(Json(json!({
        "starts_on": params.starts_on,
        "ends_on": params.ends_on,
        "days": omnion_module_hr::leave::days_to_text(days),
    })))
}

/// The query string of the self-service day preview.
#[derive(Debug, Deserialize)]
pub struct MyPreviewParams {
    /// The first day away.
    #[serde(with = "omnion_module_hr::dates")]
    pub starts_on: Date,
    /// The last day away.
    #[serde(with = "omnion_module_hr::dates")]
    pub ends_on: Date,
    /// Whether it is a half-day.
    #[serde(default)]
    pub half_day: Option<bool>,
}

/// `GET /api/v1/hr/me/documents` — the caller's own documents, newest first.
///
/// Present in slice 2c although the request puts documents in slice 4, because the *read* needs
/// no new table: `hr_documents` arrived with the people core in migration 0196 and has been sitting
/// there with no route, which means a contract uploaded by HR is currently unreadable by the only
/// person who is supposed to have it. The upload and expiry events stay in slice 4.
pub async fn my_documents(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<MyParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let documents = me::my_documents(state.db().pool(), organization_id, current.user.id).await?;
    Ok(Json(json!({ "items": documents })))
}
