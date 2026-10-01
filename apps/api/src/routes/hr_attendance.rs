//! `/api/v1/hr/attendance/*` — the clock, the month grid, the corrections (REQ-055, slice 2d).
//!
//! Thin in the same way the rest of the HR routes are, with the four decisions the HTTP layer owns
//! because a module cannot know them:
//!
//! * **Whose day is being punched.** The body may name an employee — that is what the service
//!   account in the request's API table needs — but only `hr.attendance.record` may name somebody
//!   else. A caller with that key punching their own day is the same call, so the self-service
//!   route is the same handler with the employee resolved from the session rather than the body.
//! * **What the day is.** `work_date` is optional and defaults to today, derived on the server.
//!   Accepting a client-supplied "today" would let a phone with the wrong clock write yesterday's
//!   worked hours, and the summary would then be right about the wrong day.
//! * **That a correction is audited.** `hr.attendance.corrected` carries the two punches and the
//!   reason and **not** the minutes: `minutes_worked` is derived on read, and an event that
//!   publishes it would publish a number a later correction can silently invalidate.
//! * **That the three refusals are `409`.** They are conflicts with the current state of a day,
//!   not malformed payloads, and the mapping lives in `routes::hr` with the rest of `HrError`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_hr::attendance::{
    self, AttendanceDay, ClockKind, ClockSource,
};
use omnion_module_hr::employees;
use omnion_module_hr::model::Visibility;
use omnion_module_hr::{HrError, Page};
use serde::Deserialize;
use serde_json::{Value, json};
use time::Date;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::crm::{emit, organization_of};
use crate::routes::iam::record;
use crate::state::AppState;

/// How a clock request may name its subject.
#[derive(Debug, Default, Deserialize)]
pub struct ClockParams {
    /// The organization, when the caller may act for more than one.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// The day, `YYYY-MM-DD`. Defaults to today **on the server**.
    #[serde(default)]
    pub work_date: Option<String>,
    /// Whom to punch. Only honoured for a caller holding `hr.attendance.record`.
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    /// The instant to record, RFC 3339. The correction drawer and an import need it; a person
    /// pressing the button does not, and the server's clock is the honest default.
    #[serde(default)]
    pub at: Option<String>,
    /// The moment's offset, when `at` carries none.
    #[serde(default)]
    pub offset_minutes: Option<i32>,
}

/// The read side: which month, whose days.
#[derive(Debug, Default, Deserialize)]
pub struct AttendanceParams {
    /// The organization.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// The month, `YYYY-MM`. Defaults to this month.
    #[serde(default)]
    pub month: Option<String>,
    /// An employee, for the grid and the summary. Defaults to the caller's own.
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    /// A single day, `YYYY-MM-DD`, for the roster.
    #[serde(default)]
    pub work_date: Option<String>,
}

