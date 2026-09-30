//! `/api/v1/accounting` — the chart of accounts, the tax rates and the journal
//! (docs/requests/REQ-054, slice 1).
//!
//! Three things this layer owns, and the reasons are the same three the CRM and sales routes
//! state rather than a second set of rules:
//!
//! * the caller's organization is resolved by the CRM's [`organization_of`], so a screen opened
//!   on `/accounting/*` with no query string shows **its** records rather than a prompt;
//! * a record of another organization is a `404`, never a `403` — a `403` would confirm it
//!   exists, and one organization's chart is the thing this module exists to keep apart;
//! * every mutation writes an audit row with the actor, what changed and the before/after.
//!
//! # The one message this file is really about
//!
//! The slice is done when "a manual balanced entry posts and an unbalanced one is **refused with
//! a visible message**", and the two halves of that sentence pull in opposite directions: the
//! refusal has to be *invisible to the schema* (a `CHECK`, so no future route can write an
//! unbalanced entry) and *legible to a person* (three numbers, so the operator does not re-add a
//! column by hand). Those are separate layers and this file is where they meet: the module raises
//! [`AccountingError::UnbalancedEntry`] carrying `debit_total`, `credit_total` and a **signed**
//! `difference`, and [`unbalanced_entry`] renders all three into the response. The status is a
//! `422`, not the `409` the rest of this family's "will not perform" uses, and the reason is
//! worth stating: nothing the caller typed is malformed and nothing is taken — the request was
//! well-formed and the **arithmetic** says no, which is the same distinction the inventory module
//! draws for a stock refusal.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_module_accounting::accounts::{
    self, AccountKind, AccountPatch, NewAccount, NewTaxRate, TaxRateKind, TaxRatePatch,
};
use omnion_module_accounting::journal::{self, NewJournalEntry};
use omnion_module_accounting::{AccountingError, JournalEntrySummary, JournalEntryView};
use serde::Deserialize;
use serde_json::{Value, json};
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

/// The query of the chart of accounts.
#[derive(Debug, Default, Deserialize)]
pub struct AccountListParams {
    /// One of the five kinds; absent for the whole chart.
    #[serde(default)]
    pub kind: Option<String>,
    /// Include deactivated accounts. The tree shows them greyed rather than hiding them, because
    /// an account that has disappeared from the chart is an account a past entry names and
    /// nobody can find.
    #[serde(default)]
    pub include_inactive: Option<bool>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The query of a tax-rate list.
#[derive(Debug, Default, Deserialize)]
pub struct TaxRateListParams {
    /// `sales` or `purchase`; absent for both.
    #[serde(default)]
    pub kind: Option<String>,
    /// Include deactivated rates.
    #[serde(default)]
    pub include_inactive: Option<bool>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The query of a journal list.
#[derive(Debug, Default, Deserialize)]
pub struct JournalListParams {
    /// `manual`, `invoice`, `payment` or `expense`.
    #[serde(default)]
    pub source: Option<String>,
    /// `YYYY-MM-DD`, the earliest entry date.
    #[serde(default)]
    pub from: Option<String>,
    /// `YYYY-MM-DD`, the latest entry date.
    #[serde(default)]
    pub to: Option<String>,
    /// Free text over the memo and the entry number.
    #[serde(default)]
    pub search: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of `POST /accounting/accounts/{id}/deactivate`.
#[derive(Debug, Default, Deserialize)]
pub struct DeactivateBody {
    /// Reactivate instead of deactivate. Present so the route is one path rather than two, and so
    /// the audit row records the intent in the request rather than in the route name.
    #[serde(default)]
    pub active: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// The chart of accounts
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/accounting/accounts` — the whole chart, one request.
///
/// The tree renders parent/child structure, and a screen that fetched children per expand would
/// show a spinner on every expand and report the wrong "used by N lines" total until the last one
/// loaded. The line counts are stamped onto each account by one grouped query for the same
/// reason.
pub async fn list_accounts(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<AccountListParams>,
) -> Result<Json<Vec<accounts::AccountView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let kind = match params.kind.as_deref() {
        None | Some("") => None,
        Some(raw) => Some(AccountKind::parse(raw).ok_or_else(|| {
            ApiError::bad_request(
                "invalid_accounting_query",
                format!("{raw:?} is not a kind — use asset, liability, equity, income or expense"),
            )
        })?),
    };

    let rows = accounts::list_accounts(
        state.db().pool(),
        organization_id,
        kind,
        params.include_inactive.unwrap_or(true),
    )
    .await?;
    Ok(Json(rows))
}

/// `POST /api/v1/accounting/accounts` — add an account to the chart.
pub async fn create_account(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewAccount>,
) -> Result<(StatusCode, Json<accounts::AccountView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let created = accounts::create_account(state.db().pool(), organization_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.account.created")
            .organization(organization_id)
            .target("accounting_account", created.id.to_string())
            .metadata(json!({ "account_id": created.id, "after": created.reference() }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("accounting.account.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(created.reference()),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /api/v1/accounting/accounts/{id}` — rename, re-parent or deactivate an account.
///
/// The **code is not patchable**, and that is the interesting part: a code is what a journal line
/// and every report row refer to, so renaming one after postings exist turns each of those
/// references into a claim about an account that never existed. The schema's
/// `unique (organization_id, code)` is the only thing stopping a duplicate, so the code is
/// immutable by contract rather than by convention.
pub async fn update_account(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(account_id): Path<Uuid>,
    body: Json<AccountPatch>,
) -> Result<Json<accounts::AccountView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = accounts::get_account(pool, organization_id, account_id).await?;
    let after = accounts::patch_account(pool, organization_id, account_id, &body.0).await?;

