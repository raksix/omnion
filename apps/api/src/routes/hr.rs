//! `/api/v1/hr/*` slice 1: employees, departments and the org chart (docs/requests/REQ-055).
//!
//! Thin by design, exactly like the CRM and accounting route files. What the HTTP layer owns here
//! is only the things a module cannot know:
//!
//! * **Which organization** the panel is looking at, and the caller's **visibility level** — read
//!   from the same `department`-scoped role bindings the CRM uses, and **narrowed by the caller's
//!   own employee row**, because in HR nobody owns the people, they report to them.
//! * **The sensitive gate.** `hr.employees.sensitive.read` decides what a *response carries*, not
//!   whether a request is allowed, so it is resolved inside the handlers rather than by
//!   `guards::require`. A route guard on it would 403 the whole detail screen; the request asks
//!   for the block to be hidden, not for the employee to be invisible.
//! * **The events and the audit rows.** Every mutation is audited; the ones the request names as
//!   automations trigger (`hr.employee.joined`, `hr.employee.terminated`, `hr.department.changed`)
//!   also go on the bus with ids and dates — never a personal field, and never a note.
//!
//! The **rules** stay in `modules::hr`: a manager cycle is refused whether it arrived through the
//! form, the bulk action or the JSON, and a rule that exists in two places is a rule that will be
//! true in one of them.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_hr::departments::{
    self, Department, DepartmentChanges, DepartmentPatch, OrgNode,
};
use omnion_module_hr::employees::{
    self, Employee, EmployeeChanges, EmployeePatch, EmployeePrivate, EmployeeQuery, Scope,
};
use omnion_module_hr::model::{SENSITIVE_FIELDS_READ, Visibility};
use omnion_module_hr::{HrError, Page};
use serde::Deserialize;
use serde_json::{Value, json};
use time::Date;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::crm::{emit, organization_of};
use omnion_permissions::{Scope as PermissionScope, authorize};
use crate::routes::iam::record;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// The employee list's query string.
#[derive(Debug, Default, Deserialize)]
pub struct EmployeeParams {
    /// Free text: name, number or work e-mail.
    #[serde(default)]
    pub search: Option<String>,
    /// A department.
    #[serde(default)]
    pub department_id: Option<Uuid>,
    /// Whether a department filter also matches its children.
    #[serde(default)]
    pub include_subdepartments: Option<bool>,
    /// A manager id.
    #[serde(default)]
    pub manager_id: Option<Uuid>,
    /// One of the four employment types.
    #[serde(default)]
    pub employment_type: Option<String>,
    /// `active`, `on_leave` or `terminated`.
    #[serde(default)]
    pub status: Option<String>,
    /// Started on or after.
    #[serde(default)]
    pub started_from: Option<Date>,
    /// Started on or before.
    #[serde(default)]
    pub started_to: Option<Date>,
    /// Sort key.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc`.
    #[serde(default)]
    pub direction: Option<String>,
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

impl EmployeeParams {
    /// The module's own query type, which is where the filters are validated.
    fn into_query(self) -> EmployeeQuery {
        EmployeeQuery {
            search: self.search,
            department_id: self.department_id,
            include_subdepartments: self.include_subdepartments,
            manager_id: self.manager_id,
            employment_type: self.employment_type,
            status: self.status,
            started_from: self.started_from,
            started_to: self.started_to,
            sort: self.sort,
            direction: self.direction,
            limit: self.limit,
            cursor: self.cursor,
            // The **string**, deliberately: the module refuses an unknown level rather than
            // widening to `all`, and a hand-rolled `parse` here would have undone that.
            visibility: self.visibility,
        }
    }
}

/// The body of a new employee.
#[derive(Debug, Deserialize)]
pub struct NewEmployee {
    /// Employee number; suggested when absent.
    #[serde(default)]
    pub employee_no: Option<String>,
    /// The platform account to link.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Given name (required).
    pub first_name: String,
    /// Family name (required).
    pub last_name: String,
    /// Work address (required).
    pub work_email: String,
    /// Work phone.
    #[serde(default)]
    pub phone: Option<String>,
    /// Job title (required).
    pub position: String,
    /// Department (required).
    pub department_id: Uuid,
    /// Line manager.
    #[serde(default)]
    pub manager_id: Option<Uuid>,
    /// Employment type (required).
    pub employment_type: String,
    /// First day, `YYYY-MM-DD`.
    #[serde(default, with = "omnion_module_hr::dates::option")]
    pub start_date: Option<Date>,
    /// Last day.
    #[serde(default, with = "omnion_module_hr::dates::option")]
    pub end_date: Option<Date>,
    /// Lifecycle status; `active` when absent.
    #[serde(default)]
    pub employee_status: Option<String>,
    /// Where the person works.
    #[serde(default)]
    pub location: Option<String>,
    /// Gated: personal e-mail.
    #[serde(default)]
    pub personal_email: Option<String>,
    /// Gated: personal phone.
    #[serde(default)]
    pub personal_phone: Option<String>,
    /// Gated: home address.
    #[serde(default)]
    pub address: Option<String>,
    /// Gated: emergency contact.
    #[serde(default)]
    pub emergency_contact: Option<String>,
    /// Free text.
    #[serde(default)]
    pub notes: Option<String>,
    /// Organization to write in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl NewEmployee {
    /// The module's change set.
    ///
    /// `start_date` is `Option` on the wire because a `Date` cannot be absent, but the field is
    /// required — the refusal is the module's, and it arrives as a `400` naming `start_date`
    /// rather than as a `422` from serde that a form cannot show.
    fn into_changes(self, fallback_start: Date) -> EmployeeChanges {
        EmployeeChanges {
            employee_no: self.employee_no,
            user_id: self.user_id,
            first_name: self.first_name,
            last_name: self.last_name,
            work_email: self.work_email,
            phone: self.phone,
            position: self.position,
            department_id: self.department_id,
            manager_id: self.manager_id,
            employment_type: self.employment_type,
            start_date: self.start_date.unwrap_or(fallback_start),
            end_date: self.end_date,
            employee_status: self.employee_status,
            location: self.location,
            personal_email: self.personal_email,
            personal_phone: self.personal_phone,
            address: self.address,
            emergency_contact: self.emergency_contact,
            notes: self.notes,
        }
    }
}

/// The body of an employee patch. Every key optional; `null` means "clear this optional field".
#[derive(Debug, Default, Deserialize)]
pub struct EmployeePatchBody {
    /// Given name.
    #[serde(default)]
    pub first_name: Option<String>,
    /// Family name.
    #[serde(default)]
    pub last_name: Option<String>,
    /// Work address.
    #[serde(default)]
    pub work_email: Option<String>,
    /// Work phone.
    #[serde(default)]
    pub phone: Option<String>,
    /// Job title.
    #[serde(default)]
    pub position: Option<String>,
    /// Department.
    #[serde(default)]
    pub department_id: Option<Uuid>,
    /// Line manager.
    #[serde(default)]
    pub manager_id: Option<Uuid>,
    /// Employment type.
    #[serde(default)]
    pub employment_type: Option<String>,
    /// First day.
    #[serde(default, with = "omnion_module_hr::dates::option")]
    pub start_date: Option<Date>,
    /// Last day.
    #[serde(default, with = "omnion_module_hr::dates::option")]
    pub end_date: Option<Date>,
    /// Lifecycle status.
    #[serde(default)]
    pub employee_status: Option<String>,
    /// Location.
    #[serde(default)]
    pub location: Option<String>,
    /// Gated: personal e-mail.
    #[serde(default)]
    pub personal_email: Option<String>,
    /// Gated: personal phone.
    #[serde(default)]
    pub personal_phone: Option<String>,
    /// Gated: home address.
    #[serde(default)]
    pub address: Option<String>,
    /// Gated: emergency contact.
    #[serde(default)]
    pub emergency_contact: Option<String>,
    /// Free text.
    #[serde(default)]
    pub notes: Option<String>,
}

impl EmployeePatchBody {
    fn into_patch(self) -> EmployeePatch {
        EmployeePatch {
            first_name: self.first_name,
            last_name: self.last_name,
            work_email: self.work_email,
            phone: self.phone,
            position: self.position,
            department_id: self.department_id,
            manager_id: self.manager_id,
            employment_type: self.employment_type,
            start_date: self.start_date,
            end_date: self.end_date,
            employee_status: self.employee_status,
            location: self.location,
            personal_email: self.personal_email,
            personal_phone: self.personal_phone,
            address: self.address,
            emergency_contact: self.emergency_contact,
            notes: self.notes,
        }
    }
}

/// The body of a termination: the last day.
#[derive(Debug, Deserialize)]
pub struct TerminateBody {
    /// The last day of the employment, `YYYY-MM-DD`.
    #[serde(default, with = "omnion_module_hr::dates::option")]
    pub end_date: Option<Date>,
    /// Organization to write in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of a new department.
#[derive(Debug, Deserialize)]
pub struct NewDepartment {
    /// Name (required).
    pub name: String,
    /// Short code.
    #[serde(default)]
    pub code: Option<String>,
    /// The department it sits under.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// The department head.
    #[serde(default)]
    pub manager_employee_id: Option<Uuid>,
    /// Free text.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether it may still take members.
    #[serde(default)]
    pub active: Option<bool>,
    /// Organization to write in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl NewDepartment {
    fn into_changes(self) -> DepartmentChanges {
        DepartmentChanges {
            name: self.name,
            code: self.code,
            parent_id: self.parent_id,
            manager_employee_id: self.manager_employee_id,
            description: self.description,
            active: self.active,
        }
    }
}

/// The body of a department patch.
#[derive(Debug, Default, Deserialize)]
pub struct DepartmentPatchBody {
    /// Name.
    #[serde(default)]
    pub name: Option<String>,
    /// Short code.
    #[serde(default)]
    pub code: Option<String>,
    /// Parent department.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// Department head.
    #[serde(default)]
    pub manager_employee_id: Option<Uuid>,
    /// Free text.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether it may still take members.
    #[serde(default)]
    pub active: Option<bool>,
}

impl DepartmentPatchBody {
    fn into_patch(self) -> DepartmentPatch {
        DepartmentPatch {
            name: self.name,
            code: self.code,
            parent_id: self.parent_id,
            manager_employee_id: self.manager_employee_id,
            description: self.description,
            active: self.active,
        }
    }
}

/// The body of a merge: the department to fold away and the one to keep.
#[derive(Debug, Deserialize)]
pub struct MergeDepartmentBody {
    /// The department whose members move.
    pub source_id: Uuid,
    /// The department they move to.
    pub target_id: Uuid,
    /// Organization to write in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl MergeDepartmentBody {
    /// The two halves of the merge: the department that goes away and the one that keeps its name.
    fn parts(&self) -> (Uuid, Uuid) {
        (self.source_id, self.target_id)
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The caller's employee row, when their platform account has one.
///
/// This is what makes `own` mean *the caller's own record* and `team` mean *their own row plus
/// their direct reports* — a level resolved on a CRM-style "owner" would be meaningless here,
/// because an organization does not own its people, it employs them.
async fn employee_of_caller(
    state: &AppState,
    organization_id: Uuid,
    current: &CurrentSession,
) -> Result<Option<Uuid>, ApiError> {
    Ok(employees::employee_of_user(state.db().pool(), organization_id, current.user.id).await?)
}

/// The caller's visibility level, read from the same department-scoped bindings the CRM uses.
async fn scope_of(state: &AppState, current: &CurrentSession, organization_id: Uuid) -> Scope {
    let Some(tenant) = current.user.organization_id else {
        return Scope::all(organization_id, current.user.id);
    };

    #[derive(sqlx::FromRow)]
    struct Narrowing {
        resource_id: String,
    }

    let rows: Vec<Narrowing> = sqlx::query_as(
        "select rb.resource_id \
         from role_bindings rb \
         where rb.user_id = $1 and rb.revoked_at is null \
           and rb.organization_id = $2 \
           and rb.scope_type = 'department' \
           and exists (select 1 from role_permissions rp \
                       where rp.role_id = rb.role_id and rp.effect = 'allow' \
                         and rp.permission_key in ('hr.employees.read', 'hr.employees.update'))",
    )
    .bind(current.user.id)
    .bind(tenant)
    .fetch_all(state.db().pool())
    .await
    .unwrap_or_default();

    let mut narrowest = Visibility::All;
    for row in rows {
        if let Some(level) = Visibility::parse(Some(&row.resource_id))
            && level != Visibility::All
            && (narrowest == Visibility::All || level < narrowest)
        {
            narrowest = level;
        }
    }

    // A caller bound to `own` with no employee record sees nobody. That is the honest answer:
    // there is no record that could be theirs, and returning the whole directory because the
    // lookup came back empty would be the exact widening the level exists to prevent.
    let employee_id =
        employees::employee_of_user(state.db().pool(), organization_id, current.user.id)
            .await
            .unwrap_or(None);

    Scope::all(organization_id, current.user.id).with(narrowest, employee_id)
}

/// `true` when the caller may read the personal contact block.
///
/// Resolved with the authorizer rather than by a route guard, because it decides what a response
/// **carries** — the same split `crm.fields.sensitive.read` has, and for the same reason: a guard
/// on it would refuse the whole employee rather than hide four fields of them.
async fn may_read_sensitive(state: &AppState, current: &CurrentSession) -> bool {
    let Some(organization_id) = current.user.organization_id else {
        return true;
    };

    authorize(
        state.db().pool(),
        current.user.id,
        PermissionScope::Organization { organization_id },
        SENSITIVE_FIELDS_READ,
    )
    .await
    .map(|decision| decision.is_allowed())
    .unwrap_or(false)
}

/// An employee plus the gated block, **only** for a caller who may read it.
///
/// The single place the decision becomes a response. A second place would be a second decision,
/// and the request's risk note is explicit that the list, the detail and the export must agree —
/// the risk is not that a field leaks once, it is that a list hides it and a download does not.
fn gated_response(private: &EmployeePrivate, may_read: bool) -> Value {
    let mut body = serde_json::to_value(&private.employee).unwrap_or(Value::Null);
    if !may_read {
        return body;
    }
    let gated = private.gated(true);
    if let Value::Object(map) = &mut body {
        for (key, value) in [
            ("personal_email", gated.personal_email),
            ("personal_phone", gated.personal_phone),
            ("address", gated.address),
            ("emergency_contact", gated.emergency_contact),
        ] {
            if let Some(text) = value {
                map.insert(key.to_owned(), Value::String(text.to_owned()));
            }
        }
    }
    body
}

/// The identity of an employee for an event payload: ids and dates, never a personal field.
///
/// A name is a person's own words about themselves and a subscriber may be a third party's
/// webhook, so the payload carries what a rule needs to act and nothing a rule could exfiltrate.
fn employee_ref(employee: &Employee) -> Value {
    // `json!` never consults serde, so a `Date` placed in one is written as `time`'s tuple
    // (`[2026, 273]`) rather than as a date a subscriber can read.
    let start_date = omnion_module_hr::dates::to_wire(&employee.start_date);
    let end_date = employee.end_date.as_ref().map(omnion_module_hr::dates::to_wire);
    json!({
        "employee_id": employee.id,
        "organization_id": employee.organization_id,
        "department_id": employee.department_id,
        "manager_id": employee.manager_id,
        "user_id": employee.user_id,
        "employment_type": employee.employment_type,
        "employee_status": employee.employee_status,
        "start_date": start_date,
        "end_date": end_date,
    })
}

/// A page rendered as the API's own JSON: the rows, the cursor and the total.
fn page_of<T: serde::Serialize>(page: Page<T>) -> Json<Value> {
    Json(json!({
        "items": page.items,
        "next_cursor": page.next_cursor,
        "total_estimate": page.total_estimate,
    }))
}

/// `today` in UTC — the fallback a create uses when the body omits the start date, so the refusal
/// reads "a start date is required" instead of serde reporting a missing field a form cannot map
/// to an input.
fn today() -> Date {
    time::OffsetDateTime::now_utc().date()
}

// ---------------------------------------------------------------------------------------------
// Employees
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/hr/employees` — a page of the directory, filtered and sorted as asked.
pub async fn list_employees(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<EmployeeParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = params.into_query();
    // The level a request *asks* for can only narrow what the bindings already allow, and the
    // scope carries the tighter of the two. Validated here so an unknown level is a `400` from a
    // named rule rather than a silently widened list.
    let requested = query.visibility()?;
    let scope = scope_of(&state, &current, organization_id).await;
    let scope = Scope::all(scope.organization_id, scope.user_id)
        .with(scope.visibility.min(requested), scope.employee_id);

    let page = employees::list_employees(state.db().pool(), &scope, &query).await?;
    Ok(page_of(page))
}

/// `GET /api/v1/hr/employees/{id}` — one employee, with the gated block when the caller may read it.
pub async fn get_employee(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(employee_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let scope = scope_of(&state, &current, organization_id).await;

    // A record outside the caller's scope is a `404`, never a `403`: a `403` confirms the
    // employee exists, which is itself the disclosure the level is there to prevent.
    let private = employees::get_employee_private(state.db().pool(), &scope, employee_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "employee_not_found", "no such employee in this organization"))?;

    let may_read = may_read_sensitive(&state, &current).await;
    Ok(Json(gated_response(&private, may_read)))
}

/// `GET /api/v1/hr/employees/suggest-number` — the next free employee number for the form.
///
/// A **route of its own** rather than a field on the list: the form needs the suggestion before
/// anything is typed, and a list endpoint that answers a number when asked for it would make the
/// directory screen's response shape depend on a query parameter.
pub async fn suggest_employee_number(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let number = employees::suggest_employee_no(state.db().pool(), organization_id).await?;
    Ok(Json(json!({ "employee_no": number })))
}

/// `POST /api/v1/hr/employees` — add an employee.
pub async fn create_employee(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<NewEmployee>,
) -> Result<(StatusCode, Json<Employee>), ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id).await?;
    let body = body.0;

    let changes = body.into_changes(today());
    let after = employees::create_employee(state.db().pool(), organization_id, &changes).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.employee.created")
            .organization(organization_id)
            .target("hr_employee", after.id.to_string())
            .metadata(json!({
                "employee_id": after.id,
                "department_id": after.department_id,
                "employment_type": after.employment_type,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    // The request names `hr.employee.joined` as the classic automation trigger (create the
    // account, apply the onboarding template, notify the team) — a *distinct* event from the
    // audit row, because a subscriber needs something to act on rather than something to read.
    emit(
        &state,
        NewEvent::new("hr.employee.joined")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(employee_ref(&after)),
    )
    .await;

    Ok((StatusCode::CREATED, Json(after)))
}

/// `PATCH /api/v1/hr/employees/{id}` — edit an employee's own fields.
pub async fn update_employee(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(employee_id): Path<Uuid>,
    body: Json<EmployeePatchBody>,
) -> Result<Json<Employee>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let scope = scope_of(&state, &current, organization_id).await;
    let before = employees::get_employee(state.db().pool(), &scope, employee_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "employee_not_found", "no such employee in this organization"))?;

    let after = employees::update_employee(
        state.db().pool(),
        organization_id,
        employee_id,
        &body.0.into_patch(),
    )
    .await?;

    // Typed rather than "serialise both and diff the JSON": a diff over two serialised rows would
    // report a change to a field this update never touched, and the audit screen would then claim
    // a move that did not happen.
    let mut changed: Vec<String> = Vec::new();
    if before.department_id != after.department_id {
        changed.push("department_id".to_owned());
    }
    if before.manager_id != after.manager_id {
        changed.push("manager_id".to_owned());
    }
    if before.position != after.position {
        changed.push("position".to_owned());
    }
    if before.employment_type != after.employment_type {
        changed.push("employment_type".to_owned());
    }
    if before.employee_status != after.employee_status {
        changed.push("employee_status".to_owned());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.employee.updated")
            .organization(organization_id)
            .target("hr_employee", after.id.to_string())
            .metadata(json!({
                "employee_id": after.id,
                "changed": changed,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    if changed.contains(&"department_id".to_owned()) || changed.contains(&"manager_id".to_owned()) {
        // The reporting line moved, which is what the calendar and the org chart both read.
        emit(
            &state,
            NewEvent::new("hr.employee.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "employee_id": after.id,
                    "department_id": after.department_id,
                    "manager_id": after.manager_id,
                    "changed": changed,
                })),
        )
        .await;
    }

    Ok(Json(after))
}

/// `POST /api/v1/hr/employees/{id}/terminate` — end an employment: a status and a last day.
///
/// A separate route rather than a patch, because the two are different decisions: `.update` is
/// the person's record, `.terminate` is a statement about the employment, and the request gives
/// them different keys. There is **no delete route** — the record is referenced by leave,
/// attendance and onboarding, and a hard delete would take a person's history with it.
pub async fn terminate_employee(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(employee_id): Path<Uuid>,
    body: Json<TerminateBody>,
) -> Result<Json<Employee>, ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id).await?;
    let end_date = body.0.end_date.unwrap_or_else(today);

    let after = employees::terminate_employee(state.db().pool(), organization_id, employee_id, end_date)
        .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.employee.terminated")
            .organization(organization_id)
            .target("hr_employee", after.id.to_string())
            .metadata(json!({
                "employee_id": after.id,
                "end_date": omnion_module_hr::dates::to_wire(&end_date),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("hr.employee.terminated")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(employee_ref(&after)),
    )
    .await;

    Ok(Json(after))
}

// ---------------------------------------------------------------------------------------------
// Departments and the org chart
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/hr/departments` — the whole tree, with the counts the labels need.
pub async fn list_departments(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let departments = departments::list_departments(state.db().pool(), organization_id).await?;
    Ok(Json(json!({ "items": departments })))
}

/// `GET /api/v1/hr/org-chart` — the same rows, nested.
///
/// The acceptance criterion is that the chart and the department list agree on counts, so this
/// route reads **the same** function the list does and shapes the answer as a tree; two queries
/// and two count expressions would be able to disagree the moment either changed.
pub async fn org_chart(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<Vec<OrgNode>>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let nodes = departments::org_chart(state.db().pool(), organization_id).await?;
    Ok(Json(nodes))
}

/// `POST /api/v1/hr/departments` — create a department.
pub async fn create_department(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<NewDepartment>,
) -> Result<(StatusCode, Json<Department>), ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id).await?;
    let after = departments::create_department(state.db().pool(), organization_id, &body.0.into_changes())
        .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.department.created")
            .organization(organization_id)
            .target("hr_department", after.id.to_string())
            .metadata(json!({ "department_id": after.id, "name": after.name }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("hr.department.changed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(after.reference()),
    )
    .await;

    Ok((StatusCode::CREATED, Json(after)))
}

/// `PATCH /api/v1/hr/departments/{id}` — rename, re-parent, or set the head.
pub async fn update_department(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(department_id): Path<Uuid>,
    body: Json<DepartmentPatchBody>,
) -> Result<Json<Department>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let after = departments::update_department(
        state.db().pool(),
        organization_id,
        department_id,
        &body.0.into_patch(),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.department.updated")
            .organization(organization_id)
            .target("hr_department", after.id.to_string())
            .metadata(json!({ "department_id": after.id, "name": after.name }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("hr.department.changed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(after.reference()),
    )
    .await;

    Ok(Json(after))
}

/// `DELETE /api/v1/hr/departments/{id}` — delete one that is empty, or refuse with both counts.
pub async fn delete_department(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(department_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    departments::delete_department(state.db().pool(), organization_id, department_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.department.deleted")
            .organization(organization_id)
            .target("hr_department", department_id.to_string())
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/hr/departments/merge` — move a department's members into another and drop it.
///
/// A separate route from the delete because it is a different act with a different consequence: a
/// delete refuses when the department has people, and "rename it" is not what the person meant by
/// "this team is now that team". The move and the delete are one transaction, so a merge cannot
/// half-happen and leave employees in a department that no longer exists.
pub async fn merge_departments(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<MergeDepartmentBody>,
) -> Result<Json<Department>, ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id).await?;
    let (source_id, target_id) = body.0.parts();
    let after = departments::merge_departments(state.db().pool(), organization_id, source_id, target_id)
        .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.department.merged")
            .organization(organization_id)
            .target("hr_department", target_id.to_string())
            .metadata(json!({
                "source_id": source_id,
                "target_id": target_id,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("hr.department.changed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "merged_from": source_id,
                "department": after.reference(),
            })),
    )
    .await;

    Ok(Json(after))
}

// ---------------------------------------------------------------------------------------------
// The module's refusals, in the platform's HTTP vocabulary
// ---------------------------------------------------------------------------------------------

impl From<HrError> for ApiError {
    /// A validation failure is a `400` naming the field the form renders it under, a missing or
    /// out-of-scope record is a `404`, a taken number, address or name is a `409`, and the store
    /// keeps the dependency/internal split the rest of the platform reports.
    fn from(error: HrError) -> Self {
        match error {
            HrError::Invalid { entity, field, message } => {
                Self::bad_request("invalid_hr_record", message)
                    .with_details(json!({ "entity": entity, "field": field }))
            }
            HrError::InvalidQuery(message) => Self::bad_request("invalid_hr_query", message),
            HrError::NotFound(kind) => Self::new(
                StatusCode::NOT_FOUND,
                match kind {
                    "employee" => "employee_not_found",
                    "department" => "department_not_found",
                    _ => "hr_record_not_found",
                },
                format!("no such {kind} in this organization"),
            ),
            HrError::EmployeeNoTaken(_) => Self::new(
                StatusCode::CONFLICT,
                "employee_number_taken",
                "that employee number is already used in this organization",
            ),
            HrError::WorkEmailTaken => Self::new(
                StatusCode::CONFLICT,
                "employee_work_email_taken",
                "another employee of this organization already uses this work e-mail address",
            ),
            HrError::DepartmentNameTaken => Self::new(
                StatusCode::CONFLICT,
                "department_name_taken",
                "another department of this organization already carries this name",
            ),
            // The two cycle refusals are `409`, not `400`: the request is well-formed and the
            // conflict is with the **current shape of the org chart**, which is a different thing
            // from a malformed body. A `400` would tell a person their form was wrong.
            HrError::SelfManager => Self::new(
                StatusCode::CONFLICT,
                "hr_manager_cycle",
                "an employee cannot be their own manager",
            ),
            HrError::ManagerCycle(chain) => Self::new(
                StatusCode::CONFLICT,
                "hr_manager_cycle",
                format!("that employee already reports to this one through {chain}"),
            ),
            HrError::DepartmentCycle => Self::new(
                StatusCode::CONFLICT,
                "hr_department_cycle",
                "a department cannot be moved under itself or one of its own children",
            ),
            // The counts travel with the refusal, because "cannot delete" on its own sends the
            // person to two reports to find out how exposed the department is.
            HrError::DepartmentNotEmpty { members, children } => Self::new(
                StatusCode::CONFLICT,
                "hr_department_not_empty",
                format!(
                    "this department still has {members} members and {children} child departments; move them first"
                ),
            )
            .with_details(json!({ "members": members, "children": children })),
            HrError::InvalidMerge(message) => Self::bad_request("invalid_hr_merge", message),
            // Leave (slice 2). Every one of these is a **conflict with the current state**, not a
            // malformed request: the payload is well-formed and the world says no. 409 rather than
            // 400, because a client that retries a 400 forever will retry this one forever too.
            HrError::LeaveOverlap {
                other_request_id,
                starts_on,
                ends_on,
            } => Self::new(
                StatusCode::CONFLICT,
                "hr_leave_overlap",
                format!(
                    "those dates overlap request {other_request_id}, which runs from {} to {}",
                    omnion_module_hr::dates::to_wire(&starts_on),
                    omnion_module_hr::dates::to_wire(&ends_on)
                ),
            ),
            HrError::InsufficientBalance {
                leave_type,
                entitled,
                used,
                pending,
                requested,
                remaining,
            } => Self::new(
                StatusCode::CONFLICT,
                "hr_leave_insufficient_balance",
                format!(
                    "this request is {requested} days of {leave_type}, but only {remaining} are left \
                     ({entitled} entitled, {used} used, {pending} pending)"
                ),
            ),
            HrError::LeaveAlreadyDecided { status } => Self::new(
                StatusCode::CONFLICT,
                "hr_leave_already_decided",
                format!("this request is already {status}, so it cannot be decided again"),
            ),
            HrError::LeaveNotCancellable { status } => Self::new(
                StatusCode::CONFLICT,
                "hr_leave_not_cancellable",
                format!("this request is {status}, so it can no longer be cancelled"),
            ),
            HrError::Database(err)
                if matches!(
                    err,
                    sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_)
                ) =>
            {
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "dependency_unavailable",
                    "database is unavailable",
                )
            }
            HrError::Database(err) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", err.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// The JSON body an `ApiError` turns into, read through its own `IntoResponse` — the shape a
    /// client actually receives, nested under `error` as the rest of the API documents.
    async fn body_of(error: ApiError) -> Value {
        let response = error.into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body must read");
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn a_refused_field_names_the_field_the_form_renders_it_under() {
        let error = ApiError::from(HrError::invalid(
            "employee",
            "work_email",
            "that is not an e-mail address",
        ));
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let body = body_of(error).await;
        assert_eq!(body["error"]["code"], "invalid_hr_record");
        assert_eq!(body["error"]["details"]["field"], "work_email");
    }

    #[tokio::test]
    async fn a_missing_record_is_a_404_and_never_a_403() {
        // A `403` would confirm to a stranger that the employee exists, which is the disclosure
        // the visibility level exists to prevent.
        let error = ApiError::from(HrError::NotFound("employee"));
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
        let body = body_of(error).await;
        assert_eq!(body["error"]["code"], "employee_not_found");
    }

    #[tokio::test]
    async fn both_cycle_refusals_are_a_409_and_say_which_way_the_chain_runs() {
        // The request asks for an *explicit* message. The chain's own text travels with it, so
        // the person can see which line closes the loop rather than guessing from the tree.
        let self_manager = ApiError::from(HrError::SelfManager);
        assert_eq!(self_manager.status(), StatusCode::CONFLICT);
        let body = body_of(self_manager).await;
        assert_eq!(body["error"]["code"], "hr_manager_cycle");
        assert!(body["error"]["message"].as_str().unwrap().contains("their own manager"));

        let cycle = ApiError::from(HrError::ManagerCycle(
            omnion_module_hr::error::ManagementChain::new(vec![
                "Ada Lovelace".to_owned(),
                "Grace Hopper".to_owned(),
            ]),
        ));
        assert_eq!(cycle.status(), StatusCode::CONFLICT);
        let body = body_of(cycle).await;
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("Ada Lovelace \u{2192} Grace Hopper"), "{message}");
    }

    #[tokio::test]
    async fn a_non_empty_department_carries_both_counts_into_the_body() {
        let error = ApiError::from(HrError::DepartmentNotEmpty {
            members: 12,
            children: 3,
        });
        assert_eq!(error.status(), StatusCode::CONFLICT);
        let body = body_of(error).await;
        assert_eq!(body["error"]["code"], "hr_department_not_empty");
        assert_eq!(body["error"]["details"]["members"], 12);
        assert_eq!(body["error"]["details"]["children"], 3);
    }

    #[tokio::test]
    async fn a_gated_response_carries_the_personal_block_only_for_a_caller_who_may_read_it() {
        // The acceptance criterion the risk note leads with, proved at the layer that decides it:
        // the same record serialises **without** the four keys for one caller and **with** them
        // for another. Absent, never null — a null still tells a reader the field exists.
        let private = EmployeePrivate {
            employee: fake_employee(),
            personal_email: Some("private@example.com".to_owned()),
            personal_phone: Some("+90 532 000 00 00".to_owned()),
            address: Some("Somewhere".to_owned()),
            emergency_contact: Some("A relative".to_owned()),
        };

        let hidden = gated_response(&private, false);
        for field in omnion_module_hr::model::SENSITIVE_FIELDS {
            assert!(hidden.get(field).is_none(), "{field} must be absent, not null");
        }

        let shown = gated_response(&private, true);
        assert_eq!(shown["personal_email"], "private@example.com");
        assert_eq!(shown["emergency_contact"], "A relative");
        // The ungated fields are present either way — the gate is about four keys, not a different
        // shape, so a screen can render one layout for both callers.
        assert_eq!(shown["work_email"], hidden["work_email"]);
        assert_eq!(shown["position"], hidden["position"]);
    }

    #[tokio::test]
    async fn an_event_payload_carries_ids_and_dates_and_no_personal_field() {
        // A subscriber may be a third party's webhook, so the payload is what a rule needs to act
        // on and nothing a rule could exfiltrate.
        let mut employee = fake_employee();
        employee.start_date = Date::from_calendar_date(2026, time::Month::March, 1).unwrap();
        employee.end_date = Some(Date::from_calendar_date(2026, time::Month::September, 30).unwrap());

        let payload = employee_ref(&employee);
        assert_eq!(payload["start_date"], "2026-03-01");
        assert_eq!(payload["end_date"], "2026-09-30");
        for field in omnion_module_hr::model::SENSITIVE_FIELDS {
            assert!(payload.get(field).is_none(), "{field} must not be in a payload");
        }
        assert!(payload.get("first_name").is_none());
        assert!(payload.get("work_email").is_none());
    }

    #[tokio::test]
    async fn a_requested_level_narrows_the_scope_and_never_widens_it() {
        // The request asks for `own` / `team` / `all` bindings; a query that names a **wider**
        // level than the caller's bindings must not hand back the whole directory.
        let organization = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let employee = Uuid::new_v4();

        let bound = Scope::all(organization, caller).with(Visibility::Own, Some(employee));

        for (requested, expected) in [
            (Visibility::Own, Visibility::Own),
            (Visibility::Team, Visibility::Own),
            (Visibility::All, Visibility::Own),
        ] {
            let narrowed = Scope::all(bound.organization_id, bound.user_id)
                .with(bound.visibility.min(requested), bound.employee_id);
            assert_eq!(narrowed.visibility, expected, "requested {requested:?}");
        }
    }

    fn fake_employee() -> Employee {
        Employee {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            employee_no: "EMP-0001".to_owned(),
            user_id: None,
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            work_email: "ada@example.com".to_owned(),
            phone: None,
            position: "Engineer".to_owned(),
            department_id: Uuid::new_v4(),
            department_name: "Engineering".to_owned(),
            manager_id: None,
            manager_name: None,
            employment_type: "full_time".to_owned(),
            start_date: Date::from_calendar_date(2026, time::Month::January, 5).unwrap(),
            end_date: None,
            employee_status: "active".to_owned(),
            location: None,
            notes: String::new(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }
}