/// A correction, as the drawer submits it.
#[derive(Debug, Default, Deserialize)]
pub struct CorrectionBody {
    /// The organization.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Whose day is being corrected. Required, and deliberately so: the day alone does not say
    /// whose it is — a roster day is an organization's, not a person's — and a correction with
    /// no subject would have to pick the lowest id and edit the wrong person's hours.
    pub employee_id: Uuid,
    /// The day being corrected.
    pub work_date: String,
    /// The corrected check-in, RFC 3339. Omit to keep the stored one.
    #[serde(default)]
    pub check_in: Option<String>,
    /// The corrected check-out. Omit to keep the stored one — which leaves a day open, and the
    /// reason a person opens a correction at all.
    #[serde(default)]
    pub check_out: Option<String>,
    /// Why the day is being changed. Required by the schema, not merely by the form.
    #[serde(default)]
    pub reason: String,
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/hr/attendance` — the caller's own month, or one employee's for a reader.
pub async fn get_attendance(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<AttendanceParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let month = month_of(params.month.as_deref())?;
    let employee_id = match params.employee_id {
        Some(id) => {
            require_reader(&state, &current, organization_id, id).await?;
            id
        }
        None => employee_of_caller(&state, &current, organization_id).await?,
    };
    let days = attendance::month_of(state.db().pool(), organization_id, employee_id, month).await?;
    let summary =
        attendance::summary(state.db().pool(), organization_id, employee_id, month).await?;
    // The grid and the summary ship together, for the reason the leave screen's balances and
    // requests do: a person reconciling their month against its total needs both on screen at
    // once, and two fetches a minute apart is how a screen shows two different numbers.
    Ok(Json(json!({
        "month": omnion_module_hr::dates::to_wire(&summary.month),
        "employee_id": employee_id,
        "days": days,
        "summary": summary,
    })))
}

/// `GET /api/v1/hr/attendance/summary` — one employee's month, as the reports screen reads it.
pub async fn get_summary(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<AttendanceParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let month = month_of(params.month.as_deref())?;
    let employee_id = match params.employee_id {
        Some(id) => {
            require_reader(&state, &current, organization_id, id).await?;
            id
        }
        None => employee_of_caller(&state, &current, organization_id).await?,
    };
    let summary =
        attendance::summary(state.db().pool(), organization_id, employee_id, month).await?;
    Ok(Json(json!(summary)))
}

/// `GET /api/v1/hr/attendance/roster` — one organization's day, with who is working and who is
/// on leave.
///
/// The absence half is not this module's to decide: `hr_leave` owns approved leave, and the two
/// are joined here rather than re-derived, so the roster cannot say somebody is working on a day
/// their approved leave says they are away.
pub async fn get_roster(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<AttendanceParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let work_date = day_of(params.work_date.as_deref())?;
    let days = attendance::roster(state.db().pool(), organization_id, work_date).await?;
    Ok(Json(json!({
        "work_date": omnion_module_hr::dates::to_wire(&work_date),
        "today": work_date == OffsetDateTime::now_utc().date(),
        "days": days,
    })))
}

// ---------------------------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/hr/attendance/check-in` — open the caller's day, or an employee's with the key.
pub async fn check_in(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(params): Json<ClockParams>,
) -> Result<Json<Value>, ApiError> {
    let (organization_id, employee_id) = subject_of(&state, &current, &params).await?;
    let day = punch(&state, &current, &params, organization_id, employee_id, ClockKind::In).await?;
    Ok(Json(json!(day)))
}

/// `POST /api/v1/hr/attendance/check-out` — close the caller's day, or an employee's with the key.
pub async fn check_out(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(params): Json<ClockParams>,
) -> Result<Json<Value>, ApiError> {
    let (organization_id, employee_id) = subject_of(&state, &current, &params).await?;
    let day =
        punch(&state, &current, &params, organization_id, employee_id, ClockKind::Out).await?;
    Ok(Json(json!(day)))
}

/// `POST /api/v1/hr/attendance/corrections` — change a day, with a reason, audited.
pub async fn correct_day(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<CorrectionBody>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, body.organization_id).await?;
    let work_date = omnion_module_hr::dates::parse(&body.work_date)
        .map_err(|_| ApiError::bad_request("invalid_work_date", "work_date is not a date"))?;
    let check_in = instant_of(body.check_in.as_deref())?;
    let check_out = instant_of(body.check_out.as_deref())?;
    // The day is addressed by employee AND date, and the store re-checks both — an update whose
    // `where` matched no row returns a 404 rather than a silent success, which is the difference
    // between "corrected" and "believed corrected".
    let day = attendance::correct(
        state.db().pool(),
        organization_id,
        body.employee_id,
        work_date,
        check_in,
        check_out,
        &body.reason,
        current.user.id,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.attendance.corrected")
            .organization(organization_id)
            .target("hr_attendance", day.id.to_string())
            .metadata(json!({
                "work_date": omnion_module_hr::dates::to_wire(&work_date),
                "employee_id": body.employee_id,
                "reason": body.reason,
                "check_in": day.check_in.map(|m| omnion_module_hr::dates::instant_to_wire(&m)),
                "check_out": day.check_out.map(|m| omnion_module_hr::dates::instant_to_wire(&m)),
            })),
    )
    .await?;
    emit(
        &state,
        NewEvent::new("hr.attendance.corrected")
            .organization(organization_id)
            .actor(current.user.id)
            // Ids and the day, never the reason: a correction note is the person explaining
            // themselves to their manager, and a subscriber may be a third party's webhook.
            .payload(json!({
                "attendance_id": day.id,
                "employee_id": body.employee_id,
                "work_date": omnion_module_hr::dates::to_wire(&work_date),
                "corrected_by": current.user.id,
            })),
    )
    .await;
    Ok(Json(json!(day)))
}