    let mut changed: Vec<&'static str> = Vec::new();
    if before.name != after.name {
        changed.push("name");
    }
    if before.active != after.active {
        changed.push("active");
    }
    if before.parent_id != after.parent_id {
        changed.push("parent_id");
    }
    if changed.is_empty() {
        // A no-op PATCH writes no audit row. An audit trail that records "nothing happened" for
        // every screen that saves without editing is a trail nobody reads.
        return Ok(Json(after));
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.account.updated")
            .organization(organization_id)
            .target("accounting_account", after.id.to_string())
            .metadata(json!({
                "account_id": after.id,
                "changed": changed,
                "before": before.reference(),
                "after": after.reference(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("accounting.account.updated")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({ "account_id": after.id, "changed": changed })),
    )
    .await;

    Ok(Json(after))
}

/// `POST /api/v1/accounting/accounts/{id}/deactivate` — close an account, never delete it.
///
/// A journal line references an account with `on delete restrict`, so a delete attempt is a
/// database error naming a constraint. Deactivating is the operation that actually answers "we do
/// not use this any more" while leaving every historical reference readable, and the audit row
/// records the count of lines the account is exposed in.
pub async fn deactivate_account(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(account_id): Path<Uuid>,
    body: Option<Json<DeactivateBody>>,
) -> Result<Json<accounts::AccountView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = accounts::get_account(pool, organization_id, account_id).await?;
    let active = body.and_then(|Json(b)| b.active).unwrap_or(false);
    let after = accounts::deactivate_account(pool, organization_id, account_id, active).await?;

    record(
        &state,
        NewAuditEntry::by_user(
            current.user.id,
            if active {
                "accounting.account.reactivated"
            } else {
                "accounting.account.deactivated"
            },
        )
        .organization(organization_id)
        .target("accounting_account", after.id.to_string())
        .metadata(json!({
            "account_id": after.id,
            "code": after.code,
            "journal_lines": after.line_count,
            "active": after.active,
        }))
        .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new(if active {
            "accounting.account.reactivated"
        } else {
            "accounting.account.deactivated"
        })
        .organization(organization_id)
        .actor(current.user.id)
        .payload(after.reference()),
    )
    .await;

    let _ = before;
    Ok(Json(after))
}

// ---------------------------------------------------------------------------------------------
// Tax rates
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/accounting/tax-rates` — every rate, for the editor and the line grid.
pub async fn list_tax_rates(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<TaxRateListParams>,
) -> Result<Json<Vec<accounts::TaxRateView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let kind = match params.kind.as_deref() {
        None | Some("") => None,
        Some(raw) => Some(TaxRateKind::parse(raw).ok_or_else(|| {
            ApiError::bad_request(
                "invalid_accounting_query",
                format!("{raw:?} is not a rate kind — use sales or purchase"),
            )
        })?),
    };

    let rows = accounts::list_tax_rates(
        state.db().pool(),
        organization_id,
        kind,
        params.include_inactive.unwrap_or(true),
    )
    .await?;
    Ok(Json(rows))
}

/// `POST /api/v1/accounting/tax-rates` — add a rate, optionally taking the default for its kind.
pub async fn create_tax_rate(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewTaxRate>,
) -> Result<(StatusCode, Json<accounts::TaxRateView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let created = accounts::create_tax_rate(state.db().pool(), organization_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.tax_rate.created")
            .organization(organization_id)
            .target("accounting_tax_rate", created.id.to_string())
            .metadata(json!({ "tax_rate_id": created.id, "after": created.reference() }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("accounting.tax_rate.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(created.reference()),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /api/v1/accounting/tax-rates/{id}` — edit a rate.
///
/// The percent is editable and **no issued document is rewritten**: an invoice line stores its own
/// `tax_percent` snapshot, which is the entire reason that column is a snapshot rather than a
/// join. Correcting a rate entered as 15 when it meant 5 must not change an invoice somebody
/// already sent, and this route is where that promise is kept.
pub async fn update_tax_rate(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(rate_id): Path<Uuid>,
    body: Json<TaxRatePatch>,
) -> Result<Json<accounts::TaxRateView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = accounts::get_tax_rate(pool, organization_id, rate_id).await?;
    let after = accounts::patch_tax_rate(pool, organization_id, rate_id, &body.0).await?;

    let mut changed: Vec<&'static str> = Vec::new();
    if before.name != after.name {
        changed.push("name");
    }
    if before.percent != after.percent {
        changed.push("percent");
    }
    if before.is_default != after.is_default {
        changed.push("is_default");
    }
    if before.active != after.active {
        changed.push("active");
    }
    if changed.is_empty() {
        return Ok(Json(after));
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.tax_rate.updated")
            .organization(organization_id)
            .target("accounting_tax_rate", after.id.to_string())
            .metadata(json!({
                "tax_rate_id": after.id,
                "changed": changed,
                "before": before.reference(),
                "after": after.reference(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("accounting.tax_rate.updated")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({ "tax_rate_id": after.id, "changed": changed })),
    )
    .await;

    Ok(Json(after))
}

// ---------------------------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/accounting/journal` — the entries, newest first, without their lines.
pub async fn list_journal(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<JournalListParams>,
) -> Result<Json<Vec<JournalEntrySummary>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;

    let source = match params.source.as_deref() {
        None | Some("") => None,
        Some(raw) => Some(
            omnion_module_accounting::EntrySource::parse(raw).ok_or_else(|| {
                ApiError::bad_request(
                    "invalid_accounting_query",
                    format!("{raw:?} is not a source — use manual, invoice, payment or expense"),
                )
            })?,
        ),
    };
    let from = parse_day(params.from.as_deref(), "from")?;
    let to = parse_day(params.to.as_deref(), "to")?;
    if let (Some(from), Some(to)) = (from, to)
        && from > to
    {
        return Err(ApiError::bad_request(
            "invalid_accounting_query",
            "the earliest date is after the latest date",
        ));
    }

    let rows = journal::list_entries(
        state.db().pool(),
        organization_id,
        source,
        from,
        to,
        params.search.as_deref(),
        params.limit,
    )
    .await?;
    Ok(Json(rows))
}

/// `GET /api/v1/accounting/journal/{id}` — one entry with its lines.
pub async fn get_journal_entry(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(entry_id): Path<Uuid>,
) -> Result<Json<JournalEntryView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        journal::get_entry(state.db().pool(), organization_id, entry_id).await?,
    ))
}

/// `POST /api/v1/accounting/journal` — post a manual entry.
///
/// The route's job is to turn an unbalanced entry into a **sentence a bookkeeper can act on**. The
/// module has already refused the write and named the two totals; this is where the refusal
/// becomes a `422` whose message reads "the entry does not balance: debits 100.00, credits 90.00,
/// difference 10.00" and whose `details` carry the same three numbers for a screen that wants to
/// show them next to the grid.
///
/// It is a `422` rather than the `409` this file's other refusals use, and the difference is not
/// cosmetic: nothing typed is malformed and nothing is taken — the request was well-formed and
/// the arithmetic says no. The same distinction the inventory module draws when a warehouse does
/// not have the stock.
pub async fn post_journal_entry(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewJournalEntry>,
) -> Result<(StatusCode, Json<JournalEntryView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let posted =
        journal::post_entry(state.db().pool(), organization_id, &body.0, Some(current.user.id))
            .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.journal.posted")
            .organization(organization_id)
            .target("accounting_journal_entry", posted.id.to_string())
            .metadata(json!({
                "entry_id": posted.id,
                "entry_number": posted.entry_number,
                "debit_total": posted.debit_total,
                "credit_total": posted.credit_total,
                "balanced": posted.balanced,
                "lines": posted.lines.len(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("accounting.journal.posted")
            .organization(organization_id)
            .actor(current.user.id)
            // The payload carries the totals and the line count, never the lines: an event travels
            // to every webhook subscriber, and a fifty-line entry would make a five-line summary
            // into a document.
            .payload(json!({
                "entry_id": posted.id,
                "entry_number": posted.entry_number,
                "debit_total": posted.debit_total,
                "credit_total": posted.credit_total,
                "balanced": posted.balanced,
                "line_count": posted.line_count(),
            })),
    )
    .await;

    Ok((StatusCode::CREATED, Json(posted)))
}

// ---------------------------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------------------------

/// The organization a list or a mutation runs against.
#[derive(Debug, Default, Deserialize)]
pub struct OrganizationParam {
    /// Organization to act on. Only a platform account may name one; everybody else gets their own.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// Parse a `YYYY-MM-DD` filter, naming the parameter that was wrong.
///
/// `pub(crate)` because the invoice routes in [`super::accounting_invoices`] filter by day too,
/// and a second date parser is a second definition of what a malformed day answers with.
pub(crate) fn parse_day(
    raw: Option<&str>,
    field: &'static str,
) -> Result<Option<time::Date>, ApiError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => omnion_module_accounting::dates::parse(value).map(Some).map_err(|_| {
            ApiError::bad_request(
                "invalid_accounting_query",
                format!("{field} is a date such as 2026-01-31, not {value:?}"),
            )
        }),
    }
}

/// Record an event, tolerating a failure.
///
/// A journal entry that is posted but whose event did not emit is still a posted entry; refusing
/// the response would tell the bookkeeper their entry failed when it did not, which is a worse
/// outcome than a missing automation trigger.
///
/// `pub(crate)` because the invoice routes emit through this one function rather than carrying a
/// second copy of the "log and carry on" decision — a module that forgets to tolerate its own
/// event failure answers 500 for a write that succeeded.
pub(crate) async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the accounting event could not be recorded");
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

impl From<AccountingError> for ApiError {
    /// The accounting module's refusals.
    ///
    /// Every status here is the module's own [`omnion_module_accounting::status_of`] except the
    /// one that matters: an unbalanced entry is a `422`, not the `409` the rest of this family
    /// returns for "the module will not perform this". Nothing typed is malformed, nothing is
    /// taken, and the request will never succeed unchanged — it is well-formed and the
    /// **arithmetic** refuses it.
    fn from(error: AccountingError) -> Self {
        match error {
            AccountingError::Invalid {
                entity,
                field,
                message,
            } => Self::bad_request("invalid_accounting_record", message)
                .with_details(json!({ "entity": entity, "field": field })),
            AccountingError::InvalidQuery(message) => {
                Self::bad_request("invalid_accounting_query", message)
            }
            AccountingError::InvalidNumber {
                entity,
                field,
                source,
            } => Self::bad_request("invalid_accounting_record", source.to_string())
                .with_details(json!({ "entity": entity, "field": field })),
            AccountingError::NotFound(kind) => Self::new(
                StatusCode::NOT_FOUND,
                match kind {
                    "account" => "accounting_account_not_found",
                    "tax_rate" => "accounting_tax_rate_not_found",
                    "journal_entry" => "accounting_journal_entry_not_found",
                    _ => "accounting_record_not_found",
                },
                // The kind and nothing else: a caller must not be able to learn that a record
                // exists in another organization by comparing this with a 403.
                format!("no such {kind} in this organization"),
            ),
            AccountingError::NameTaken { entity, code } => Self::new(
                StatusCode::CONFLICT,
                "accounting_name_taken",
                format!("another {entity} of this organization is already called {code}"),
            )
            .with_details(json!({ "entity": entity, "code": code })),
            AccountingError::UnbalancedEntry {
                debit_total,
                credit_total,
                difference,
            } => {
                // Built by the same function a test and a screen read, so the sentence in the
                // response, the sentence in the docs and the sentence in an assertion cannot
                // drift into three versions of "the entry does not balance".
                let (message, details, status) = unbalanced::render(&AccountingError::UnbalancedEntry {
                    debit_total: debit_total.clone(),
                    credit_total: credit_total.clone(),
                    difference: difference.clone(),
                });
                Self::new(
                    StatusCode::from_u16(status).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY),
                    "accounting_journal_unbalanced",
                    message,
                )
                // The three numbers travel in `details` as well as in the message: a screen shows
                // the sentence, and a grid footer shows the columns. Both read the same figures,
                // and the signed difference is what says which side is short.
                .with_details(details)
            }
            AccountingError::NotAllowed(message) => {
                Self::new(StatusCode::CONFLICT, "accounting_not_allowed", message)
            }
            AccountingError::AccountInUse { code, lines } => Self::new(
                StatusCode::CONFLICT,
                "accounting_account_in_use",
                format!("account {code} has {lines} journal lines; deactivate it instead"),
            )
            .with_details(json!({ "code": code, "journal_lines": lines })),
            AccountingError::ForeignKey { kind, id } => Self::new(
                StatusCode::NOT_FOUND,
                "accounting_foreign_key_not_found",
                format!("{kind} {id} is not in this organization"),
            ),
            // The second `422` in the family (REQ-054 slice 3), and it earns one for the same
            // reason the unbalanced entry does: the request is well formed, the invoice is real,
            // nothing is taken, and it will never succeed unchanged — the arithmetic refuses it.
            // The three numbers travel in `details` for the same reason they do above: the screen
            // shows the sentence, the allocation grid shows the figures.
            AccountingError::OverAllocation {
                invoice_number,
                attempted,
                outstanding,
            } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "accounting_over_allocation",
                format!(
                    "invoice {invoice_number} has {outstanding} outstanding, which is less than \
                     the {attempted} this payment allocates to it"
                ),
            )
            .with_details(json!({
                "invoice_number": invoice_number,
                "attempted": attempted,
                "outstanding": outstanding,
            })),
            // A stored amount this build cannot read. A `500` and not a `400`: nothing the
            // caller sent is wrong, so telling them to fix their request would send them to a
            // form that is already correct.
            AccountingError::InvalidAmount { message } => {
                tracing::error!(error = %message, "a stored accounting amount is unreadable");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "accounting_storage_error",
                    "a stored amount in this organization could not be read",
                )
            }
            AccountingError::Database(inner) => {
                tracing::error!(error = %inner, "the accounting store refused a query");
                // The underlying message is attached to `details` **in a non-production build
                // only**. A 500 that says "could not answer" sends a person to the logs to find
                // out why, and the logs are not where they are standing; a 500 that echoes the
                // driver text in production is a schema leak. The `Option::is_none()` branch is
                // the test build, and the gate is the same one the rest of the platform uses for
                // diagnostics: no way to reach it from a deployment.
                let message = if cfg!(debug_assertions) {
                    format!("the accounting store could not answer: {inner}")
                } else {
                    "the accounting store could not answer".to_owned()
                };
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "accounting_storage_error",
                    message,
                )
            }
        }
    }
}

/// The status an unbalanced entry answers with, as the three numbers it reports.
///
/// A `422` and not the `409` the rest of this file's refusals use, and the difference is not
/// cosmetic: nothing the caller typed is malformed, nothing is taken, and the request will never
/// succeed unchanged — it is well-formed and the **arithmetic** refuses it. The same distinction
/// the inventory module draws when a warehouse does not have the stock.
pub mod unbalanced {
    use serde_json::json;

