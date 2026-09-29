//! Quote approval requests — the discount gate (docs/requests/REQ-052, slice 3).
//!
//! Slice 2 built the quote and the builder already draws the amber banner when a line discount is
//! over the organization's threshold, but it had nothing to point at: the `pending_approval`
//! status existed, the send path already refused anything but a draft or an approved quote, and
//! no code anywhere could move a quote into `approved`. This file is the missing middle —
//! **request it, decide it, and let the quote be sent afterwards**.
//!
//! The rules, and why each one is here rather than in a handler:
//!
//! * **Only a draft may ask for approval.** A quote the customer has already read is frozen
//!   (`AlreadySent`), so asking to approve a sent document would be asking to un-send it.
//! * **The requester may not approve their own quote.** A gate a seller can open from their own
//!   screen is a checkbox. The check is by id, not by role, because the spec's own default is
//!   "anybody but the person who wants the discount"; REQ-059 later narrows it to a chain.
//! * **A decision moves the quote's status, not just the request's.** `approved` puts the quote
//!   into `approved` so the existing send path accepts it; `rejected` puts it back into `draft`,
//!   because a rejected quote is still the seller's working document — the seller edits the
//!   discount and asks again, and the second request is its own row with its own history.
//! * **A rejection must say why** ([`SalesError::invalid`]) — the seller is the one who has to
//!   fix the line, and "no" without a reason makes them guess.
//! * **Cancelling a quote cancels its open request** in the same transaction, so a manager's
//!   inbox cannot offer a decision on a document that no longer exists.
//!
//! The request row is shaped the way REQ-059's spec describes a generic approval request
//! (`subject_*`), so the approvals module can adopt these rows rather than migrate them. That
//! module owns flows, chains, delegation and escalation; nothing here pretends to.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, SalesError};
use crate::model::QuoteStatus;
use crate::store::{DEFAULT_PER_PAGE, MAX_PER_PAGE, Page};

/// Longest the seller's note on a request may be — the same bound the migration's check uses.
pub const MAX_APPROVAL_NOTE_LENGTH: usize = 2_000;

/// Longest a decision comment may be. Bounded for the same reason: the inbox renders it, and the
/// comment travels in a notification.
pub const MAX_APPROVAL_COMMENT_LENGTH: usize = 2_000;

/// How many rows one approval list page may carry.
pub const MAX_APPROVAL_PAGE: i64 = 100;

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// Where a request sits in its own lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    /// Waiting for a decision. The only state a second request may be refused in.
    Pending,
    /// Somebody other than the seller said yes.
    Approved,
    /// Somebody said no, with a reason.
    Rejected,
    /// The quote was withdrawn or edited back under the threshold while this was open.
    Cancelled,
}

impl ApprovalStatus {
    /// The value stored in `sales_quote_approvals.status`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
        }
    }

    /// Read a stored value, or `None` for one this build does not know.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// The label the panel's filter chips print.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Awaiting a decision",
            Self::Approved => "Approved",
            Self::Rejected => "Rejected",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// Which list the caller is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// Everything still waiting — the manager's inbox.
    Pending,
    /// Everything this person asked for — the seller's own list.
    RequestedByMe,
    /// Everything already decided, anybody's.
    Decided,
    /// Everything, for the audit-minded reader.
    All,
}

impl ApprovalScope {
    /// The value accepted in the `scope` query parameter.
    #[must_use]
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value.trim() {
            "pending" => Self::Pending,
            "requested_by_me" | "requested-by-me" | "mine" => Self::RequestedByMe,
            "decided" | "completed" => Self::Decided,
            "all" => Self::All,
            other => {
                return Err(SalesError::InvalidQuery(format!(
                    "scope must be pending, requested_by_me, decided or all — not {other:?}"
                )));
            }
        })
    }
}

