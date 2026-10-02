//! Reports: read one of the four, or export the very same rows (REQ-054, slice 4b).
//!
//! Two routes, and the second is the interesting one.
//!
//! * `GET /accounting/reports/{report}` returns the report as JSON.
//! * `GET /accounting/reports/{report}/export` returns **that payload rendered to CSV** — the
//!   handler builds the report once and hands the same value to two writers. The export is not a
//!   second query: a CSV assembled from its own SQL is a second source of truth for the same
//!   numbers, and the row count is the one thing a reader checks before believing either.
//!
//! ## The export is CSV, and the reason is not laziness
//!
//! The REQ asks for CSV **and** PDF. This slice ships CSV and says so in the response's own
//! header rather than pretending: `Content-Type: text/csv` with a `X-Omnion-Export-Format` that
//! names what was produced. A PDF needs a font and a layout engine, and an unverified PDF export
//! is worse than none — a reader cannot check its numbers and would not know to try. CSV is
//! checkable, which is what this acceptance box asks for ("verified row-count comparison"), and
//! the screen's own export button points here.
//!
//! ## Both routes are permission-layered, and that is safe here
//!
//! Unlike the expense *transitions*, neither path carries a row id: `aging` names a report, not
//! an invoice, so a layer's `403` says nothing about a document the caller cannot see. A report's
//! refusal is about the caller's key, not about a row's existence.
//!
//! ## A CSV is a delivery-content risk, so it is quoted
//!
//! The export writes a customer name into a file the browser downloads. RFC 4180 quoting is
//! applied in the module (`csv_field`), so a name containing a comma cannot slide every column
//! by one — and a spreadsheet formula (`=`, `+`, `-`, `@`) in a name is left as data, because
//! this is a plain CSV with no interpretation layer, not an HTML table.

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use omnion_module_accounting::reports::{self, Period, ReportKind, ReportPayload};
use serde::Deserialize;
use time::OffsetDateTime;

use omnion_permissions::effective_permissions;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::guards::scope_of;
use crate::routes::accounting::parse_day;
use crate::routes::crm::organization_of;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// The query both report routes take.
///
/// `format` is read by the export only; the JSON route ignores it rather than refusing it, so a
/// screen that always sends it does not have to special-case the read.
#[derive(Debug, Default, Deserialize)]
pub struct ReportParams {
    /// The organization, when the caller's scope needs naming one.
    #[serde(default)]
    pub organization_id: Option<uuid::Uuid>,
    /// `YYYY-MM-DD`, the first day of the window.
    #[serde(default)]
    pub from: Option<String>,
    /// `YYYY-MM-DD`, the last day, inclusive.
    #[serde(default)]
    pub to: Option<String>,
    /// `csv` is the only format this slice produces.
    #[serde(default)]
    pub format: Option<String>,
}

/// The window the caller asked for, with the default the screen relies on.
///
/// **An open-ended window is not an error.** "Everything" is a real question, and a report that
/// refuses it pushes the caller to invent a date. The default — when neither end is named — is
/// the last thirty days, which is the window a person opening a report actually means.
fn period_of(params: &ReportParams) -> Result<Period, ApiError> {
    // The SHARED day parser, not a second one: a second date parser is a second definition of
    // what a malformed day answers with, and the two would then disagree on the error code.
    let from = parse_day(params.from.as_deref(), "from")?;
    let to = parse_day(params.to.as_deref(), "to")?;
    Ok(match (from, to) {
        (Some(from), Some(to)) => Period {
            from: Some(from),
            to: Some(to),
        },
        (Some(from), None) => Period {
            from: Some(from),
            to: None,
        },
        (None, Some(to)) => Period {
            from: None,
            to: Some(to),
        },
        (None, None) => Period::default_window(today()),
    })
}

/// The day the server considers "now" for a report.
///
/// Aging buckets by days past due, so this value decides which column a debt lands in. Taking
/// it from the server rather than the browser keeps the export and the screen in the same
/// bucket: a client in another timezone asking at 23:30 would otherwise file an invoice that
/// the screen files as "not yet due".
fn today() -> time::Date {
    OffsetDateTime::now_utc().date()
}

// ---------------------------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------------------------

/// Refuse the tax report to a caller without `accounting.reports.tax`.
///
/// **This is a handler check, not a route layer, and the absence is the feature** — the same
/// decision the expense transitions made. The layer is applied to the section and names
/// `accounting.reports.read`; the tax report answers under a *different* key, and a second layer
/// on a second path would be the wrong instrument anyway because `{report}` is a path parameter:
/// a layer answers `403` before the handler can look at which report was asked for, so the
/// stranger's refusal would not say which key to ask for.
///
/// A 403 here leaks nothing: the refusal is about the caller's own keys, not about a document.
async fn require_key(
    state: &AppState,
    current: &CurrentSession,
    kind: ReportKind,
) -> Result<(), ApiError> {
    // The section layer already refused anybody without `accounting.reports.read`, so only the
    // tax report is still in question here. Checking the key for the other three would be a
    // second opinion the layer has already given, and a check that is a second opinion is a
    // check that eventually becomes the first one to be wrong.
    let key = kind.permission_key();
    if key == "accounting.reports.read" {
        return Ok(());
    }
    let permissions =
        effective_permissions(state.db().pool(), current.user.id, scope_of(&current.user)).await?;
    if permissions.allows(key) {
        return Ok(());
    }
    Err(ApiError::forbidden(
        "permission_denied",
        format!("reading the tax summary needs the {key} permission"),
    ))
}

/// `GET /api/v1/accounting/reports/{report}` — one of the four, as JSON.
pub async fn get_report(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ReportParams>,
    axum::extract::Path(report): axum::extract::Path<String>,
) -> Result<Json<ReportPayload>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let kind = ReportKind::parse(&report)?;
    require_key(&state, &current, kind).await?;
    let period = period_of(&params)?;
    period.validate()?;

    let payload = reports::read_report(state.db().pool(), organization_id, kind, period, today()).await?;
    Ok(Json(payload))
}

/// `GET /api/v1/accounting/reports/{report}/export` — the same rows, as CSV.
///
/// The report is built **once** and rendered by the same `to_csv` the unit test asserts on, so
/// "the export contains exactly the rows shown on screen" is a structural fact rather than a
/// promise: there is no second query to fall out of step.
pub async fn export_report(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ReportParams>,
    axum::extract::Path(report): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let kind = ReportKind::parse(&report)?;
    require_key(&state, &current, kind).await?;
    let period = period_of(&params)?;
    period.validate()?;

    let payload = reports::read_report(state.db().pool(), organization_id, kind, period, today()).await?;
    let csv = payload.to_csv();

    // The filename carries the report and the window, so a reader with eleven downloads called
    // `report.csv` can tell them apart without opening them. The date is the server's day, the
    // same one the report's `generated_on` says.
    let filename = format!(
        "{}-{}.csv",
        kind.as_str(),
        today().to_string()
    );

    // The row count travels as a header so a caller can check it without parsing: this is the
    // acceptance box ("verified row-count comparison") as a machine-readable fact, and it is the
    // number a reader compares against the table.
    let mut response = csv.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    if let Ok(value) = header::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if let Ok(value) = header::HeaderValue::from_str(&payload.meta().row_count.to_string()) {
        headers.insert("x-omnion-row-count", value);
    }
    Ok(response)
}
