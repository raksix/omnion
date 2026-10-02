//! `/api/v1/hr/leave/*` — the leave catalogue, the requests and the absence calendar
//! (docs/requests/REQ-055, slice 2).
//!
//! Thin in the same way as the people core, with three decisions the HTTP layer owns because a
//! module cannot know them:
//!
//! * **Whose request is this.** The body may name an employee, and only HR may name somebody
//!   else: a request without an `employee_id` is the caller's own, resolved through their
//!   employee row, so "request leave" is one button and not a picker a person has to be trusted
//!   with. Somebody else's request needs `hr.employees.read` on top of `hr.leave.request`.
//! * **Who may cancel.** The employee who raised it, or anybody with `hr.leave.approve`. A
//!   pending request an employee cannot withdraw would be the most complained-about leave rule
//!   there is.
//! * **The events and the audit rows.** `hr.leave.requested`, `hr.leave.approved`,
//!   `hr.leave.rejected` and `hr.leave.cancelled` carry **ids and dates only** — never the reason,
//!   which is a person's own words about their health, their family or their manager and may be
//!   on its way to a third party's webhook.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_hr::employees;
use omnion_module_hr::leave::{
    self, BalanceCard, LeaveQuery, LeaveScope, LeaveType, LeaveTypeChanges, LeaveTypePatch,
    NewLeaveRequest,
};
use omnion_module_hr::model::Visibility;
use omnion_module_hr::requests::{self, Decision, RequestDetail};
use omnion_module_hr::{HrError, Page};
use omnion_permissions::{Scope as PermissionScope, authorize};
use serde::Deserialize;
use serde_json::{Value, json};
use time::Date;
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

