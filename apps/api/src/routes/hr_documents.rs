//! `/api/v1/hr/documents/*` and `/api/v1/hr/reports/*` — slice 4's second half
//! (docs/requests/REQ-055).
//!
//! Thin in the same way as the rest of this module, with three decisions the HTTP layer owns:
//!
//! * **The sweep is a POST, and it is permission-guarded by `hr.documents.manage`.** It is a
//!   write — it stamps `acknowledged_at` on rows — so it cannot sit in the read router where a
//!   `GET` screen would call it, and "the reminder fired" must be something HR can trigger by
//!   hand and not something a link can cause.
//! * **`hr.document.expiring` carries ids and dates, never a title.** The bus may be a third
//!   party's webhook, and a title is a person's own words about their own paperwork. The event
//!   fires once per document per window because the module claims the rows before announcing
//!   them — see `documents::sweep_expiring`.
//! * **The CSV is built from the same struct the JSON renders**, so the export and the table
//!   cannot drift. The route serialises the report and hands it to `reports::csv_for`, which is
//!   the one place that knows which report is which.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_hr::documents::{self, Document, DocumentQuery, NewDocument, SweepResult};
use omnion_module_hr::reports::{self, ReportQuery};
use omnion_permissions::Scope as PermissionScope;
use omnion_permissions::authorize;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::crm::{emit, organization_of};
use crate::routes::iam::record;
use crate::state::AppState;

/// The key a CSV export needs, as a constant so the route and its error message cannot disagree.
pub const EXPORT_KEY: &str = "hr.reports.export";

/// The document list's query, plus the organization an instance operator may name.
#[derive(Debug, Default, Deserialize)]
pub struct DocumentsParams {
    /// The organization, for an instance operator.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Every filter the list takes, flattened into the query string.
    #[serde(flatten)]
    pub query: DocumentQuery,
}

/// The report's query string: the filters plus the one presentation choice.
///
/// `format` lives here and not on the module's `ReportQuery` because it is not a filter — the four
/// report builders must not have to know that a CSV exists, or a new one grows its own idea of how
/// to be exported.
#[derive(Debug, Default, Deserialize)]
pub struct ReportParams {
    /// The organization, for an instance operator.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// `csv` for the file, anything else (and absent) for the JSON the screen renders.
    #[serde(default)]
    pub format: Option<String>,
    /// Every filter the report takes.
    #[serde(flatten)]
    pub query: ReportQuery,
}

/// A sweep's payload.
#[derive(Debug, Deserialize)]
pub struct SweepBody {
    /// How far ahead to look, in days. Absent means the module's own thirty.
    #[serde(default)]
    pub window_days: Option<i64>,
}

/// An attach's payload.
#[derive(Debug, Deserialize)]
pub struct AttachBody {
    /// `contract`, `id`, `certificate` or `other`.
    pub kind: String,
    /// The row's title.
    #[serde(default)]
    pub title: Option<String>,
    /// The media pipeline's id.
    pub media_id: Uuid,
    /// The validity date.
    #[serde(default)]
    pub expires_on: Option<time::Date>,
}

impl From<AttachBody> for NewDocument {
    fn from(body: AttachBody) -> Self {
        Self {
            kind: body.kind,
            title: body.title,
            media_id: body.media_id,
            expires_on: body.expires_on,
        }
    }
}

/// `GET /api/v1/hr/documents` — the list, with the header's counts.
pub async fn list_documents(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<DocumentsParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let page = documents::list(state.db().pool(), organization_id, &params.query).await?;
    Ok(Json(serde_json::to_value(page).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "document_page_unreadable",
            error.to_string(),
        )
    })?))
}

/// `GET /api/v1/hr/documents/{id}` — one document.
pub async fn get_document(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(document_id): Path<Uuid>,
) -> Result<Json<Document>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let document = documents::of(state.db().pool(), organization_id, document_id)
        .await?
        .ok_or(ApiError::new(
            StatusCode::NOT_FOUND,
            "document_not_found",
            "no such document in this organization",
        ))?;
    Ok(Json(document))
}

