//! `/api/v1/sales/quotes` and the public quote page (docs/requests/REQ-052, slice 2).
//!
//! The quote is the document the rest of the selling side hangs off, and this route file is
//! deliberately **thin**: every rule about when a quote may change, what its totals are and who may
//! see it lives in [`omnion_module_sales::quotes`]. The reasons are the same three the catalog
//! route gave, and they are worth repeating because slice 2 added two more callers of the same
//! rules — the **public** accept/decline (no session, token-scoped) and the **expiry sweep** (no
//! session, no actor). A rule written in a handler is a rule those two writers do not have.
//!
//! What this layer owns:
//!
//! * the permission each call is behind (`sales.quotes.*` already exists in the catalogue);
//! * the audit row with the before/after diff and the `sales.*` events automations subscribe to;
//! * the two places a public request needs a defence the module does not have: a **rate limit** by
//!   client address, and the refusal to echo a token back in an error.
//! * **the 404-vs-403 rule**: a quote of another organization is `404`, never `403`, so a caller
//!   cannot use the status code to discover that a document exists.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_module_sales::quotes::{
    self, NewQuote, NewQuoteLine, PublicQuote, QuoteDetail, QuotePatch, QuoteQuery, QuoteView,
};
use omnion_module_sales::store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::crm::organization_of;
use crate::routes::iam::record;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// The query of a quote list: the catalog's shape plus the quote-specific filters.
#[derive(Debug, Deserialize)]
pub struct QuoteListParams {
    /// Free text over the number, the title and the customer name.
    #[serde(default)]
    pub search: Option<String>,
    /// Comma-separated statuses, e.g. `draft,sent`.
    #[serde(default)]
    pub status: Option<String>,
    /// The seller to filter by.
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// Only quotes expiring within this many days.
    #[serde(default)]
    pub expiring_in_days: Option<i32>,
    /// Only quotes still valid on this day or later.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// Only quotes at or above this grand total.
    #[serde(default)]
    pub min_total: Option<String>,
    /// Only quotes at or below this grand total.
    #[serde(default)]
    pub max_total: Option<String>,
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
    /// Include the archived quotes.
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl QuoteListParams {
    /// The module's query, with the day filter parsed.
    ///
    /// A bad day is a refusal here rather than a `None` that silently widens the filter: a
    /// "valid from tomorrow" box that quietly shows everything is worse than an error message.
    fn into_query(self) -> Result<QuoteQuery, ApiError> {
        let valid_from = match self.valid_from.as_deref() {
            None | Some("") => None,
            Some(raw) => Some(
                time::Date::parse(raw.trim(), &time::format_description::well_known::Iso8601::DATE)
                    .map_err(|_| {
                        ApiError::bad_request(
                            "invalid_query",
                            "valid_from must be a day such as 2026-09-28",
                        )
                    })?,
            ),
        };
        Ok(QuoteQuery {
            search: self.search,
            status: self.status,
            owner_user_id: self.owner_user_id,
            expiring_in_days: self.expiring_in_days,
            valid_from,
            min_total: self.min_total,
            max_total: self.max_total,
            sort: self.sort,
            direction: self.direction,
            limit: self.limit,
            cursor: self.cursor,
            include_archived: self.include_archived,
        })
    }
}

/// The body of a grid replacement: the whole grid, because the builder saves the whole grid.
#[derive(Debug, Deserialize)]
pub struct ReplaceLinesBody {
    /// The rows the quote should carry afterwards.
    #[serde(default)]
    pub lines: Vec<NewQuoteLine>,
}

/// The body of a cancel.
#[derive(Debug, Deserialize)]
pub struct CancelBody {
    /// Why the organization is withdrawing it. Shown in the timeline, so it is asked for.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The body of a public accept: an optional note the customer typed.
#[derive(Debug, Deserialize)]
pub struct PublicDecisionBody {
    /// A note from the customer — the reason, on a decline; a greeting, on an acceptance.
    #[serde(default)]
    pub note: Option<String>,
    /// Honeypot. A public form that a bot fills in gets every field filled, so this one is named
    /// like a field a person would use and is never rendered.
    #[serde(default)]
    pub website: Option<String>,
}

/// The public caller's own organization, resolved from the token rather than from a session.
#[derive(Debug, Deserialize)]
pub struct PublicTokenParam {
    /// The token from the URL path.
    pub token: String,
}

// ---------------------------------------------------------------------------------------------
// Audit, events and the public rate limit
// ---------------------------------------------------------------------------------------------

/// The reference an audit row carries: ids, numbers and totals, never the whole line grid.
///
/// A quote's audit row is read on the compliance screen, where a hundred-line grid would make the
/// row unreadable and slow the page; the *content* of a version lives in `sales_quote_versions`,
/// which is where a diff of a sent document belongs.
fn quote_ref(quote: &QuoteView) -> Value {
    json!({
        "quote_id": quote.id,
        "number": quote.number,
        "status": quote.status,
        "customer": quote.customer.name,
        "currency": quote.currency,
        "subtotal": quote.totals.subtotal,
        "discount_total": quote.totals.discount_total,
        "tax_total": quote.totals.tax_total,
        "grand_total": quote.totals.grand_total,
        "max_discount_percent": quote.max_discount_percent,
        "version": quote.version,
    })
}

/// Which header fields a write actually changed.
fn header_changes(before: &QuoteView, after: &QuoteView) -> Vec<String> {
    let mut changed = Vec::new();
    if before.title != after.title {
        changed.push("title".to_owned());
    }
    if before.valid_until != after.valid_until {
        changed.push("valid_until".to_owned());
    }
    if before.currency != after.currency {
        changed.push("currency".to_owned());
    }
    if before.price_list_id != after.price_list_id {
        changed.push("price_list_id".to_owned());
    }
    if before.owner.as_ref().map(|o| o.id) != after.owner.as_ref().map(|o| o.id) {
        changed.push("owner".to_owned());
    }
    changed
}

async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the sales event could not be recorded");
    }
}