/// `GET /api/v1/hr/attendance/export` — the month as CSV, row for row with the grid.
///
/// The request's acceptance criterion is that the export **matches the grid**, so this reads the
/// same `month_of` the screen reads rather than a second query: a report that re-queries is a
/// report that can disagree with the table above it, and nobody notices until a payroll run.
pub async fn export_csv(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<AttendanceParams>,
) -> Result<String, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let month = month_of(params.month.as_deref())?;
    let employee_id = match params.employee_id {
        Some(id) => {
            require_reader(&state, &current, organization_id, id).await?;
            id
        }
        None => employee_of_caller(&state, &current, organization_id).await?,
    };
    let days = attendance::month_of(state.db().pool(), organization_id, employee_id, month).await?;
    let summary =
        attendance::summary(state.db().pool(), organization_id, employee_id, month).await?;
    let today = OffsetDateTime::now_utc().date();
    let mut out = String::from("work_date,check_in,check_out,minutes_worked,source,corrected,exception\n");
    for day in &days {
        out.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            omnion_module_hr::dates::to_wire(&day.work_date),
            day.check_in.map_or_else(String::new, |m| omnion_module_hr::dates::instant_to_wire(&m)),
            day.check_out.map_or_else(String::new, |m| omnion_module_hr::dates::instant_to_wire(&m)),
            day.minutes_worked.map_or_else(String::new, |m| m.to_string()),
            day.source,
            day.corrected,
            day.exception(today).map_or_else(String::new, |e| e.as_str().to_string()),
        ));
    }
    // The totals travel with the rows: a CSV whose last line is a bare number is a file a person
    // has to re-derive the sum from, and this is the export a payroll import reads.
    out.push_str(&format!(
        "# total,{},{},{},{},{},{}\n",
        summary.days_present, summary.minutes_worked, summary.overtime_days,
        summary.under_hours_days, summary.missing_checkout_days, summary.open_days
    ));
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The employee the caller is acting for, and the organization it is scoped to.
async fn subject_of(
    state: &AppState,
    current: &CurrentSession,
    params: &ClockParams,
) -> Result<(Uuid, Uuid), ApiError> {
    let organization_id = organization_of(state, current, params.organization_id).await?;
    match params.employee_id {
        // A named employee is only ever honoured with the recording key; without it the
        // parameter is ignored and the caller's own day is punched, which is the same shape as
        // slice 2c's "there is deliberately no employee_id here".
        Some(id) => {
            // Poking somebody else's day is a power, so it is resolved here rather than trusted
            // from the body. Without the key the parameter is IGNORED below and the caller's own
            // day is punched — the same shape as slice 2c's "there is deliberately no
            // `employee_id` here", except that this route is the one place a service account
            // legitimately needs one.
            let allowed = omnion_permissions::authorize(
                state.db().pool(),
                current.user.id,
                omnion_permissions::Scope::Organization { organization_id },
                "hr.attendance.record",
            )
            .await
            .map(|decision| decision.is_allowed())
            .unwrap_or(false);
            if !allowed {
                return Err(ApiError::forbidden(
                    "hr.attendance.record",
                    "punching the clock for somebody else needs the hr.attendance.record permission",
                ));
            }
            Ok((organization_id, id))
        }
        None => {
            let employee_id = employee_of_caller(state, current, organization_id).await?;
            Ok((organization_id, employee_id))
        }
    }
}

/// One punch, whichever kind, with the event and the audit row the request's event list names.
async fn punch(
    state: &AppState,
    current: &CurrentSession,
    params: &ClockParams,
    organization_id: Uuid,
    employee_id: Uuid,
    kind: ClockKind,
) -> Result<AttendanceDay, ApiError> {
    let work_date = day_of(params.work_date.as_deref())?;
    let at = instant_of(params.at.as_deref())?.or_else(|| params.offset_minutes.map(|m| {
        OffsetDateTime::now_utc() + time::Duration::minutes(i64::from(m))
    }));
    // A caller who is punching somebody else's day is a service account; the row says which, and
    // the difference matters to anybody reading the roster a year later.
    let source = if params.employee_id.is_some() {
        ClockSource::Api
    } else {
        ClockSource::Manual
    };
    let day = attendance::punch(
        state.db().pool(),
        organization_id,
        employee_id,
        work_date,
        kind,
        at,
        source,
    )
    .await?;
    record(
        state,
        NewAuditEntry::by_user(current.user.id, "hr.attendance.recorded")
            .organization(organization_id)
            .target("hr_attendance", day.id.to_string())
            .metadata(json!({
                "employee_id": employee_id,
                "work_date": omnion_module_hr::dates::to_wire(&work_date),
                "kind": kind.as_str(),
                "source": day.source,
            })),
    )
    .await?;
    emit(
        state,
        NewEvent::new("hr.attendance.recorded")
            .organization(organization_id)
            .actor(current.user.id)
            // Ids, the day and the kind — and never the minutes: the projection is derived on
            // read, so an event carrying it would publish a number a later correction silently
            // invalidates, and an automation that trusted it would be wrong rather than stale.
            .payload(json!({
                "attendance_id": day.id,
                "employee_id": employee_id,
                "work_date": omnion_module_hr::dates::to_wire(&work_date),
                "kind": kind.as_str(),
                "source": day.source,
            })),
    )
    .await;
    Ok(day)
}