    use super::AccountingError;

    /// The message, the `details` object and the `422` status — one function so a test and a
    /// screen cannot describe the same failure three slightly different ways.
    ///
    /// `super::AccountingError` and not `crate::AccountingError`: inside a route module `crate`
    /// is the **API** crate, and the error type lives in the module dependency. A `crate::` path
    /// here fails to resolve for a reason that has nothing to do with the accounting crate.
    #[must_use]
    pub fn render(error: &AccountingError) -> (String, serde_json::Value, u16) {
        match error {
            AccountingError::UnbalancedEntry {
                debit_total,
                credit_total,
                difference,
            } => (
                format!(
                    "the entry does not balance: debits {debit_total}, credits {credit_total}, \
                     difference {difference}"
                ),
                json!({
                    "debit_total": debit_total,
                    "credit_total": credit_total,
                    "difference": difference,
                }),
                422,
            ),
            other => (other.to_string(), json!({}), 409),
        }
    }
}

/// A small helper the tests use to prove the mapping without a running server: the status a
/// refusal answers with.
///
/// It is a function rather than a table in a test because the API surface is the thing being
/// proved, and a table would only prove the table. `AccountingError` is not `Clone` on purpose —
/// it carries a `sqlx::Error` and a `DecimalError`, and cloning a failure to inspect it is how a
/// test ends up asserting on a copy that is not the one the route saw — so the mapping is applied
/// by constructing the `ApiError` from a **fresh** refusal instead.
#[must_use]
pub fn refusal_status(error: &AccountingError) -> u16 {
    let api: ApiError = clone_refusal(error).into();
    api.status().as_u16()
}

/// Whether a refusal is one a form should render under a field rather than as a banner.
#[must_use]
pub fn refusal_field(error: &AccountingError) -> Option<String> {
    match error {
        AccountingError::Invalid { field, .. } => Some((*field).to_owned()),
        AccountingError::InvalidNumber { field, .. } => Some((*field).to_owned()),
        _ => None,
    }
}

/// The three numbers an unbalanced entry reports, as the detail object a grid footer binds to.
#[must_use]
pub fn balance_details(error: &AccountingError) -> Option<Value> {
    match error {
        AccountingError::UnbalancedEntry { .. } => {
            let (_, details, _) = unbalanced::render(error);
            Some(details)
        }
        _ => None,
    }
}

/// Rebuild an equivalent refusal so it can be handed to `From`, without cloning the original's
/// database error.
fn clone_refusal(error: &AccountingError) -> AccountingError {
    match error {
        AccountingError::Invalid {
            entity,
            field,
            message,
        } => AccountingError::Invalid {
            entity,
            field,
            message: message.clone(),
        },
        AccountingError::InvalidQuery(message) => AccountingError::InvalidQuery(message.clone()),
        AccountingError::NotFound(kind) => AccountingError::NotFound(kind),
        AccountingError::NameTaken { entity, code } => AccountingError::NameTaken {
            entity,
            code: code.clone(),
        },
        AccountingError::UnbalancedEntry {
            debit_total,
            credit_total,
            difference,
        } => AccountingError::UnbalancedEntry {
            debit_total: debit_total.clone(),
            credit_total: credit_total.clone(),
            difference: difference.clone(),
        },
        AccountingError::NotAllowed(message) => AccountingError::NotAllowed(message.clone()),
        AccountingError::AccountInUse { code, lines } => AccountingError::AccountInUse {
            code: code.clone(),
            lines: *lines,
        },
        AccountingError::ForeignKey { kind, id } => AccountingError::ForeignKey { kind, id: *id },
        // A storage error is not reconstructible and does not need to be: it maps to a `500`
        // whatever it was, and no test asserts on the wording of an I/O failure.
        AccountingError::Database(_) => AccountingError::NotAllowed("storage error".to_owned()),
        AccountingError::InvalidNumber {
            entity,
            field,
            source,
        } => AccountingError::InvalidNumber {
            entity,
            field,
            source: source.clone(),
        },
        // The payment refusals (REQ-054 slice 3). Cloned in full, unlike `Database`, because
        // these are the two the API layer turns into a `422` **with numbers in `details`** — a
        // reconstruction that dropped them would answer with the right status and the wrong
        // message, which is the failure the walk that asserts on them is written to catch.
        AccountingError::OverAllocation {
            invoice_number,
            attempted,
            outstanding,
        } => AccountingError::OverAllocation {
            invoice_number: invoice_number.clone(),
            attempted: attempted.clone(),
            outstanding: outstanding.clone(),
        },
        AccountingError::InvalidAmount { message } => AccountingError::InvalidAmount {
            message: message.clone(),
        },
    }
}