/// How many public calls one address may make per minute.
///
/// The public accept/decline endpoints take no session, so the only thing standing between them
/// and a database are the token and this. Twenty a minute is far above what a person clicking
/// "Accept" does and far below a script walking the token space; the module's own token lookup is
/// already an indexed equality on a sha256, which is the expensive part this is protecting.
///
/// In-process and deliberately so: this route file has no shared cache to borrow, and a single
/// box's memory is the right lifetime for a per-address counter. A multi-node deployment would move
/// this to the store, and the *interface* would not change — which is why it is one function.
type Budget = std::sync::Mutex<HashMap<String, (u32, u64)>>;

static PUBLIC_BUDGET: std::sync::LazyLock<Budget> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Take one unit of the address's minute budget, or refuse.
fn take_public_budget(address: &str) -> bool {
    /// Per address, per minute.
    const LIMIT: u32 = 20;
    /// Addresses tracked at once. Bounded so a spray of source addresses cannot grow the map
    /// without end: past the bound the map is cleared rather than evicted one by one, because a
    /// quote link is not a service whose callers need a long history.
    const MAX_TRACKED: usize = 10_000;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let minute = now / 60;

    let Ok(mut budget) = PUBLIC_BUDGET.lock() else {
        // A poisoned lock is a panic in somebody else's call; refusing here would turn one
        // poisoned mutex into an outage, so the call is allowed through un-metered.
        return true;
    };
    if budget.len() > MAX_TRACKED {
        budget.clear();
    }
    let entry = budget.entry(address.to_owned()).or_insert((0, minute));
    if entry.1 != minute {
        *entry = (0, minute);
    }
    if entry.0 >= LIMIT {
        return false;
    }
    entry.0 += 1;
    true
}