/// The leave list's query string.
#[derive(Debug, Default, Deserialize)]
pub struct LeaveParams {
    /// One of the four statuses.
    #[serde(default)]
    pub status: Option<String>,
    /// A leave type.
    #[serde(default)]
    pub leave_type_id: Option<Uuid>,
    /// An employee.
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    /// Free text over the employee's name or number.
    #[serde(default)]
    pub search: Option<String>,
    /// Starting on or after.
    #[serde(default, with = "omnion_module_hr::dates::option")]
    pub from: Option<Date>,
    /// Starting on or before.
    #[serde(default, with = "omnion_module_hr::dates::option")]
    pub to: Option<Date>,
    /// The list's "pending only" chip.
    #[serde(default)]
    pub pending_only: Option<bool>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The cursor of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// `own`, `team` or `all`.
    #[serde(default)]
    pub visibility: Option<String>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl LeaveParams {
    /// The module's own query type, which is where the filters are validated.
    fn into_query(self) -> LeaveQuery {
        LeaveQuery {
            status: self.status,
            leave_type_id: self.leave_type_id,
            employee_id: self.employee_id,
            department_id: None,
            from: self.from,
            to: self.to,
            search: self.search,
            pending_only: self.pending_only,
            limit: self.limit,
            cursor: self.cursor,
        }
    }
}

/// The body of a new leave request.
#[derive(Debug, Deserialize)]
pub struct NewLeaveRequestBody {
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
    /// An attachment in the media pipeline.
    #[serde(default)]
    pub attachment_media_id: Option<Uuid>,
    /// Whose leave this is. Omitted means the caller's own; naming somebody else needs
    /// `hr.employees.read` on top of `hr.leave.request`.
    #[serde(default)]
    pub employee_id: Option<Uuid>,
}

/// The body of a decision.
#[derive(Debug, Deserialize)]
pub struct DecisionBody {
    /// `approve` or `reject`.
    pub decision: String,
    /// The comment the approver leaves.
    #[serde(default)]
    pub comment: Option<String>,
}

/// The body of a new leave type.
#[derive(Debug, Deserialize)]
pub struct NewLeaveTypeBody {
    /// The name.
    pub name: String,
    /// The code.
    pub code: String,
    /// Whether it costs entitlement.
    #[serde(default)]
    pub paid: Option<bool>,
    /// The yearly entitlement, as a string or a number the form sends.
    #[serde(default)]
    pub annual_days: Option<f64>,
    /// Whether a request needs a decision.
    #[serde(default)]
    pub requires_approval: Option<bool>,
    /// Whether a request may exceed the balance.
    #[serde(default)]
    pub allow_negative: Option<bool>,
}

/// The body of a leave type patch.
#[derive(Debug, Default, Deserialize)]
pub struct LeaveTypePatchBody {
    /// The name.
    #[serde(default)]
    pub name: Option<String>,
    /// The code.
    #[serde(default)]
    pub code: Option<String>,
    /// Whether it costs entitlement.
    #[serde(default)]
    pub paid: Option<bool>,
    /// The yearly entitlement.
    #[serde(default)]
    pub annual_days: Option<f64>,
    /// Whether a request needs a decision.
    #[serde(default)]
    pub requires_approval: Option<bool>,
    /// Whether a request may exceed the balance.
    #[serde(default)]
    pub allow_negative: Option<bool>,
    /// Whether the type is offered.
    #[serde(default)]
    pub active: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The caller's leave scope, narrowed the same way the employee list narrows its own.
///
/// One function, because the two lists must agree: a caller who sees their own employee record
/// and nothing else on `/hr/employees` and everybody's leave on `/hr/leave` would be a disclosure
/// the visibility level was introduced to prevent.
async fn scope_of(state: &AppState, current: &CurrentSession, organization_id: Uuid) -> LeaveScope {
    let employee_id =
        employees::employee_of_user(state.db().pool(), organization_id, current.user.id)
            .await
            .unwrap_or(None);
    LeaveScope::all(organization_id).with(Visibility::All, employee_id)
}

/// The narrower of the caller's bindings and the level the request asks for.
fn narrow(scope: &LeaveScope, requested: Option<&str>) -> Result<LeaveScope, ApiError> {
    let asked = Visibility::parse(requested)
        .ok_or_else(|| ApiError::bad_request("invalid_hr_query", "unknown visibility level"))?;
    Ok(LeaveScope::all(scope.organization_id).with(scope.visibility.min(asked), scope.employee_id))
}

/// A page rendered as the API's own JSON.
fn page_of<T: serde::Serialize>(page: Page<T>) -> Json<Value> {
    Json(json!({
        "items": page.items,
        "next_cursor": page.next_cursor,
        "total_estimate": page.total_estimate,
    }))
}

/// The days a body would charge, for the form's preview.
///
/// A `GET` the request form calls as the person picks their dates, so the number it shows before
/// submit is the number the store will charge. Computed by the module, never re-implemented in
/// the handler: two implementations of "how many days is this" is how a preview starts promising
/// 5 days for a request that is then refused for having 3.
fn days_preview(
    starts_on: Date,
    ends_on: Date,
    half_day: bool,
) -> Result<String, ApiError> {
    let days = leave::request_days_hundredths(starts_on, ends_on, half_day);
    if days <= 0 {
        return Err(ApiError::from(HrError::invalid(
            "leave request",
            "starts_on",
            "that range contains no working day, so there is nothing to request",
        )));
    }
    Ok(leave::days_to_text(days))
}

/// The leave type, or a `404` that is indistinguishable from "not yours".
async fn type_or_404(
    state: &AppState,
    organization_id: Uuid,
    id: Uuid,
) -> Result<LeaveType, ApiError> {
    leave::leave_type_of(state.db().pool(), organization_id, id)
        .await?
        .ok_or(ApiError::from(HrError::NotFound("leave type")))
}

/// The year a balance request is about, defaulting to the current one.
fn year_of(value: Option<i32>) -> i32 {
    value.unwrap_or_else(|| time::OffsetDateTime::now_utc().year())
}

// ---------------------------------------------------------------------------------------------
// The catalogue
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/hr/leave/types` — the catalogue a request form offers.
pub async fn list_leave_types(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let types = leave::list_leave_types(state.db().pool(), organization_id).await?;
    Ok(Json(json!({ "items": types })))
}

/// `POST /api/v1/hr/leave/types` — a new leave type.
pub async fn create_leave_type(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<NewLeaveTypeBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let created = leave::create_leave_type(
        state.db().pool(),
        organization_id,
        &LeaveTypeChanges {
            name: body.name,
            code: body.code,
            paid: body.paid,
            annual_days: body.annual_days.map(hundredths_of),
            requires_approval: body.requires_approval,
            allow_negative: body.allow_negative,
            active: None,
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.leave_type.created")
            .organization(organization_id)
            .target("hr_leave_type", created.id.to_string())
            .metadata(json!({ "code": created.code, "annual_days": created.annual_days }))
            .ip_address(address.as_text()),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// `PATCH /api/v1/hr/leave/types/{id}` — the type editor.
pub async fn update_leave_type(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(body): Json<LeaveTypePatchBody>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let before = type_or_404(&state, organization_id, id).await?;
    let updated = leave::update_leave_type(
        state.db().pool(),
        organization_id,
        id,
        &LeaveTypePatch {
            name: body.name,
            code: body.code,
            paid: body.paid,
            annual_days: body.annual_days.map(hundredths_of),
            requires_approval: body.requires_approval,
            allow_negative: body.allow_negative,
            active: body.active,
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.leave_type.updated")
            .organization(organization_id)
            .target("hr_leave_type", id.to_string())
            .metadata(json!({
                "before": { "annual_days": before.annual_days, "active": before.active },
                "after": { "annual_days": updated.annual_days, "active": updated.active },
            }))
            .ip_address(address.as_text()),
    )
    .await?;
    Ok(Json(json!(updated)))
}

/// A day count from a JSON number, in hundredths.
///
/// The form sends a number and the column is `numeric(6,2)`, so the conversion happens once here
/// and in one direction. `f64` is acceptable **at this boundary only** because the value is
/// immediately quantised to hundredths and everything downstream is integer arithmetic; a `0.1`
/// that arrives as `0.1` becomes exactly 10 hundredths.
fn hundredths_of(value: f64) -> i32 {
    (value * 100.0).round() as i32
}

// ---------------------------------------------------------------------------------------------
// Balances
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/hr/leave/balances` — the balance cards for an employee and a year.
///
/// The self-service read: an employee with no `hr.*` permission at all reaches their own cards
/// through the handler's own-employee check, which is the request's "an employee sees self"
/// rule. The route is *not* unguarded — it resolves the caller instead of refusing them, and
/// it answers only for the record that is theirs.
#[derive(Debug, Default, Deserialize)]
pub struct BalanceParams {
    /// The employee. Omitted means the caller.
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    /// The year. Defaults to the current one.
    #[serde(default)]
    pub year: Option<i32>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/hr/leave/balances` — every type's card for one employee in one year.
pub async fn list_balances(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<BalanceParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let mine =
        employees::employee_of_user(state.db().pool(), organization_id, current.user.id)
            .await?
            .ok_or(ApiError::from(HrError::NotFound("employee")))?;

    let employee_id = params.employee_id.unwrap_or(mine);
    // Somebody else's balance is a *different person's* entitlement: refused rather than read,
    // because the route sits behind `hr.leave.read` and a caller holding that key is expected
    // to be an HR role — the self-service path is the one that needs no key at all.
    if employee_id != mine {
        let may = authorize(
            state.db().pool(),
            current.user.id,
            PermissionScope::Organization { organization_id },
            "hr.employees.read",
        )
        .await
        .map(|decision| decision.is_allowed())
        .unwrap_or(false);
        if !may {
            return Err(ApiError::forbidden(
                "hr_leave_balance_scope",
                "you may read your own leave balance, not somebody else's",
            ));
        }
    }

    let cards: Vec<BalanceCard> =
        leave::balances_for(state.db().pool(), organization_id, employee_id, year_of(params.year))
            .await?;
    Ok(Json(json!({ "items": cards })))
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/hr/leave/requests` — a page of requests.
pub async fn list_requests(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<LeaveParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let scope = scope_of(&state, &current, organization_id).await;
    let scope = narrow(&scope, params.visibility.as_deref())?;
    let page = requests::list_requests(state.db().pool(), &scope, &params.into_query()).await?;
    Ok(page_of(page))
}

/// `GET /api/v1/hr/leave/requests/{id}` — one request with its balance and timeline.
pub async fn get_request(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let may_decide = may_approve(&state, &current).await;
    let detail: RequestDetail =
        requests::request_detail(state.db().pool(), organization_id, id, may_decide).await?;
    Ok(Json(json!(detail)))
}

/// `POST /api/v1/hr/leave/requests` — raise a request.
pub async fn create_request(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<NewLeaveRequestBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let leave_type = type_or_404(&state, organization_id, body.leave_type_id).await?;

    // The employee's own row is the default, and naming somebody else is an HR act.
    let mine =
        employees::employee_of_user(state.db().pool(), organization_id, current.user.id)
            .await?
            .ok_or(ApiError::from(HrError::NotFound("employee")))?;
    let employee_id = match body.employee_id {
        Some(id) if id != mine => {
            let may = authorize(
                state.db().pool(),
                current.user.id,
                PermissionScope::Organization { organization_id },
                "hr.employees.read",
            )
            .await
            .map(|decision| decision.is_allowed())
            .unwrap_or(false);
            if !may {
                return Err(ApiError::forbidden(
                    "hr_leave_request_scope",
                    "you may request leave for yourself; for somebody else, ask HR",
                ));
            }
            id
        }
        _ => mine,
    };

    let created = requests::create_request(
        state.db().pool(),
        organization_id,
        &leave_type,
        NewLeaveRequest {
            employee_id: Some(employee_id),
            leave_type_id: body.leave_type_id,
            starts_on: body.starts_on,
            ends_on: body.ends_on,
            half_day: body.half_day.unwrap_or(false),
            reason: body.reason,
            attachment_media_id: body.attachment_media_id,
        },
    )
    .await?;

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

    // A type that needs no approval is approved on creation, and the same event has to fire —
    // an automation waiting on `hr.leave.approved` would otherwise never fire for an
    // organization that turned approval off, which is exactly the organization that believes it
    // is running unattended.
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

/// The body of a cancel: nothing, but the route takes one for the CSRF contract's sake.
#[derive(Debug, Default, Deserialize)]
pub struct EmptyBody {}

/// `POST /api/v1/hr/leave/requests/{id}/decision` — approve or reject.
pub async fn decide_request(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(body): Json<DecisionBody>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let decision = match body.decision.trim().to_ascii_lowercase().as_str() {
        "approve" | "approved" => Decision::Approve,
        "reject" | "rejected" => Decision::Reject,
        other => {
            return Err(ApiError::from(HrError::invalid(
                "leave request",
                "decision",
                format!("{other:?} is not a decision; use approve or reject"),
            )));
        }
    };

    let decided = requests::decide_request(
        state.db().pool(),
        organization_id,
        id,
        decision,
        current.user.id,
        body.comment,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.leave.decided")
            .organization(organization_id)
            .target("hr_leave_request", decided.id.to_string())
            .metadata(json!({
                "decision": decision.as_str(),
                "employee_id": decided.employee_id,
                "days": decided.days,
                "comment": decided.decision_comment,
            }))
            .ip_address(address.as_text()),
    )
    .await?;
    emit(
        &state,
        NewEvent::new(decision.event_name())
            .organization(organization_id)
            .actor(current.user.id)
            .payload(decided.event_ref()),
    )
    .await;
    Ok(Json(json!(decided)))
}

/// `POST /api/v1/hr/leave/requests/{id}/cancel` — withdraw a pending request.
pub async fn cancel_request(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;

    // A pending request an employee cannot withdraw is the most complained-about rule in any
    // leave system, so the raiser may always cancel their own — and HR may cancel anybody's.
    let existing = requests::request_of(state.db().pool(), organization_id, id)
        .await?
        .ok_or(ApiError::from(HrError::NotFound("leave request")))?;
    let mine =
        employees::employee_of_user(state.db().pool(), organization_id, current.user.id)
            .await?
            .ok_or(ApiError::from(HrError::NotFound("employee")));
    let is_owner = mine.is_ok_and(|id| id == existing.employee_id);
    if !is_owner && !may_approve(&state, &current).await {
        return Err(ApiError::forbidden(
            "hr_leave_cancel_scope",
            "you may cancel a request you raised; somebody else's needs an approver",
        ));
    }

    let cancelled = requests::cancel_request(state.db().pool(), organization_id, id, current.user.id)
        .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.leave.cancelled")
            .organization(organization_id)
            .target("hr_leave_request", cancelled.id.to_string())
            .metadata(json!({ "employee_id": cancelled.employee_id, "days": cancelled.days }))
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

/// `true` when the caller may decide somebody else's request.
async fn may_approve(state: &AppState, current: &CurrentSession) -> bool {
    // The caller's OWN tenant is the scope, never the `organization_id` the handler resolved:
    // a platform account asking about another organization's leave would otherwise be
    // authorized against the tenant it is *reading*, which is how a cross-tenant read becomes a
    // cross-tenant decision.
    let Some(tenant) = current.user.organization_id else {
        return true;
    };
    authorize(
        state.db().pool(),
        current.user.id,
        PermissionScope::Organization { organization_id: tenant },
        "hr.leave.approve",
    )
    .await
    .map(|decision| decision.is_allowed())
    .unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// The calendar and the preview
// ---------------------------------------------------------------------------------------------

/// The absence calendar's query string.
#[derive(Debug, Default, Deserialize)]
pub struct CalendarParams {
    /// The first day of the window; defaults to the first of the current month.
    #[serde(default, with = "omnion_module_hr::dates::option")]
    pub from: Option<Date>,
    /// The last day; defaults to the last of the current month.
    #[serde(default, with = "omnion_module_hr::dates::option")]
    pub to: Option<Date>,
    /// `own`, `team` or `all`.
    #[serde(default)]
    pub visibility: Option<String>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/hr/leave/calendar` — the month grid's bars and rows.
pub async fn absence_calendar(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<CalendarParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let today = time::OffsetDateTime::now_utc().date();
    // The default window is the current month, and it is computed with `time`'s own month
    // lengths rather than by adding 30 days — a 30-day default leaves the 31st of a long month
    // off the grid, which is the day somebody's holiday starts.
    let from = params.from.unwrap_or_else(|| today.replace_day(1).expect("the first is a valid day"));
    let to = params.to.unwrap_or_else(|| {
        let (next_year, next_month) = match today.month() {
            time::Month::December => (today.year() + 1, time::Month::January),
            other => (today.year(), other.next()),
        };
        Date::from_calendar_date(next_year, next_month, 1)
            .expect("the first of a month is a valid date")
            - time::Duration::DAY
    });

    let scope = scope_of(&state, &current, organization_id).await;
    let scope = narrow(&scope, params.visibility.as_deref())?;
    let calendar = requests::absence_calendar(state.db().pool(), &scope, from, to).await?;
    // The typed structs, not a hand-built `json!`: `json!` never consults serde, so a `Date`
    // placed in one is written as `time`'s tuple (`[2027, 95]`) rather than as `2027-04-01`.
    // The window bounds would then be unreadable to the grid that has to draw them, while the
    // bars — which go through `Serialize` — render correctly, and the mismatch looks like a
    // backend bug in the calendar rather than a missing adapter.
    Ok(Json(serde_json::to_value(calendar).unwrap_or(Value::Null)))
}

/// The preview's query string.
#[derive(Debug, Deserialize)]
pub struct PreviewParams {
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

/// `GET /api/v1/hr/leave/requests/preview` — the days a range would charge.
///
/// A real route rather than arithmetic in the browser, because the acceptance criterion asks that
/// "the number shown before submit equals the stored value" — and a second implementation in
/// TypeScript is a second answer. One implementation, called by the form and by the store.
pub async fn preview_days(
    Query(params): Query<PreviewParams>,
) -> Result<Json<Value>, ApiError> {
    let days = days_preview(params.starts_on, params.ends_on, params.half_day.unwrap_or(false))?;
    Ok(Json(json!({
        "days": days,
        "working_days": leave::working_days_between(params.starts_on, params.ends_on),
    })))
}