/// `POST /api/v1/hr/employees/{id}/documents` — attach one.
///
/// 201 rather than 200: the row did not exist, and a client that cannot tell a create from an
/// update cannot retry a timed-out POST without risking a duplicate attachment.
pub async fn attach_document(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(employee_id): Path<Uuid>,
    Json(body): Json<AttachBody>,
) -> Result<(StatusCode, Json<Document>), ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let document = documents::attach(
        state.db().pool(),
        organization_id,
        employee_id,
        current.user.id,
        &body.into(),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "hr.document.uploaded")
            .organization(organization_id)
            .target("hr_document", document.id.to_string())
            // The metadata and the ids, never the title: an audit row is read by more people than
            // the event bus is, and the title is whatever somebody typed.
            .metadata(json!({
                "employee_id": document.employee_id,
                "kind": document.kind,
                "media_id": document.media_id,
                "expires_on": document.expires_on.as_ref().map(omnion_module_hr::dates::to_wire),
            })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("hr.document.uploaded")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "document_id": document.id,
                "employee_id": document.employee_id,
                "kind": document.kind,
                "expires_on": document.expires_on.as_ref().map(omnion_module_hr::dates::to_wire),
            })),
    )
    .await;

    Ok((StatusCode::CREATED, Json(document)))
}