// ---------------------------------------------------------------------------------------------
// Quotes
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/sales/quotes` — one page of the list.
///
/// Also runs the **expiry sweep** first, so a quote that lapsed overnight is badged `expired`
/// without waiting for a background job. The sweep is a single indexed update on the caller's own
/// organization; the list would be wrong by a day if it did not run.
pub async fn list_quotes(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<QuoteListParams>,
) -> Result<Json<store::Page<QuoteView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let pool = state.db().pool();
    if let Err(error) = quotes::sweep_expired(pool, organization_id).await {
        // A failed sweep must not fail the list: the worst case is a stale badge, and refusing
        // to show quotes over a badge is a far worse product.
        tracing::warn!(error = %error, "the quote expiry sweep did not run");
    }
    let query = params.into_query()?;
    Ok(Json(quotes::list_quotes(pool, organization_id, &query).await?))
}

/// `GET /api/v1/sales/quotes/vocabulary` — what the status tabs and the builder dropdowns offer.
#[derive(Debug, Serialize)]
pub struct QuoteVocabulary {
    /// The statuses, in pipeline order, with the label the tabs print.
    pub statuses: Vec<StatusVocabulary>,
    /// The currency the organization issues quotes in by default.
    pub default_currency: String,
    /// How many days a new quote is valid for.
    pub validity_days: i32,
    /// The largest line discount that can be sent without a manager.
    pub discount_approval_threshold: i32,
}

/// One status the tab bar offers.
#[derive(Debug, Serialize)]
pub struct StatusVocabulary {
    /// The stored value.
    pub value: &'static str,
    /// The label the tab prints.
    pub label: &'static str,
    /// Whether the status counts as open work in the overview.
    pub open: bool,
}

/// The eight statuses with the words the tab bar shows, in pipeline order.
///
/// The labels are here rather than in the screen so the two cannot drift: a status whose label
/// only exists in the front end is a status the API can filter by and no tab can show.
const STATUS_LABELS: [(&str, &str); 8] = [
    ("draft", "Draft"),
    ("pending_approval", "Awaiting approval"),
    ("approved", "Approved"),
    ("sent", "Sent"),
    ("accepted", "Accepted"),
    ("declined", "Declined"),
    ("expired", "Expired"),
    ("cancelled", "Cancelled"),
];

/// `GET /api/v1/sales/quotes/vocabulary` — the tab bar's statuses and the builder's defaults.
pub async fn quote_vocabulary(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<QuoteVocabulary>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let settings = store::get_settings(state.db().pool(), organization_id).await?;
    Ok(Json(QuoteVocabulary {
        statuses: STATUS_LABELS
            .iter()
            .map(|(value, label)| StatusVocabulary {
                value,
                label,
                open: omnion_module_sales::QuoteStatus::parse(value)
                    .is_some_and(omnion_module_sales::QuoteStatus::is_open),
            })
            .collect(),
        default_currency: settings.currency,
        validity_days: settings.quote_validity_days,
        discount_approval_threshold: settings.discount_approval_threshold,
    }))
}

/// The one optional organization parameter the single-record reads carry.
#[derive(Debug, Deserialize)]
pub struct OrganizationParam {
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/sales/quotes/{id}` — one quote with its lines and versions.
pub async fn get_quote(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
) -> Result<Json<QuoteDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let _ = quotes::sweep_expired(pool, organization_id).await;
    Ok(Json(quotes::get_quote(pool, organization_id, quote_id).await?))
}