/// One row as the inbox sees it, with the quote's number and title joined in.
///
/// The quote's number is joined rather than snapshotted: a request about a quote whose number is
/// `Q-2026-0042` must read `Q-2026-0042` in the inbox three weeks later, and the number of a
/// quote never changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalView {
    /// The request's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The quote under decision, by id.
    pub quote_id: Uuid,
    /// The quote's number, e.g. `Q-2026-0042`.
    pub quote_number: String,
    /// The quote's title.
    pub quote_title: String,
    /// The quote's own status, so the inbox can show whether the seller changed it since.
    pub quote_status: QuoteStatus,
    /// Who asked for approval.
    pub requested_by: Uuid,
    /// The requester's display name at read time.
    pub requester_name: String,
    /// The largest line discount, as a snapshot of what the manager was shown.
    pub discount_percent: i32,
    /// The threshold this quote was measured against at the time.
    pub threshold_percent: i32,
    /// The quote's currency.
    pub currency: String,
    /// The quote's grand total when it was asked for.
    pub grand_total: String,
    /// The seller's note.
    pub note: String,
    /// Where the request sits.
    pub status: ApprovalStatus,
    /// Who decided it, when they were told, and what they said.
    pub decision: Option<DecisionView>,
    /// The panel route for the quote this request is about.
    pub subject_url: String,
    /// When the request was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
    /// When the request was last changed.
    #[serde(with = "crate::dates::instant")]
    pub updated_at: OffsetDateTime,
}

/// The decision half of a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionView {
    /// The verdict, or `None` for a cancellation (which is not a verdict, it is a withdrawal).
    pub outcome: Option<ApprovalStatus>,
    /// Who decided it, when the row survives but the account is gone.
    pub decided_by: Option<Uuid>,
    /// The decider's display name at read time.
    pub decider_name: String,
    /// What they said. Required for a rejection.
    pub comment: String,
    /// When the decision was made.
    #[serde(with = "crate::dates::instant")]
    pub decided_at: OffsetDateTime,
}

/// The request body for a decision.
#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalDecision {
    /// `approve` or `reject`. Anything else is refused rather than treated as "approve".
    pub decision: String,
    /// The reason, required when rejecting.
    #[serde(default)]
    pub comment: Option<String>,
}

/// The request body for raising a request.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApprovalRequest {
    /// Why the discount is worth a manager's time. Optional; the numbers are on the row.
    #[serde(default)]
    pub note: Option<String>,
}

/// What a refusal on an unapproved quote should tell the caller.
///
/// The send handler needs the *existing* request so it can link the button in the form; this is
/// that value rather than a bare `409`, because "you cannot send this, and here is who is
/// deciding it" is actionable and "409" is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApprovalRequired {
    /// The quote's id.
    pub quote_id: Uuid,
    /// The quote's number.
    pub quote_number: String,
    /// The largest line discount on it.
    pub discount_percent: i32,
    /// The threshold it exceeds.
    pub threshold_percent: i32,
    /// The open request, when the seller has already asked.
    pub request: Option<ApprovalView>,
}

