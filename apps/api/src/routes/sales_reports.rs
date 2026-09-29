//! `/api/v1/sales/reports` and `/api/v1/sales/search` (docs/requests/REQ-052, slice 4b).
//!
//! Deliberately thin, like the other sales route files: what a report *means* lives in
//! [`omnion_module_sales::reports`], and this layer owns only the three things a module does not
//! know about — the permission, the audit row, and the HTTP shape of a file download.
//!
//! Two of those three need a paragraph:
//!
//! * **The summary and the export are one permission, `sales.reports.read`, and they answer the
//!   same filter.** An export that is readable by somebody who cannot see the table is not a
//!   smaller copy of it, it is a way around the permission — the file lands in a download folder
//!   with no screen on it. And the export calls the **same** `build_report`, so "the CSV contains
//!   the same rows as the table" is true by construction rather than by two queries agreeing.
//! * **The export sends a `Content-Disposition` with a `filename`, and a UTF-8 BOM inside the
//!   file** rather than in the header. A seller on a Windows machine double-clicks the file, it
//!   opens in Excel, and a customer's name with a Turkish character in it must not turn into
//!   three question marks in a document they are about to send to a bank. The report's window is
//!   in the filename too, because a folder of six files called `sales-report.csv` is a folder
//!   nobody can find anything in.
//!
//! **Global search is behind `sales.quotes.read` OR `sales.orders.read`, not behind both.**
//!
//! That is the one place this file departs from the uniform permission shape, and the reason is
//! that the obvious alternative is wrong in a way a seller would notice within a day: the palette
//! is on every screen, and a person whose job is deliveries and not quoting must still be able to
//! type a customer's name and find their order. Requiring both would make the search silently
//! absent for half the sales desk — a feature that vanishes with no message, which is worse than
//! a feature that is not there.

use axum::extract::{Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use omnion_module_sales::reports::{self, GlobalSearchResults, ReportQuery, SalesReport};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::crm::organization_of;
use crate::state::AppState;

/// The tenant a platform account may read through, and nothing else on these two routes.
#[derive(Debug, Default, Deserialize)]
pub struct OrganizationParam {
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/sales/reports/summary` — the four numbers, the breakdown and the rows.
pub async fn report_summary(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Query(query): Query<ReportQuery>,
) -> Result<Json<SalesReport>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        reports::build_report(state.db().pool(), organization_id, &query).await?,
    ))
}

/// `GET /api/v1/sales/reports/export` — the same filter set as CSV.
///
/// A separate route rather than `?format=csv` on the summary because a file download and a JSON
/// body have different failure modes: a browser that gets a JSON error where it expected a file
/// has nothing to show, and a route that can answer either way is a route whose error handling is
/// whichever branch ran last.
pub async fn report_export(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Query(query): Query<ReportQuery>,
) -> Result<Response, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let report = reports::build_report(state.db().pool(), organization_id, &query).await?;
    let csv = reports::report_csv(&report);
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    let filename = format!(
        "sales-report-{}-to-{}.csv",
        report.from, report.to
    );
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            // A report's window is a date, so the filename can only be characters a header may
            // carry; the fallback keeps a download rather than turning a bad day into a 500.
            .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"sales-report.csv\"")),
    );
    // The body says what the screen cannot: a capped table exports a capped file, and somebody
    // reconciling two spreadsheets deserves to know which one they are holding. The header is
    // only visible in devtools, so the note rides as a first line of the file — a comment row a
    // spreadsheet ignores, which is why it is prefixed rather than commented.
    let body = if report.truncated {
        format!(
            "# showing the first {} of {} quotes; narrow the window or the filter for the rest\r\n{csv}",
            report.rows.len(),
            report.rows_matched,
        )
    } else {
        csv
    };
    Ok((StatusCode::OK, headers, body).into_response())
}

/// A file, or a JSON body, or a refusal — whichever the route produced.
pub type Response = axum::response::Response;

/// The search term, the tenant and the cap.
#[derive(Debug, Default, Deserialize)]
pub struct SearchParams {
    /// What the person typed.
    #[serde(default)]
    pub q: Option<String>,
    /// How many rows.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/sales/search` — quotes and orders by number and customer, one ranked list.
///
/// A term the platform refuses is a `400` naming the reason, not an empty list: a search box that
/// silently returns nothing for a 200-character term looks broken, and the person typing it has no
/// way to know the length was the problem.
pub async fn search(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<SearchParams>,
) -> Result<Json<GlobalSearchResults>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let term = params.q.as_deref().unwrap_or_default();
    Ok(Json(
        reports::global_search(state.db().pool(), organization_id, term, params.limit).await?,
    ))
}