/// The caller's own employee row, or a `404` naming the employee.
async fn employee_of_caller(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
) -> Result<Uuid, ApiError> {
    // `employee_of_user` already resolves to the row's **id**, not to a row: the self-service
    // routes need the id, and wrapping it in a struct to read one field would be a type that
    // exists only to be unwrapped.
    employees::employee_of_user(state.db().pool(), organization_id, current.user.id)
        .await?
        .ok_or(ApiError::from(HrError::NotFound("employee")))
}

/// A reader may look at another employee's month only behind `hr.attendance.read`.
///
/// The visibility level is **not** re-derived here: `routes::hr::scope_of` already reads the
/// department-scoped bindings and picks the narrowest level, and a second implementation of that
/// rule in this file is a second answer to "who may read whom" — the exact drift the leave slice
/// hit when the module and the route each had their own scope. So this asks the one owner and
/// narrows the answer.
async fn require_reader(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
    employee_id: Uuid,
) -> Result<(), ApiError> {
    // `authorize`, not a guard layer: the key is being resolved for a `?employee_id=` that
    // arrives in the body of a GET, so it is a decision this handler makes rather than a rule
    // installed on a router. (Asking a *layer* to do it would also answer 403 for a caller
    // reading their own month, which is the surface slice 2c exists to protect.)
    let allowed = omnion_permissions::authorize(
        state.db().pool(),
        current.user.id,
        omnion_permissions::Scope::Organization { organization_id },
        "hr.attendance.read",
    )
    .await
    .map(|decision| decision.is_allowed())
    .unwrap_or(false);
    if !allowed {
        return Err(ApiError::forbidden(
            "hr.attendance.read",
            "reading another employee's attendance needs the hr.attendance.read permission",
        ));
    }
    let scope = crate::routes::hr::scope_of(state, current, organization_id).await;
    // `own` reads the caller's own day and nobody else's, so naming a colleague narrows to a 404
    // rather than a 403: from outside, "that day does not exist for you" and "you may not read
    // it" have to be the same answer, or the 403 confirms the day exists.
    if scope.visibility == Visibility::Own {
        let own = employee_of_caller(state, current, organization_id)
            .await
            .unwrap_or(Uuid::nil());
        if employee_id != own {
            // Through the module's own error, so the 404 body reads like every other HR 404.
            return Err(ApiError::from(HrError::NotFound("attendance day")));
        }
    }
    Ok(())
}

/// The month a query names, defaulting to this one.
fn month_of(raw: Option<&str>) -> Result<Date, ApiError> {
    match raw {
        None => Ok(attendance::this_month()),
        Some(text) => {
            let mut parts = text.splitn(2, '-');
            let year: i32 = parts
                .next()
                .and_then(|y| y.parse().ok())
                .ok_or_else(|| ApiError::bad_request("invalid_month", "month is not YYYY-MM"))?;
            let month: u8 = parts
                .next()
                .and_then(|m| m.parse().ok())
                .ok_or_else(|| ApiError::bad_request("invalid_month", "month is not YYYY-MM"))?;
            Date::from_calendar_date(year, time::Month::try_from(month).map_err(|_| {
                ApiError::bad_request("invalid_month", "month is not YYYY-MM")
            })?, 1)
            .map_err(|_| ApiError::bad_request("invalid_month", "month is not YYYY-MM"))
        }
    }
}

/// The day a query names, defaulting to today.
fn day_of(raw: Option<&str>) -> Result<Date, ApiError> {
    match raw {
        None => Ok(OffsetDateTime::now_utc().date()),
        Some(text) => omnion_module_hr::dates::parse(text)
            .map_err(|_| ApiError::bad_request("invalid_work_date", "work_date is not a date")),
    }
}

/// An RFC 3339 instant, or `None` when the field was not supplied.
fn instant_of(raw: Option<&str>) -> Result<Option<OffsetDateTime>, ApiError> {
    match raw {
        None => Ok(None),
        Some(text) if text.trim().is_empty() => Ok(None),
        Some(text) => OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .map(Some)
            .map_err(|_| ApiError::bad_request("invalid_instant", "not an RFC 3339 instant")),
    }
}