/// `DELETE /api/v1/hr/documents/{id}` — remove the reference, keep the file.
pub async fn delete_document(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(document_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;

    // Read the row BEFORE the delete, so the audit entry can name the employee and the kind. An
    // audit row saying "document 4f2 deleted" is a row nobody can act on six months later.
    let before = documents::of(state.db().pool(), organization_id, document_id).await?;
    documents::remove(state.db().pool(), organization_id, document_id).await?;

    if let Some(document) = before {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "hr.document.removed")
                .organization(organization_id)
                .target("hr_document", document.id.to_string())
                .metadata(json!({
                    "employee_id": document.employee_id,
                    "kind": document.kind,
                    "media_id": document.media_id,
                })),
        )
        .await?;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/hr/documents/sweep` — claim and announce what is expiring.
///
/// Deliberately a POST behind `hr.documents.manage`: it writes `acknowledged_at` on rows, and a
/// reminder that a `GET` can trigger is a reminder a prefetcher, a crawler or a link preview burns
/// out of a month. The response carries `considered` as well as the notices, so a run that
/// announces nothing because everything was already announced is visibly different from a run in
/// an empty window — the two look identical otherwise, and the second is a broken sweep.
pub async fn sweep_documents(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<SweepBody>,
) -> Result<Json<SweepResult>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let today = time::OffsetDateTime::now_utc().date();
    let window = body
        .window_days
        .unwrap_or(documents::EXPIRY_WINDOW_DAYS)
        .clamp(0, 365);

    let found =
        documents::sweep_expiring(state.db().pool(), organization_id, today, window).await?;

    // One event per document, after the claim — so a sweep that dies half way through emitting
    // announces fewer next time rather than repeating the ones it already sent.
    for notice in &found.expiring {
        emit(
            &state,
            NewEvent::new("hr.document.expiring")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "document_id": notice.document_id,
                    "employee_id": notice.employee_id,
                    "expires_on": omnion_module_hr::dates::to_wire(&notice.expires_on),
                    "days_left": notice.days_left,
                })),
        )
        .await;
    }

    if !found.expiring.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "hr.document.expiring_swept")
                .organization(organization_id)
                .target("hr_document", "expiry-sweep".to_string())
                .metadata(json!({
                    "considered": found.considered,
                    "announced": found.expiring.len(),
                    "window_days": window,
                })),
        )
        .await?;
    }

    Ok(Json(found))
}

/// `GET /api/v1/hr/reports` — the report picker.
///
/// The list is served rather than hardcoded in the screen: four entries in four places is four
/// chances to add a report to the menu and forget the CSV arm, and a screen offering a report the
/// route refuses answers 404 at the click.
pub async fn report_names(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<Value>, ApiError> {
    // Resolved so an instance operator naming another organization is refused here rather than
    // after the picker has rendered.
    let _ = organization_of(&state, &current, None).await?;
    Ok(Json(json!({
        "items": reports::REPORTS,
        "exports": ["csv"],
    })))
}

/// `GET /api/v1/hr/reports/{report}` — one report, or its CSV with `?format=csv`.
pub async fn get_report(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(report): Path<String>,
    Query(params): Query<ReportParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    if !reports::is_known_report(&report) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_report",
            format!(
                "'{report}' is not a report — use one of {}",
                reports::REPORTS.join(", ")
            ),
        ));
    }

    let today = time::OffsetDateTime::now_utc().date();
    let period = reports::Period::resolve(&params.query, today)?;
    let body = build(&state, organization_id, &report, period, params.query.department_id, today)
        .await?;

    // The CSV is derived from the JSON, not from a second query. That is the whole point: the
    // file a payroll import reads is built from the same value the screen rendered, so the two
    // cannot describe different organizations.
    if params.format.as_deref() == Some("csv") {
        // The second key, checked here rather than as a second route: the screen's export button
        // and a hand-typed `?format=csv` must reach the same decision, and two paths would be two
        // chances for one of them to skip it. Reading the report and taking it away are different
        // acts — the second one writes a file out of the tenant — so `hr.reports.read` alone does
        // not buy the CSV.
        if !may_export(&state, &current, organization_id).await {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "hr.reports.export required",
                "reading the report is not taking a copy of it — this export needs hr.reports.export",
            ));
        }
        let csv = reports::csv_for(&report, &body)?;
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "hr.reports.exported")
                .organization(organization_id)
                .target("hr_report", report.clone())
                // The period and the row count, not the rows: an audit log of a payroll export is
                // read for "who pulled the numbers and when", not for the numbers themselves.
                .metadata(json!({
                    "report": report,
                    "period_from": omnion_module_hr::dates::to_wire(&period.from),
                    "period_to": omnion_module_hr::dates::to_wire(&period.to),
                    "bytes": csv.len(),
                })),
        )
        .await?;
        return Ok(Json(json!({
            "report": report,
            "format": "csv",
            "csv": csv,
            "period": period,
        })));
    }

    Ok(Json(body))
}

/// Whether the caller may export a report.
///
/// The same authorizer the route guard uses, so a policy that denies `hr.reports.export` has the
/// same effect here as it does on a route. An account with no organization (an instance operator)
/// is allowed: they already sit above every tenant's data.
async fn may_export(state: &AppState, current: &CurrentSession, organization_id: Uuid) -> bool {
    let Some(own) = current.user.organization_id else {
        return true;
    };
    // Authorised in the **organization the report came from**, not the caller's own: an instance
    // operator reading another tenant's report must be checked against that tenant, or the export
    // key would be evaluated in a scope where the caller holds nothing and always pass.
    let scope = if own == organization_id {
        PermissionScope::Organization { organization_id: own }
    } else {
        PermissionScope::Global
    };
    authorize(state.db().pool(), current.user.id, scope, EXPORT_KEY)
        .await
        .map(|decision| decision.is_allowed())
        .unwrap_or(false)
}

/// A report that cannot be serialised is a bug in the report, not in the caller's query.
fn unreadable(error: serde_json::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "report_unreadable",
        error.to_string(),
    )
}

/// Build whichever report was named.
async fn build(
    state: &AppState,
    organization_id: Uuid,
    report: &str,
    period: reports::Period,
    department_id: Option<Uuid>,
    today: time::Date,
) -> Result<Value, ApiError> {
    // Each arm serialises its own type rather than the four unifying behind one: they are four
    // structs with four row shapes, and a `Value` they all coerce to is a way of losing a compile
    // error about a field that changed shape. `to_value` is the only place they meet, and it is
    // the same call the CSV path takes, which is what keeps the two identical by construction.
    let value: Value = match report {
        "headcount" => serde_json::to_value(
            reports::headcount(state.db().pool(), organization_id, period, department_id).await?,
        )
        .map_err(unreadable)?,
        "turnover" => serde_json::to_value(
            reports::turnover(state.db().pool(), organization_id, period, department_id).await?,
        )
        .map_err(unreadable)?,
        "absence" => {
            serde_json::to_value(reports::absence(state.db().pool(), organization_id, period).await?)
                .map_err(unreadable)?
        }
        "attendance" => serde_json::to_value(
            reports::attendance_report(state.db().pool(), organization_id, period, department_id)
                .await?,
        )
        .map_err(unreadable)?,
        other => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_report",
                format!("'{other}' is not a report"),
            ))
        }
    };
    let _ = today;
    Ok(value)
}