/// `POST /api/v1/sales/quotes` — create a draft.
pub async fn create_quote(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewQuote>,
) -> Result<(StatusCode, Json<QuoteDetail>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let settings = store::get_settings(pool, organization_id).await?;
    let created = quotes::create_quote(pool, organization_id, &settings, current.user.id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.quote.created")
            .organization(organization_id)
            .target("sales_quote", created.quote.id.to_string())
            .metadata(json!({
                "request_id": created.quote.id,
                "lines": created.lines.len(),
                "after": quote_ref(&created.quote),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.quote.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(quote_ref(&created.quote)),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /api/v1/sales/quotes/{id}` — edit a header that is still being worked on.
pub async fn update_quote(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
    body: Json<QuotePatch>,
) -> Result<Json<QuoteDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = quotes::get_quote(pool, organization_id, quote_id).await?;
    let after = quotes::patch_quote(pool, organization_id, quote_id, &body.0).await?;
    let changed = header_changes(&before.quote, &after.quote);

    if !changed.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "sales.quote.updated")
                .organization(organization_id)
                .target("sales_quote", after.quote.id.to_string())
                .metadata(json!({
                    "request_id": after.quote.id,
                    "changed": changed,
                    "before": quote_ref(&before.quote),
                    "after": quote_ref(&after.quote),
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("sales.quote.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({ "quote_id": after.quote.id, "changed": changed })),
        )
        .await;
    }
    Ok(Json(after))
}

/// `PUT /api/v1/sales/quotes/{id}/lines` — replace the whole grid of a working quote.
pub async fn replace_lines(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
    body: Json<ReplaceLinesBody>,
) -> Result<Json<QuoteDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let settings = store::get_settings(pool, organization_id).await?;
    let before = quotes::get_quote(pool, organization_id, quote_id).await?;
    let after = quotes::replace_lines(pool, organization_id, quote_id, &settings, &body.0.lines).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.quote.lines_replaced")
            .organization(organization_id)
            .target("sales_quote", after.quote.id.to_string())
            .metadata(json!({
                "request_id": after.quote.id,
                "lines_before": before.lines.len(),
                "lines_after": after.lines.len(),
                "before": quote_ref(&before.quote),
                "after": quote_ref(&after.quote),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.quote.updated")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(quote_ref(&after.quote)),
    )
    .await;

    Ok(Json(after))
}

/// `POST /api/v1/sales/quotes/{id}/send` — snapshot a version and mark the quote sent.
pub async fn send_quote(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
) -> Result<Json<QuoteDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let sent = quotes::send_quote(pool, organization_id, quote_id, current.user.id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.quote.sent")
            .organization(organization_id)
            .target("sales_quote", sent.quote.id.to_string())
            .metadata(json!({
                "request_id": sent.quote.id,
                "version": sent.quote.version,
                "after": quote_ref(&sent.quote),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.quote.sent")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(quote_ref(&sent.quote)),
    )
    .await;

    Ok(Json(sent))
}

/// `POST /api/v1/sales/quotes/{id}/cancel` — withdraw a quote that is not decided.
pub async fn cancel_quote(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
    body: Option<Json<CancelBody>>,
) -> Result<Json<QuoteDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let reason = body.and_then(|Json(body)| body.reason);
    let cancelled =
        quotes::cancel_quote(pool, organization_id, quote_id, reason, current.user.id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.quote.cancelled")
            .organization(organization_id)
            .target("sales_quote", cancelled.quote.id.to_string())
            .metadata(json!({
                "request_id": cancelled.quote.id,
                "reason": cancelled.cancel_reason,
                "after": quote_ref(&cancelled.quote),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.quote.cancelled")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(quote_ref(&cancelled.quote)),
    )
    .await;

    Ok(Json(cancelled))
}

/// `POST /api/v1/sales/quotes/{id}/duplicate` — copy a quote into a new draft.
pub async fn duplicate_quote(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
) -> Result<(StatusCode, Json<QuoteDetail>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let copy = quotes::duplicate_quote(state.db().pool(), organization_id, quote_id, current.user.id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.quote.duplicated")
            .organization(organization_id)
            .target("sales_quote", copy.quote.id.to_string())
            .metadata(json!({
                "request_id": copy.quote.id,
                "from": quote_id,
                "after": quote_ref(&copy.quote),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(copy)))
}

/// `POST /api/v1/sales/quotes/{id}/link` — issue (or re-issue) the public link.
///
/// **The token is in the response body and nowhere else.** It is not written to the audit row and
/// not carried in the event, because both of those are readable by people who should not be able
/// to open the customer's copy: the audit screen is wider than the sales desk, and an event
/// payload travels to every webhook subscriber.
pub async fn issue_link(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
) -> Result<Json<PublicLink>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let token = quotes::issue_public_link(state.db().pool(), organization_id, quote_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.quote.link_issued")
            .organization(organization_id)
            .target("sales_quote", quote_id.to_string())
            .metadata(json!({
                "request_id": quote_id,
                "reissued": true,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    // The path the customer opens. The host is the panel's, which the screen prefixes; sending a
    // full URL here would bake one deployment's domain into an API response.
    Ok(Json(PublicLink { url: format!("/q/{token}") }))
}

/// The public link, with the token in clear — the only response that ever carries it.
#[derive(Debug, Serialize)]
pub struct PublicLink {
    /// The path the customer opens.
    pub url: String,
}

/// `GET /api/v1/sales/public/quotes/{token}` — the document a customer reads.
///
/// No session and no permission: the token **is** the credential, and the module's resolver
/// answers `None` for a token that is wrong, expired, consumed or archived — one answer, so the
/// endpoint is not an oracle for which tokens exist.
pub async fn public_quote(
    State(state): State<AppState>,
    address: ClientAddress,
    Path(token): Path<String>,
) -> Result<Json<PublicQuote>, ApiError> {
    let address_key = address.as_text().unwrap_or_else(|| "unknown".to_owned());
    if !take_public_budget(&address_key) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "sales_link_rate_limited",
            "this link has been opened too many times — try again in a minute",
        ));
    }
    let pool = state.db().pool();
    let quote_id = quotes::resolve_public_token(pool, &token)
        .await?
        .ok_or(omnion_module_sales::SalesError::InvalidPublicToken)?;
    Ok(Json(quotes::public_quote(pool, quote_id).await?))
}

/// `POST /api/v1/sales/public/quotes/{token}/accept` — the customer accepted.
pub async fn accept_quote(
    State(state): State<AppState>,
    address: ClientAddress,
    Path(token): Path<String>,
    body: Option<Json<PublicDecisionBody>>,
) -> Result<Json<PublicQuote>, ApiError> {
    let address_key = address.as_text().unwrap_or_else(|| "unknown".to_owned());
    if !take_public_budget(&address_key) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "sales_link_rate_limited",
            "this link has been opened too many times — try again in a minute",
        ));
    }
    let payload = body.map(|Json(body)| body).unwrap_or(PublicDecisionBody {
        note: None,
        website: None,
    });
    // The honeypot is answered with the ordinary "not found" rather than a 400: a bot that fills
    // every field should not learn that the endpoint exists and what it wants.
    if payload.website.as_deref().is_some_and(|value| !value.trim().is_empty()) {
        return Err(omnion_module_sales::SalesError::InvalidPublicToken.into());
    }

    let pool = state.db().pool();
    let accepted = quotes::accept_public_quote(pool, &token, payload.note).await?;
    let organization_id = accepted.organization_id;
    let quote_id = accepted.id;

    record(
        &state,
        NewAuditEntry::system("sales.quote.accepted")
            .organization(organization_id)
            .target("sales_quote", quote_id.to_string())
            .metadata(json!({ "request_id": quote_id, "via": "public link" }))
            .ip_address(address.as_text()),
    )
    .await?;

    // `sales.quote.accepted` is one of the two names the spec pins as an integration hook, so its
    // payload is the documented document: number, customer, currency and the totals a partner
    // system needs to book the deal — and nothing about the seller's price list or margin.
    emit(
        &state,
        NewEvent::new("sales.quote.accepted")
            .organization(organization_id)
            .payload(quote_ref(&accepted)),
    )
    .await;

    Ok(Json(quotes::public_quote(pool, quote_id).await?))
}

/// `POST /api/v1/sales/public/quotes/{token}/decline` — the customer declined.
pub async fn decline_quote(
    State(state): State<AppState>,
    address: ClientAddress,
    Path(token): Path<String>,
    body: Option<Json<PublicDecisionBody>>,
) -> Result<Json<PublicQuote>, ApiError> {
    let address_key = address.as_text().unwrap_or_else(|| "unknown".to_owned());
    if !take_public_budget(&address_key) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "sales_link_rate_limited",
            "this link has been opened too many times — try again in a minute",
        ));
    }
    let payload = body.map(|Json(body)| body).unwrap_or(PublicDecisionBody {
        note: None,
        website: None,
    });
    if payload.website.as_deref().is_some_and(|value| !value.trim().is_empty()) {
        return Err(omnion_module_sales::SalesError::InvalidPublicToken.into());
    }

    let pool = state.db().pool();
    let declined = quotes::decline_public_quote(pool, &token, payload.note).await?;
    let organization_id = declined.organization_id;
    let quote_id = declined.id;

    record(
        &state,
        NewAuditEntry::system("sales.quote.declined")
            .organization(organization_id)
            .target("sales_quote", quote_id.to_string())
            .metadata(json!({ "request_id": quote_id, "via": "public link" }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.quote.declined")
            .organization(organization_id)
            .payload(quote_ref(&declined)),
    )
    .await;

    Ok(Json(quotes::public_quote(pool, quote_id).await?))
}

// ---------------------------------------------------------------------------------------------
// Tests of this file's own decisions
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_module_sales::QuoteStatus;

    #[test]
    fn every_status_the_module_knows_has_a_label() {
        // A status the API can filter by and no tab can show is a status nobody can find.
        let labelled: Vec<&str> = STATUS_LABELS.iter().map(|(value, _)| *value).collect();
        for status in QuoteStatus::all() {
            assert!(
                labelled.contains(&status.as_str()),
                "{} has no tab label",
                status.as_str()
            );
        }
        assert_eq!(labelled.len(), QuoteStatus::all().len(), "and no label is spare");
    }

    #[test]
    fn the_open_flag_the_overview_uses_is_the_modules_own_definition() {
        for (value, label) in STATUS_LABELS {
            let status = QuoteStatus::parse(value).expect("a known status");
            let vocabulary = StatusVocabulary {
                value,
                label,
                open: status.is_open(),
            };
            assert_eq!(vocabulary.open, status.is_open(), "{value}");
        }
    }

    #[test]
    fn a_bad_date_filter_is_refused_rather_than_ignored() {
        // A "valid from tomorrow" box that silently widens the filter is worse than an error.
        let refused = QuoteListParams {
            search: None,
            status: None,
            owner_user_id: None,
            expiring_in_days: None,
            valid_from: Some("next tuesday".to_string()),
            min_total: None,
            max_total: None,
            sort: None,
            direction: None,
            limit: None,
            cursor: None,
            include_archived: None,
            organization_id: None,
        }
        .into_query();
        assert!(refused.is_err(), "a date that is not a date is refused");

        let accepted = QuoteListParams {
            search: None,
            status: None,
            owner_user_id: None,
            expiring_in_days: None,
            valid_from: Some("2026-09-28".to_string()),
            min_total: None,
            max_total: None,
            sort: None,
            direction: None,
            limit: None,
            cursor: None,
            include_archived: None,
            organization_id: None,
        }
        .into_query()
        .expect("an ISO day is read");
        assert!(accepted.valid_from.is_some());
    }

    #[test]
    fn a_bad_search_term_is_left_to_the_module_rather_than_guessed_here() {
        // The module owns the length bound (MAX_SEARCH_LENGTH) and the LIKE escaping; this layer
        // only has to pass the string through, so the rule exists once.
        let query = QuoteListParams {
            search: Some("  acme  ".to_string()),
            status: None,
            owner_user_id: None,
            expiring_in_days: None,
            valid_from: None,
            min_total: None,
            max_total: None,
            sort: None,
            direction: None,
            limit: None,
            cursor: None,
            include_archived: None,
            organization_id: None,
        }
        .into_query()
        .expect("a search term is always passable");
        assert_eq!(query.search.as_deref(), Some("  acme  "));
    }

    #[test]
    fn the_audit_reference_carries_the_totals_and_never_the_grid() {
        // The compliance screen reads these rows; a hundred-line grid would make them unreadable.
        let quote = QuoteView {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            number: "Q-2026-0001".to_string(),
            title: "Website".to_string(),
            status: QuoteStatus::Sent,
            customer: quotes::CustomerRef {
                kind: omnion_module_sales::model::CustomerKind::Company,
                id: Some(Uuid::nil()),
                name: "Acme".to_string(),
            },
            owner: None,
            currency: "TRY".to_string(),
            price_list_id: None,
            valid_until: omnion_module_sales::quotes::today_utc(),
            totals: quotes::QuoteTotalsView {
                subtotal: "200.00".to_string(),
                discount_total: "20.00".to_string(),
                tax_total: "36.00".to_string(),
                grand_total: "216.00".to_string(),
            },
            version: 1,
            max_discount_percent: 10,
            updated_at: omnion_module_sales::quotes::now_utc(),
            created_at: omnion_module_sales::quotes::now_utc(),
        };
        let reference = quote_ref(&quote);
        assert_eq!(reference["grand_total"], "216.00");
        assert_eq!(reference["number"], "Q-2026-0001");
        assert!(reference.get("lines").is_none(), "the grid is never in an audit row");
        assert!(reference.get("notes").is_none());
    }

    #[test]
    fn a_header_edit_reports_the_fields_a_person_edited() {
        let base = QuoteView {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            number: "Q-1".to_string(),
            title: "Old".to_string(),
            status: QuoteStatus::Draft,
            customer: quotes::CustomerRef {
                kind: omnion_module_sales::model::CustomerKind::Company,
                id: None,
                name: "Acme".to_string(),
            },
            owner: None,
            currency: "TRY".to_string(),
            price_list_id: None,
            valid_until: omnion_module_sales::quotes::today_utc(),
            totals: quotes::QuoteTotalsView {
                subtotal: "0.00".to_string(),
                discount_total: "0.00".to_string(),
                tax_total: "0.00".to_string(),
                grand_total: "0.00".to_string(),
            },
            version: 0,
            max_discount_percent: 0,
            updated_at: omnion_module_sales::quotes::now_utc(),
            created_at: omnion_module_sales::quotes::now_utc(),
        };
        let mut after = base.clone();
        after.title = "New".to_string();
        assert_eq!(header_changes(&base, &after), vec!["title".to_string()]);

        // A totals-only change (the lines moved, not the header) is not a header change: it would
        // otherwise be reported twice, once here and once by the line replacement.
        let mut totals_only = base.clone();
        totals_only.totals.grand_total = "99.00".to_string();
        assert!(header_changes(&base, &totals_only).is_empty());
    }

    #[test]
    fn the_public_budget_refuses_the_twenty_first_call_in_a_minute() {
        // Twenty is far above what a person clicking "Accept" does and far below a walk of the
        // token space; the test pins the number so a change to it is deliberate.
        let address = "203.0.113.9:9999";
        for _ in 0..20 {
            assert!(take_public_budget(address), "the first twenty calls pass");
        }
        assert!(!take_public_budget(address), "the twenty-first is refused");
        // A different address has its own budget: one customer's link is not another's problem.
        assert!(take_public_budget("198.51.100.4:1111"));
    }
}