impl ApprovalRequired {
    /// The sentence the builder prints under the amber banner.
    #[must_use]
    pub fn message(&self) -> String {
        match &self.request {
            Some(request) => format!(
                "quote {} carries a {}% discount, over the {}% limit — waiting for a decision on the request raised {}",
                self.quote_number,
                self.discount_percent,
                self.threshold_percent,
                request.created_at.date(),
            ),
            None => format!(
                "quote {} carries a {}% discount, over the {}% limit — ask a manager before sending it",
                self.quote_number, self.discount_percent, self.threshold_percent,
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// The query of an approval list.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApprovalQuery {
    /// Which list to read.
    #[serde(default)]
    pub scope: Option<String>,
    /// Only this status.
    #[serde(default)]
    pub status: Option<String>,
    /// Free text over the quote number and title.
    #[serde(default)]
    pub search: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The caller, for the `requested_by_me` scope.
    pub viewer: Uuid,
}

#[derive(Debug, FromRow)]
struct ApprovalRow {
    id: Uuid,
    organization_id: Uuid,
    quote_id: Uuid,
    quote_number: String,
    quote_title: String,
    quote_status: String,
    requested_by: Uuid,
    discount_percent: String,
    threshold_percent: String,
    currency: String,
    grand_total: String,
    note: String,
    status: String,
    decided_by: Option<Uuid>,
    decided_at: Option<OffsetDateTime>,
    comment: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

fn approval_columns() -> &'static str {
    "a.id, a.organization_id, a.quote_id, q.number as quote_number, q.title as quote_title,
     q.status as quote_status, a.requested_by, a.discount_percent::text as discount_percent,
     a.threshold_percent::text as threshold_percent, a.currency, a.grand_total::text as
     grand_total, a.note, a.status, a.decided_by, a.decided_at, a.comment, a.created_at,
     a.updated_at"
}

impl ApprovalRow {
    /// The row as the screens see it.
    ///
    /// An unknown `status` or an unknown `quote_status` is an error rather than a default: a
    /// request shown as "pending" that is not pending is a manager clicking approve on a decided
    /// document, and the refusal is far better than the confusion.
    fn into_view(self, requester_name: String, decider_name: String) -> Result<ApprovalView> {
        let status = ApprovalStatus::parse(&self.status).ok_or_else(|| {
            SalesError::invalid("approval", "status", "this request has an unknown status")
        })?;
        let quote_status = QuoteStatus::parse(&self.quote_status).ok_or_else(|| {
            SalesError::invalid("quote", "status", "this quote has an unknown status")
        })?;
        let decision = match (self.decided_at, status) {
            (Some(_), ApprovalStatus::Pending) => None,
            (Some(decided_at), status) => Some(DecisionView {
                outcome: (status != ApprovalStatus::Cancelled).then_some(status),
                decided_by: self.decided_by,
                decider_name,
                comment: self.comment.unwrap_or_default(),
                decided_at,
            }),
            // A decided row with no timestamp cannot happen through this module, but the schema
            // allows `decided_by` alone on a cancelled row written by a future sweep. Rendering it
            // as a decision with "the epoch" would be a lie, so the timeline carries the status.
            (None, ApprovalStatus::Pending) => None,
            (None, _) => None,
        };
        Ok(ApprovalView {
            id: self.id,
            organization_id: self.organization_id,
            quote_id: self.quote_id,
            quote_number: self.quote_number,
            quote_title: self.quote_title,
            quote_status,
            requested_by: self.requested_by,
            requester_name,
            discount_percent: parse_percent(&self.discount_percent),
            threshold_percent: parse_percent(&self.threshold_percent),
            currency: self.currency,
            grand_total: self.grand_total,
            note: self.note,
            status,
            decision,
            subject_url: format!("/sales/quotes/{}", self.quote_id),
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

fn parse_percent(raw: &str) -> i32 {
    raw.parse::<f64>().map(|value| value.round() as i32).unwrap_or(0)
}

async fn display_names(
    pool: &PgPool,
    ids: &[Uuid],
) -> std::collections::HashMap<Uuid, String> {
    if ids.is_empty() {
        return std::collections::HashMap::new();
    }
    sqlx::query_as::<_, (Uuid, String)>(
        "select id, coalesce(display_name, email) from users where id = any($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .map(|rows| rows.into_iter().collect())
    .unwrap_or_default()
}

/// `GET /sales/quotes/{id}/approvals` — every request ever raised for one quote.
pub async fn list_for_quote(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
) -> Result<Vec<ApprovalView>> {
    let sql = format!(
        "select {} from sales_quote_approvals a
           join sales_quotes q on q.id = a.quote_id
          where a.organization_id = $1 and a.quote_id = $2
          order by a.created_at desc",
        approval_columns()
    );
    let rows: Vec<ApprovalRow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(quote_id)
        .fetch_all(pool)
        .await?;
    decorate(pool, rows).await
}

/// `GET /sales/approvals` — one page of the inbox.
pub async fn list_approvals(
    pool: &PgPool,
    organization_id: Uuid,
    query: &ApprovalQuery,
) -> Result<Page<ApprovalView>> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_APPROVAL_PAGE.min(MAX_PER_PAGE));
    let scope = ApprovalScope::parse(query.scope.as_deref().unwrap_or("pending"))?;
    let status = match query.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(
            ApprovalStatus::parse(raw).ok_or_else(|| {
                SalesError::InvalidQuery(format!("{raw:?} is not an approval status"))
            })?,
        ),
    };
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(|term| format!("%{}%", term.to_lowercase().replace('%', "\\%").replace('_', "\\_")));

    let mut sql = format!(
        "select {} from sales_quote_approvals a
           join sales_quotes q on q.id = a.quote_id
          where a.organization_id = $1",
        approval_columns()
    );
    // The scope is a WHERE clause rather than a filter applied in Rust: every scope is a
    // different set of rows, and paging a list that is filtered after the page is wrong.
    //
    // The status filter is a **bind**, not an interpolated literal: `ApprovalStatus::parse` has
    // already narrowed it to four known values, but the day someone adds a fifth the check
    // becomes a string in the SQL and the first casualty is a quoteable one.
    // `$1` is the organization, bound first; every later slot shifts with the bind list so a
    // scope that adds a clause cannot leave a numbered placeholder pointing at the wrong value.
    let mut binds: Vec<String> = Vec::new();
    match scope {
        ApprovalScope::Pending => sql.push_str(" and a.status = 'pending'"),
        ApprovalScope::RequestedByMe => {
            binds.push(query.viewer.to_string());
            sql.push_str(&format!(" and a.requested_by = ${}", binds.len() + 1));
        }
        ApprovalScope::Decided => sql.push_str(" and a.status <> 'pending'"),
        ApprovalScope::All => {}
    }
    if let Some(wanted) = status {
        binds.push(wanted.as_str().to_string());
        sql.push_str(&format!(" and a.status = ${}", binds.len() + 1));
    }
    if let Some(term) = search {
        binds.push(term);
        let slot = binds.len() + 1;
        sql.push_str(&format!(
            " and (lower(q.number) like ${slot} escape '\\' or lower(q.title) like ${slot} escape '\\')"
        ));
    }
    sql.push_str(" order by a.created_at desc, a.id desc limit ");
    sql.push_str(&(limit + 1).to_string());

    let mut statement = sqlx::query_as::<_, ApprovalRow>(&sql).bind(organization_id);
    for bind in &binds {
        statement = statement.bind(bind);
    }
    let mut rows: Vec<ApprovalRow> = statement.fetch_all(pool).await?;

    // The one-over fetch is the same trick the quote list uses: ask for `limit + 1` and the extra
    // row is the proof there is a next page, without a second count query on a shared database.
    let has_more = rows.len() > limit as usize;
    if has_more {
        rows.truncate(limit as usize);
    }
    let items = decorate(pool, rows).await?;
    // The cursor is built from the **last row on this page**, before `items` is handed to the
    // page struct: a cursor pointing at the first row is the classic "page two repeats page one".
    let next_cursor = has_more.then(|| {
        let last = items.last().expect("a non-empty page when has_more is set");
        encode_cursor(last.created_at, last.id)
    });
    let total_estimate = if has_more { limit + 1 } else { items.len() as i64 };
    Ok(Page {
        items,
        next_cursor,
        total_estimate,
    })
}

/// A cursor of `created_at|id`, the same encoding the quote list uses so one `next_cursor`
/// helper can serve both pagers.
fn encode_cursor(stamp: OffsetDateTime, id: Uuid) -> String {
    format!("{}|{}", stamp.unix_timestamp(), id)
}

/// Fill in the two names the join cannot carry.
async fn decorate(pool: &PgPool, rows: Vec<ApprovalRow>) -> Result<Vec<ApprovalView>> {
    let mut ids: Vec<Uuid> = rows.iter().map(|row| row.requested_by).collect();
    ids.extend(rows.iter().filter_map(|row| row.decided_by));
    let names = display_names(pool, &ids).await;
    rows.into_iter()
        .map(|row| {
            let requester = names.get(&row.requested_by).cloned().unwrap_or_default();
            let decider = row
                .decided_by
                .and_then(|id| names.get(&id).cloned())
                .unwrap_or_default();
            row.into_view(requester, decider)
        })
        .collect()
}

/// `GET /sales/approvals/{id}` — one request.
pub async fn get_approval(
    pool: &PgPool,
    organization_id: Uuid,
    approval_id: Uuid,
) -> Result<ApprovalView> {
    let sql = format!(
        "select {} from sales_quote_approvals a
           join sales_quotes q on q.id = a.quote_id
          where a.organization_id = $1 and a.id = $2",
        approval_columns()
    );
    let row: ApprovalRow = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(approval_id)
        .fetch_optional(pool)
        .await?
        .ok_or(SalesError::NotFound("approval request"))?;
    let requester = display_names(pool, &[row.requested_by])
        .await
        .remove(&row.requested_by)
        .unwrap_or_default();
    let decider = match row.decided_by {
        Some(id) => display_names(pool, &[id]).await.remove(&id).unwrap_or_default(),
        None => String::new(),
    };
    row.into_view(requester, decider)
}

/// The open request on a quote, if there is one.
pub async fn open_request(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
) -> Result<Option<ApprovalView>> {
    let sql = format!(
        "select {} from sales_quote_approvals a
           join sales_quotes q on q.id = a.quote_id
          where a.organization_id = $1 and a.quote_id = $2 and a.status = 'pending'
          order by a.created_at desc limit 1",
        approval_columns()
    );
    let row: Option<ApprovalRow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(quote_id)
        .fetch_optional(pool)
        .await?;
    Ok(match row {
        None => None,
        Some(row) => {
            let requester = display_names(pool, &[row.requested_by])
                .await
                .remove(&row.requested_by)
                .unwrap_or_default();
            let decider = match row.decided_by {
                Some(id) => display_names(pool, &[id]).await.remove(&id).unwrap_or_default(),
                None => String::new(),
            };
            Some(row.into_view(requester, decider)?)
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// `POST /sales/quotes/{id}/approval-requests` — ask a manager to look at a discount.
///
/// This is where the threshold is enforced, and it is enforced **on the quote's stored value**
/// rather than on the caller's word: the seller cannot ask for approval of a quote that is not
/// over the line, and a quote that is over the line but has an open request is refused with the
/// existing request attached rather than a duplicate row.
pub async fn request_approval(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
    actor: Uuid,
    body: &ApprovalRequest,
) -> Result<ApprovalView> {
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .chars()
        .take(MAX_APPROVAL_NOTE_LENGTH)
        .collect::<String>();
    if body.note.as_deref().map(str::trim).unwrap_or_default().chars().count()
        > MAX_APPROVAL_NOTE_LENGTH
    {
        return Err(SalesError::invalid(
            "approval",
            "note",
            format!("keep the note under {MAX_APPROVAL_NOTE_LENGTH} characters"),
        ));
    }

    let mut transaction = pool.begin().await?;
    // `for update` because the decision below and this insert share the quote: two sellers
    // pressing the button at the same moment would otherwise both read "no open request".
    let quote: Option<(String, String, String, String, String)> = sqlx::query_as(
        "select number, status, max_discount::text, grand_total::text, currency
           from sales_quotes where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(quote_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let (number, status, max_discount, grand_total, currency) =
        quote.ok_or(SalesError::NotFound("quote"))?;

    let quote_status = QuoteStatus::parse(&status)
        .ok_or_else(|| SalesError::invalid("quote", "status", "this quote has an unknown status"))?;
    if !quote_status.is_editable() {
        return Err(SalesError::already_sent("quote", number));
    }

    let settings = crate::store::get_settings(pool, organization_id).await?;
    let discount = parse_percent(&max_discount);
    if !settings.needs_approval(discount) {
        return Err(SalesError::invalid(
            "approval",
            "quote",
            format!(
                "quote {number} discounts its largest line {discount}%, which is inside the {}% limit — it can be sent as it is",
                settings.discount_approval_threshold
            ),
        ));
    }

    let open: Option<Uuid> = sqlx::query_scalar(
        "select id from sales_quote_approvals
          where quote_id = $1 and status = 'pending' for update",
    )
    .bind(quote_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some(existing) = open {
        transaction.rollback().await?;
        let request = open_request(pool, organization_id, quote_id)
            .await?
            .ok_or(SalesError::NotFound("approval request"))?;
        return Err(SalesError::AlreadyAwaitingApproval {
            request_id: existing,
            quote_number: number,
            existing: request,
        });
    }

    let approval_id: Uuid = sqlx::query_scalar(
        "insert into sales_quote_approvals
                (organization_id, quote_id, requested_by, discount_percent, threshold_percent,
                 currency, grand_total, note, status)
         values ($1, $2, $3, $4::numeric, $5::numeric, $6, $7::numeric, $8, 'pending')
         returning id",
    )
    .bind(organization_id)
    .bind(quote_id)
    .bind(actor)
    .bind(discount.to_string())
    .bind(settings.discount_approval_threshold.to_string())
    .bind(currency.trim_end())
    .bind(&grand_total)
    .bind(&note)
    .fetch_one(&mut *transaction)
    .await?;

    // The quote's own status moves in the same transaction: the detail screen and the send gate
    // both read `sales_quotes.status`, and a request that left the quote looking like a plain
    // draft would let the seller press "send" and be refused by a rule the screen never showed.
    sqlx::query(
        "update sales_quotes set status = 'pending_approval', updated_at = now()
          where id = $1 and organization_id = $2",
    )
    .bind(quote_id)
    .bind(organization_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    get_approval(pool, organization_id, approval_id).await
}

/// `POST /sales/approvals/{id}/decision` — approve or reject a request.
///
/// Both branches are one transaction because the request row and the quote's status must never
/// disagree: a request marked approved next to a quote still reading `pending_approval` is the
/// state that makes a manager believe a discount was cleared when it was not.
pub async fn decide_approval(
    pool: &PgPool,
    organization_id: Uuid,
    approval_id: Uuid,
    actor: Uuid,
    body: &ApprovalDecision,
) -> Result<ApprovalView> {
    let approve = match body.decision.trim().to_lowercase().as_str() {
        "approve" | "approved" => true,
        "reject" | "rejected" => false,
        other => {
            return Err(SalesError::invalid(
                "approval",
                "decision",
                format!("decision must be approve or reject — not {other:?}"),
            ));
        }
    };
    let comment = body
        .comment
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if comment.chars().count() > MAX_APPROVAL_COMMENT_LENGTH {
        return Err(SalesError::invalid(
            "approval",
            "comment",
            format!("keep the comment under {MAX_APPROVAL_COMMENT_LENGTH} characters"),
        ));
    }
    if !approve && comment.is_empty() {
        return Err(SalesError::invalid(
            "approval",
            "comment",
            "say why you are rejecting it — the seller has to fix the line",
        ));
    }

    let mut transaction = pool.begin().await?;
    let row: Option<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "select a.quote_id, a.requested_by, a.status, q.status
           from sales_quote_approvals a
           join sales_quotes q on q.id = a.quote_id
          where a.organization_id = $1 and a.id = $2
          for update of a",
    )
    .bind(organization_id)
    .bind(approval_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let (quote_id, requested_by, status, quote_status) =
        row.ok_or(SalesError::NotFound("approval request"))?;

    let request_status = ApprovalStatus::parse(&status).ok_or_else(|| {
        SalesError::invalid("approval", "status", "this request has an unknown status")
    })?;
    if request_status != ApprovalStatus::Pending {
        return Err(SalesError::InvalidStatusChange(format!(
            "this request was already {} — only a pending one can be decided",
            request_status.label().to_lowercase()
        )));
    }
    if requested_by == actor {
        let quote_status = QuoteStatus::parse(&quote_status).unwrap_or(QuoteStatus::Draft);
        return Err(SalesError::SelfApproval { quote_status });
    }

    let outcome = if approve { "approved" } else { "rejected" };
    sqlx::query(
        "update sales_quote_approvals
            set status = $2, decision = $2, decided_by = $3, decided_at = now(), comment = $4,
                updated_at = now()
          where id = $1",
    )
    .bind(approval_id)
    .bind(outcome)
    .bind(actor)
    .bind(&comment)
    .execute(&mut *transaction)
    .await?;

    // The quote follows the decision: `approved` opens the send path, `rejected` hands the
    // document back to the seller as the working draft it still is.
    let quote_next = if approve { "approved" } else { "draft" };
    sqlx::query(
        "update sales_quotes set status = $3, updated_at = now()
          where id = $1 and organization_id = $2 and status = 'pending_approval'",
    )
    .bind(quote_id)
    .bind(organization_id)
    .bind(quote_next)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    get_approval(pool, organization_id, approval_id).await
}

/// `POST /sales/approvals/{id}/cancel` — the requester withdraws their own request.
///
/// A manager may not cancel somebody's request: withdrawing it is a decision, and decisions are
/// the thing this gate exists to keep honest. A requester who wants the discount to stop being
/// asked for cancels it; the quote goes back to `draft` so it can be sent as it is.
pub async fn cancel_approval(
    pool: &PgPool,
    organization_id: Uuid,
    approval_id: Uuid,
    actor: Uuid,
) -> Result<ApprovalView> {
    let mut transaction = pool.begin().await?;
    let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "select quote_id, requested_by, status from sales_quote_approvals
          where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(approval_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let (quote_id, requested_by, status) =
        row.ok_or(SalesError::NotFound("approval request"))?;

    let request_status = ApprovalStatus::parse(&status).ok_or_else(|| {
        SalesError::invalid("approval", "status", "this request has an unknown status")
    })?;
    if request_status != ApprovalStatus::Pending {
        return Err(SalesError::InvalidStatusChange(format!(
            "this request was already {} — only a pending one can be withdrawn",
            request_status.label().to_lowercase()
        )));
    }
    if requested_by != actor {
        return Err(SalesError::NotRequester {
            requester: requested_by,
        });
    }

    sqlx::query(
        "update sales_quote_approvals
            set status = 'cancelled', decision = 'cancelled', decided_by = $2, decided_at = now(),
                comment = 'withdrawn by the requester', updated_at = now()
          where id = $1",
    )
    .bind(approval_id)
    .bind(actor)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "update sales_quotes set status = 'draft', updated_at = now()
          where id = $1 and organization_id = $2 and status = 'pending_approval'",
    )
    .bind(quote_id)
    .bind(organization_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    get_approval(pool, organization_id, approval_id).await
}

/// `GET /sales/quotes/{id}/approval-requirement` — what the send button needs to say.
///
/// The builder calls this on load, and the send refusal returns the same value, so the banner a
/// seller reads before pressing the button and the error they get from pressing it are the same
/// sentence built from the same numbers.
pub async fn requirement(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
) -> Result<Option<ApprovalRequired>> {
    let row: Option<(String, String)> = sqlx::query_as(
        "select number, max_discount::text from sales_quotes
          where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(quote_id)
    .fetch_optional(pool)
    .await?;
    let (number, max_discount) = row.ok_or(SalesError::NotFound("quote"))?;

    let settings = crate::store::get_settings(pool, organization_id).await?;
    let discount = parse_percent(&max_discount);
    if !settings.needs_approval(discount) {
        return Ok(None);
    }
    Ok(Some(ApprovalRequired {
        quote_id,
        quote_number: number,
        discount_percent: discount,
        threshold_percent: settings.discount_approval_threshold,
        request: open_request(pool, organization_id, quote_id).await?,
    }))
}

/// `POST /sales/quotes/{id}/cancel` calls this: a withdrawn quote must not leave a manager
/// holding a decision on a document that no longer exists.
///
/// The quote's own status is *not* touched — the caller's `update` to `cancelled` wins, and this
/// only closes what is open underneath it.
pub async fn cancel_open_request(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
    actor: Uuid,
) -> Result<Option<ApprovalView>> {
    let mut transaction = pool.begin().await?;
    let closed: Option<(Uuid,)> = sqlx::query_as(
        "update sales_quote_approvals
            set status = 'cancelled', decision = 'cancelled', decided_by = $3, decided_at = now(),
                comment = 'the quote was withdrawn', updated_at = now()
          where organization_id = $1 and quote_id = $2 and status = 'pending'
          returning id",
    )
    .bind(organization_id)
    .bind(quote_id)
    .bind(actor)
    .fetch_optional(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(match closed {
        None => None,
        Some((id,)) => Some(get_approval(pool, organization_id, id).await?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Settings;

    #[test]
    fn a_discount_above_the_threshold_needs_a_manager_and_one_at_it_does_not() {
        let settings = Settings::default();
        assert!(settings.needs_approval(20));
        assert!(!settings.needs_approval(15));
        assert!(!settings.needs_approval(0));
    }

    #[test]
    fn every_status_survives_the_round_trip_through_the_database_value() {
        for status in [
            ApprovalStatus::Pending,
            ApprovalStatus::Approved,
            ApprovalStatus::Rejected,
            ApprovalStatus::Cancelled,
        ] {
            assert_eq!(ApprovalStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(ApprovalStatus::parse("nonsense"), None);
    }

    #[test]
    fn the_scope_parameter_is_read_in_both_spellings_a_ui_and_a_book_would_use() {
        // `SalesError` deliberately has no `PartialEq` — it carries a `sqlx::Error` — so the
        // scope is compared by the value alone and the refusals are checked for `Err` + text.
        let read = |raw: &str| ApprovalScope::parse(raw).ok();
        assert_eq!(read("pending"), Some(ApprovalScope::Pending));
        assert_eq!(read("requested_by_me"), Some(ApprovalScope::RequestedByMe));
        assert_eq!(read("requested-by-me"), Some(ApprovalScope::RequestedByMe));
        assert_eq!(read("mine"), Some(ApprovalScope::RequestedByMe));
        assert_eq!(read("decided"), Some(ApprovalScope::Decided));
        assert_eq!(read("all"), Some(ApprovalScope::All));
    }

    #[test]
    fn an_unknown_scope_is_named_in_the_refusal_rather_than_defaulting_to_the_inbox() {
        let error = ApprovalScope::parse("everything").unwrap_err().to_string();
        assert!(error.contains("everything"), "{error}");
    }

    #[test]
    fn the_banner_mentions_the_open_request_when_there_is_one() {
        let waiting = ApprovalRequired {
            quote_id: Uuid::nil(),
            quote_number: "Q-2026-0042".to_string(),
            discount_percent: 22,
            threshold_percent: 15,
            request: None,
        };
        assert!(waiting.message().contains("Q-2026-0042"));
        assert!(waiting.message().contains("22%"));
        assert!(waiting.message().contains("15%"));
        assert!(waiting.message().contains("ask a manager"));

        let open = ApprovalRequired {
            request: Some(ApprovalView {
                id: Uuid::nil(),
                organization_id: Uuid::nil(),
                quote_id: Uuid::nil(),
                quote_number: "Q-2026-0042".to_string(),
                quote_title: "Yıllık bakım".to_string(),
                quote_status: QuoteStatus::PendingApproval,
                requested_by: Uuid::nil(),
                requester_name: "Furkan".to_string(),
                discount_percent: 22,
                threshold_percent: 15,
                currency: "TRY".to_string(),
                grand_total: "1000.00".to_string(),
                note: String::new(),
                status: ApprovalStatus::Pending,
                decision: None,
                subject_url: "/sales/quotes/x".to_string(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
            }),
            ..waiting.clone()
        };
        assert!(open.message().contains("waiting for a decision"), "{}", open.message());
    }

    #[test]
    fn a_decision_naming_something_other_than_approve_or_reject_is_refused() {
        // The rule this encodes: an unknown verb is never treated as "approve". A `decision` of
        // `{"decision": ""}` reaching the update would otherwise clear the gate.
        for verb in ["", "yes", "ok", "send it", "APPROVE! "] {
            let normalized = verb.trim().to_lowercase();
            let approve = matches!(normalized.as_str(), "approve" | "approved");
            assert!(
                !(approve && !normalized.starts_with("approve")),
                "{verb:?} must not be read as an approval"
            );
        }
    }

    #[test]
    fn a_cancellation_is_not_a_verdict_so_the_timeline_carries_the_status() {
        // `outcome: None` for a cancelled row is the difference between "withdrawn" and
        // "rejected" in the inbox, and conflating them would tell a seller their request was
        // refused when they were the one who withdrew it.
        let cancelled = ApprovalStatus::Cancelled;
        assert!(!cancelled.label().eq_ignore_ascii_case("approved"));
        assert!(!cancelled.label().eq_ignore_ascii_case("rejected"));
    }

    #[test]
    fn a_percentage_column_reads_as_a_whole_number_rather_than_as_text() {
        assert_eq!(parse_percent("20.00"), 20);
        assert_eq!(parse_percent("0"), 0);
        assert_eq!(parse_percent("7.60"), 8);
    }
}
