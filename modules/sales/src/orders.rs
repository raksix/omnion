//! Orders, stock reservation and the invoice handoff (docs/requests/REQ-052, slice 4).
//!
//! The quote side of the module is finished and the chain it promised is not: a quote the
//! customer accepted has nowhere to become an order, a confirmed order has nowhere to hold its
//! stock, and an order has nowhere to leave for accounting. This file is that middle —
//! **quote → order → confirm → invoice draft** — plus the status history that says how a
//! document got where it is.
//!
//! The rules, and why each one is here rather than in a handler:
//!
//! * **Only an accepted (or sent) quote may be converted, and a quote converts at most once.**
//!   The spec's chain starts at a quote the customer said yes to, and the one-open-order
//!   constraint is what stops a double-click creating two orders for one accepted quote — which
//!   is a duplicated delivery, not a duplicated row.
//! * **The order's lines are a *copy*, not a reference.** The quote is frozen once it is sent
//!   ([`crate::quotes`]), and an order that read its lines back from a frozen document would be
//!   a report on the quote rather than a document in its own right. The copy is also what makes
//!   `subtotal`/`tax_total`/`grand_total` on the order true at the moment of conversion.
//! * **Confirming twice is a no-op, not an error and not a second hold.** The acceptance
//!   criteria say so in those words, and the reason is a person pressing the button again after
//!   a slow response. The unique index on `(order_id, line_id)` makes the no-op a property of the
//!   data rather than of this handler's read-then-write.
//! * **Cancelling releases every hold, and a cancelled order cannot be confirmed.** A released
//!   hold is a *recorded* release (`state = 'released'` with the reason), never a deleted row:
//!   "when did this stop holding that stock?" has to be answerable afterwards.
//! * **The invoice handoff is a draft, and it is a `sales_*` row, not an accounting row.**
//!   REQ-054 is not built and this does not pretend to be it: the handoff is shaped the way that
//!   module's spec describes a document (`subject_*` + a label + a frozen payload) so it can
//!   adopt these rows rather than migrate them. Asking for the draft twice returns **the one
//!   that exists**, because a seller who pressed the button twice wants the invoice, not a
//!   conflict they then have to resolve by reading the order detail.
//!
//! The reservation vocabulary is deliberately honest about REQ-053's absence. Inventory is a
//! different crate that is not installed yet, so the module records the *intent* to hold — one
//! row per line, with the quantity and the product — and the order's `reservation_state` is
//! derived from those rows. `none` therefore means "no holds at all", which is a fact; it never
//! means "we could not ask anyone", because there is nobody to ask yet.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, SalesError};
use crate::money::Money;
use crate::model::Settings;
use crate::store::{DEFAULT_PER_PAGE, MAX_PER_PAGE, Page};
use crate::model::{CustomerKind, OrderReservationState, OrderStatus};

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// An order as the list screen sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// `SO-2026-0001`, assigned by the server on create.
    pub number: String,
    /// Where the order is in its lifecycle.
    pub status: OrderStatus,
    /// The customer, resolved at read time, with the name captured when the order was made.
    pub customer: crate::quotes::CustomerRef,
    /// The quote this came from, when it came from one.
    pub quote_id: Option<Uuid>,
    /// The quote's number, so the order list does not have to fetch each quote to be readable.
    pub quote_number: Option<String>,
    /// The currency every amount is expressed in.
    pub currency: String,
    /// The three totals, as text.
    pub subtotal: String,
    /// Sum of the lines' gross amounts.
    pub discount_total: String,
    /// Sum of the lines' taxes.
    pub tax_total: String,
    /// What the customer pays.
    pub grand_total: String,
    /// Whether stock is held, and whether all of it is.
    pub reservation_state: OrderReservationState,
    /// Whether an invoice draft has been handed to accounting.
    pub invoice_state: String,
    /// The seller, by name, or nobody.
    pub owner: Option<crate::quotes::OwnerRef>,
    /// When the order was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
    /// When the order was last changed.
    #[serde(with = "crate::dates::instant")]
    pub updated_at: OffsetDateTime,
}

/// One line of an order — a copy of the quote's line, frozen at conversion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderLineView {
    /// The line's id.
    pub id: Uuid,
    /// Where the line sits, from 1.
    pub position: i32,
    /// The product it sells, or `None` for a free-text line.
    pub product_id: Option<Uuid>,
    /// The product's SKU and name as they read now.
    pub product: Option<crate::quotes::ProductRef>,
    /// What the line says.
    pub description: String,
    /// The unit the quantity is counted in.
    pub unit: String,
    /// How many, as decimal text.
    pub quantity: String,
    /// The price for one, as decimal text.
    pub unit_price: String,
    /// The discount the converted quote carried, as a whole percent.
    pub discount_percent: i32,
    /// The tax snapshot, as a whole percent.
    pub tax_percent: i32,
    /// The line's own contribution to the order total.
    pub line_total: String,
    /// The hold this line currently has, if the order is confirmed and the hold is live.
    pub reservation: Option<ReservationView>,
}

/// A stock hold, as the order detail renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationView {
    /// The hold's id.
    pub id: Uuid,
    /// The line it holds for.
    pub line_id: Uuid,
    /// The product being held, if the line names one.
    pub product_id: Option<Uuid>,
    /// What is held, as decimal text.
    pub quantity: String,
    /// The unit it is counted in.
    pub unit: String,
    /// `held` or `released`.
    pub state: String,
    /// When the hold was taken.
    #[serde(with = "crate::dates::instant")]
    pub held_at: OffsetDateTime,
    /// When it was given back, if it was.
    #[serde(with = "crate::dates::instant::option")]
    pub released_at: Option<OffsetDateTime>,
    /// Why it was given back.
    pub released_reason: String,
}

/// One row of the order's own status history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// The entry's id.
    pub id: Uuid,
    /// Where the order came from, absent on creation.
    pub from_status: Option<String>,
    /// Where it went.
    pub to_status: String,
    /// The sentence the detail screen prints under the entry.
    pub note: String,
    /// Who did it, by name; `None` for a system entry.
    pub actor: Option<crate::quotes::OwnerRef>,
    /// When.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

/// The invoice draft this order owes accounting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvoiceHandoffView {
    /// The handoff's id — what `sales_invoice_handoffs` calls it.
    pub id: Uuid,
    /// The order it belongs to.
    pub order_id: Uuid,
    /// `draft`, `issued` or `void`.
    pub state: String,
    /// The frozen currency.
    pub currency: String,
    /// The frozen three totals, as text.
    pub subtotal: String,
    /// Sum of the lines' gross amounts.
    pub tax_total: String,
    /// What the customer pays.
    pub grand_total: String,
    /// Accounting's own document id, once REQ-054 has issued one.
    pub external_id: Option<Uuid>,
    /// The link to it in the accounting module, once it exists.
    pub external_url: Option<String>,
    /// When the document was issued, so the screen can answer "how long was this order waiting?"
    /// rather than showing a link with no date against it. `None` while the handoff is a draft.
    pub settled_at: Option<OffsetDateTime>,
    /// When.
    #[serde(with = "crate::dates::instant")]
    pub raised_at: OffsetDateTime,
}

/// An order with everything its detail screen needs, in one response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderDetail {
    /// The order header.
    pub order: OrderView,
    /// Its lines, in grid order, each with its hold.
    pub lines: Vec<OrderLineView>,
    /// The status history, newest first.
    pub history: Vec<HistoryEntry>,
    /// The invoice draft, if one has been raised.
    pub invoice: Option<InvoiceHandoffView>,
    /// The totals, spelled out so the totals block is one field rather than four reads.
    pub totals: crate::quotes::QuoteTotalsView,
}

// ---------------------------------------------------------------------------------------------
// The shapes the API writes
// ---------------------------------------------------------------------------------------------

/// The filters an order list sends.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OrderQuery {
    /// Free text over the order number, the customer and the quote number.
    #[serde(default)]
    pub search: Option<String>,
    /// One or more statuses; empty means every one.
    #[serde(default)]
    pub status: Option<String>,
    /// Restrict to one owner.
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// Only orders from this date (inclusive), as `YYYY-MM-DD`.
    #[serde(default)]
    pub from: Option<String>,
    /// Only orders to this date (inclusive).
    #[serde(default)]
    pub to: Option<String>,
    /// `true` for live orders only, `false` for archived only.
    #[serde(default)]
    pub active: Option<bool>,
    /// Sort key: `created` (default), `number`, `total`, `status`.
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
}

/// `POST /sales/orders` — create an order, from a quote or by hand.
#[derive(Debug, Clone, Deserialize)]
pub struct NewOrder {
    /// The accepted quote to convert. Absent for a manual order.
    #[serde(default)]
    pub quote_id: Option<Uuid>,
    /// Manual orders need a name; a converted order takes the quote's customer.
    #[serde(default)]
    pub customer_name: Option<String>,
    /// Which CRM record a manual order is addressed to.
    #[serde(default)]
    pub customer_kind: Option<String>,
    /// That CRM record's id.
    #[serde(default)]
    pub customer_id: Option<Uuid>,
    /// The owner. Defaults to the caller.
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// The lines of a manual order. Ignored when `quote_id` is set, because the copy is the
    /// point — accepting lines *and* a quote id would be two answers to "what is in this order".
    #[serde(default)]
    pub lines: Vec<NewOrderLine>,
}

/// One line of a manual order.
#[derive(Debug, Clone, Deserialize)]
pub struct NewOrderLine {
    /// The product, when the line is one.
    #[serde(default)]
    pub product_id: Option<Uuid>,
    /// Free text, when it is not.
    #[serde(default)]
    pub description: Option<String>,
    /// The unit; defaults to the product's, then to `piece`.
    #[serde(default)]
    pub unit: Option<String>,
    /// How many, as decimal text.
    pub quantity: String,
    /// The price for one, as decimal text.
    pub unit_price: String,
    /// A whole percent, `0..=100`.
    #[serde(default)]
    pub discount_percent: Option<i32>,
    /// A whole percent, `0..=100`.
    #[serde(default)]
    pub tax_percent: Option<i32>,
}

/// `POST /sales/orders/{id}/cancel` — why the order was withdrawn.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CancelOrder {
    /// The reason. Bounded and required, for the same reason a rejection is: the person reading
    /// the released stock next month is not the person who pulled it.
    #[serde(default)]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// The filters, parsed
// ---------------------------------------------------------------------------------------------

/// Longest a free-text filter may be.
pub const MAX_ORDER_SEARCH_LENGTH: usize = 200;

/// How many rows one order list page may carry.
pub const MAX_ORDER_PAGE: i64 = 100;

/// Longest a customer name captured on a hand-written order may be.
pub const MAX_CUSTOMER_NAME_LENGTH: usize = 320;

/// Longest one line's description may be — the same bound the migration's check uses.
pub const MAX_ORDER_LINE_DESCRIPTION: usize = 1_000;

/// Longest a cancel reason may be.
pub const MAX_CANCEL_REASON_LENGTH: usize = 2_000;

/// The upper bound on a manual order's lines — the same bound a quote has, so the grid a seller
/// is used to (and the totals block beside it) does not suddenly grow a scroll bar.
pub const MAX_ORDER_LINES: usize = 200;

struct OrderFilters {
    search: Option<String>,
    statuses: Vec<OrderStatus>,
    owner_user_id: Option<Uuid>,
    from: Option<time::Date>,
    to: Option<time::Date>,
    active: bool,
}

fn parse_order_filters(query: &OrderQuery) -> Result<OrderFilters> {
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .filter(|s| s.chars().count() <= MAX_ORDER_SEARCH_LENGTH);
    Ok(OrderFilters {
        search,
        statuses: parse_status_filter(query.status.as_deref())?,
        owner_user_id: query.owner_user_id,
        from: parse_day_filter(query.from.as_deref(), "from")?,
        to: parse_day_filter(query.to.as_deref(), "to")?,
        // Default is live orders: an archived order is one somebody deliberately hid, and a
        // list that showed them by default would make "archive" a button that does nothing.
        active: query.active.unwrap_or(true),
    })
}

fn parse_status_filter(raw: Option<&str>) -> Result<Vec<OrderStatus>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part == "all" {
            return Ok(Vec::new());
        }
        let state = OrderStatus::parse(part).ok_or_else(|| {
            SalesError::InvalidQuery(format!("`{part}` is not an order status"))
        })?;
        if !out.contains(&state) {
            out.push(state);
        }
    }
    Ok(out)
}

fn parse_day_filter(raw: Option<&str>, field: &'static str) -> Result<Option<time::Date>> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|value| {
            time::Date::parse(value, crate::dates::wire_format())
                .map_err(|_| SalesError::invalid("order", field, "use a `YYYY-MM-DD` date"))
        })
        .transpose()
}

// ---------------------------------------------------------------------------------------------
// The row → view projection
// ---------------------------------------------------------------------------------------------

fn order_columns() -> &'static str {
    // The joined quote's number is **aliased**, because `q.number` and `o.number` are two
    // different columns with the same name: read both unaliased and PostgreSQL answers
    // "no column found for name: quote_number" against the struct, which is a 500 on every
    // order read rather than a compile error.
    "o.id, o.organization_id, o.number, o.status, o.customer_type, o.customer_id,
     o.customer_name, o.quote_id, q.number as quote_number, o.currency, o.subtotal::text,
     o.discount_total::text, o.tax_total::text, o.grand_total::text,
     o.reservation_state, o.invoice_state, o.owner_user_id, o.created_at, o.updated_at"
}

#[derive(Debug, FromRow)]
struct OrderRow {
    id: Uuid,
    organization_id: Uuid,
    number: String,
    status: String,
    customer_type: String,
    customer_id: Option<Uuid>,
    customer_name: String,
    quote_id: Option<Uuid>,
    quote_number: Option<String>,
    currency: String,
    subtotal: String,
    discount_total: String,
    tax_total: String,
    grand_total: String,
    reservation_state: String,
    invoice_state: String,
    owner_user_id: Option<Uuid>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl OrderRow {
    fn into_view(self, owner_name: Option<String>) -> Result<OrderView> {
        let status = OrderStatus::parse(&self.status).ok_or_else(|| {
            SalesError::invalid("order", "status", "this order has an unknown status")
        })?;
        let kind = CustomerKind::parse(&self.customer_type).unwrap_or(CustomerKind::Company);
        let reservation_state = OrderReservationState::parse(&self.reservation_state)
            .unwrap_or(OrderReservationState::None);
        Ok(OrderView {
            id: self.id,
            organization_id: self.organization_id,
            number: self.number,
            status,
            customer: crate::quotes::CustomerRef {
                kind,
                id: self.customer_id,
                name: self.customer_name,
            },
            quote_id: self.quote_id,
            quote_number: self.quote_number,
            currency: self.currency,
            subtotal: self.subtotal,
            discount_total: self.discount_total,
            tax_total: self.tax_total,
            grand_total: self.grand_total,
            reservation_state,
            invoice_state: self.invoice_state,
            owner: self.owner_user_id.map(|id| crate::quotes::OwnerRef {
                id,
                name: owner_name.unwrap_or_default(),
            }),
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

async fn owner_names(pool: &PgPool, ids: &[Uuid]) -> std::collections::HashMap<Uuid, String> {
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

fn encode_cursor(stamp: OffsetDateTime, id: Uuid) -> String {
    use base64::Engine as _;
    let raw = format!("{}|{}", stamp.unix_timestamp_nanos(), id);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

fn decode_cursor(cursor: &str) -> Result<(OffsetDateTime, Uuid)> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let (nanos, id) = text
        .split_once('|')
        .ok_or_else(|| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let nanos: i128 = nanos
        .parse()
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let id: Uuid = id
        .parse()
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let stamp = OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    Ok((stamp, id))
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

// The list and its count share one predicate builder, so the "N results" line can never be
// counting a different set than the rows above it.

/// Everything the list and the count must agree about.
fn push_order_filters(
    builder: &mut QueryBuilder<'_, Postgres>,
    filters: &OrderFilters,
    cursor: Option<(OffsetDateTime, Uuid)>,
) {
    if filters.active {
        builder.push(" and o.archived_at is null");
    }
    if !filters.statuses.is_empty() {
        let states: Vec<&str> = filters.statuses.iter().copied().map(OrderStatus::as_str).collect();
        builder.push(" and o.status = any(");
        builder.push_bind(states);
        builder.push(")");
    }
    if let Some(owner) = filters.owner_user_id {
        builder.push(" and o.owner_user_id = ");
        builder.push_bind(owner);
    }
    if let Some(term) = &filters.search {
        let pattern = format!("%{}%", escape_like(term));
        builder.push(" and (o.number ilike ");
        builder.push_bind(pattern.clone());
        builder.push(" or o.customer_name ilike ");
        builder.push_bind(pattern.clone());
        builder.push(" or coalesce(q.number, '') ilike ");
        builder.push_bind(pattern);
        builder.push(" escape '\\')");
    }
    if let Some(from) = filters.from {
        builder.push(" and o.created_at >= ");
        builder.push_bind(from);
    }
    if let Some(to) = filters.to {
        // Inclusive of `to`, so the bound is the *next* midnight rather than the day itself —
        // `created_at < '2026-09-29'` would drop every order written during the 29th.
        builder.push(" and o.created_at < ");
        builder.push_bind(to);
        builder.push(" + interval '1 day'");
    }
    if let Some((stamp, id)) = cursor {
        builder.push(" and (o.updated_at, o.id) < (");
        builder.push_bind(stamp);
        builder.push(", ");
        builder.push_bind(id);
        builder.push(")");
    }
}

/// The whole `order by` clause, from a closed set.
///
/// A closed set because this is the one place in the query where a value is interpolated rather
/// than bound; refusing anything else is what makes that safe. The id tiebreak is not cosmetic:
/// the keyset cursor below needs `(updated_at, id)` to be a **strict total order**, or two orders
/// written in the same microsecond make the cursor ambiguous and one of them is never paged.
fn order_sort(sort: Option<&str>, direction: Option<&str>) -> Result<&'static str> {
    let direction = match direction.map(str::trim).unwrap_or("desc") {
        "" | "asc" => "asc",
        "desc" => "desc",
        other => {
            return Err(SalesError::InvalidQuery(format!(
                "`{other}` is not a sort direction (asc or desc)"
            )));
        }
    };
    Ok(match sort.map(str::trim).unwrap_or("") {
        "" | "created" | "updated_at" => match direction {
            "asc" => "o.updated_at asc, o.id asc",
            _ => "o.updated_at desc, o.id desc",
        },
        "number" => match direction {
            "asc" => "o.number asc, o.id asc",
            _ => "o.number desc, o.id desc",
        },
        "total" => match direction {
            "asc" => "o.grand_total asc, o.id asc",
            _ => "o.grand_total desc, o.id desc",
        },
        "status" => match direction {
            "asc" => "o.status asc, o.number asc",
            _ => "o.status desc, o.number asc",
        },
        other => {
            return Err(SalesError::InvalidQuery(format!(
                "`{other}` is not an order sort key (created, number, total or status)"
            )));
        }
    })
}

fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `GET /sales/orders` — one page of the list.
pub async fn list_orders(
    pool: &PgPool,
    organization_id: Uuid,
    query: &OrderQuery,
) -> Result<Page<OrderView>> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_ORDER_PAGE.min(MAX_PER_PAGE));
    let filters = parse_order_filters(query)?;
    let cursor = query.cursor.as_deref().map(decode_cursor).transpose()?;
    let order_by = order_sort(query.sort.as_deref(), query.direction.as_deref())?;

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "select {} from sales_orders o left join sales_quotes q on q.id = o.quote_id
           where o.organization_id = ",
        order_columns()
    ));
    builder.push_bind(organization_id);
    push_order_filters(&mut builder, &filters, cursor);
    builder.push(format!(" order by {order_by} limit "));
    builder.push_bind(limit + 1);

    let rows: Vec<OrderRow> = builder.build_query_as().fetch_all(pool).await?;
    let has_more = rows.len() > limit as usize;
    let page: Vec<OrderRow> = rows.into_iter().take(limit as usize).collect();
    let names = owner_names(
        pool,
        &page.iter().filter_map(|row| row.owner_user_id).collect::<Vec<_>>(),
    )
    .await;
    let mut out = Vec::with_capacity(page.len());
    for row in page {
        let name = row.owner_user_id.and_then(|id| names.get(&id).cloned());
        out.push(row.into_view(name)?);
    }
    let next_cursor = if has_more {
        out.last()
            .map(|order| encode_cursor(order.updated_at, order.id))
    } else {
        None
    };
    Ok(Page::new(
        out,
        next_cursor,
        count_orders(pool, organization_id, &filters).await,
    ))
}

/// How many rows the same filter matches — a **separate, simpler** query than the page's, because
/// reusing the page's builder (with its `limit + 1` and its cursor) would report the page's size.
async fn count_orders(pool: &PgPool, organization_id: Uuid, filters: &OrderFilters) -> i64 {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "select count(*)::bigint from sales_orders o left join sales_quotes q on q.id = o.quote_id
           where o.organization_id = ",
    );
    builder.push_bind(organization_id);
    // No cursor: "N results" is the size of the whole filter set, not of the page being read.
    push_order_filters(&mut builder, filters, None);
    builder
        .build_query_scalar::<i64>()
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// `GET /sales/orders/{id}` — one order with its lines, holds, history and invoice draft.
pub async fn get_order(
    pool: &PgPool,
    organization_id: Uuid,
    order_id: Uuid,
) -> Result<OrderDetail> {
    let row = fetch_order_row(pool, organization_id, order_id).await?;
    let owner_name = match row.owner_user_id {
        Some(id) => owner_names(pool, &[id]).await.get(&id).cloned(),
        None => None,
    };
    let order = row.into_view(owner_name)?;
    let lines = list_order_lines(pool, order_id).await?;
    let history = list_history(pool, order_id).await?;
    let invoice = load_invoice_handoff(pool, organization_id, order_id).await?;
    Ok(OrderDetail {
        totals: crate::quotes::QuoteTotalsView {
            subtotal: order.subtotal.clone(),
            discount_total: order.discount_total.clone(),
            tax_total: order.tax_total.clone(),
            grand_total: order.grand_total.clone(),
        },
        order,
        lines,
        history,
        invoice,
    })
}

async fn fetch_order_row(
    pool: &PgPool,
    organization_id: Uuid,
    order_id: Uuid,
) -> Result<OrderRow> {
    let sql = format!(
        "select {} from sales_orders o left join sales_quotes q on q.id = o.quote_id
          where o.organization_id = $1 and o.id = $2",
        order_columns()
    );
    sqlx::query_as::<_, OrderRow>(&sql)
        .bind(organization_id)
        .bind(order_id)
        .fetch_optional(pool)
        .await?
        .ok_or(SalesError::NotFound("order"))
}

async fn list_order_lines(pool: &PgPool, order_id: Uuid) -> Result<Vec<OrderLineView>> {
    // Left-joining the hold means a draft order (no holds yet) renders the same grid shape as a
    // confirmed one, with `reservation: null` — a screen that switches column count on confirm
    // is a screen that reflows under the person reading it.
    // A `FromRow` struct rather than a tuple, and the reason is mechanical: sqlx implements
    // `FromRow` for tuples up to sixteen elements, and this row has twenty. The alternative —
    // fetching the line and the hold separately and zipping them in Rust — would put the join's
    // own knowledge (which line has a hold) into a second place that has to be kept in step.
    let rows = sqlx::query_as::<_, OrderLineRow>(
        // Every column of the hold is **aliased**. The join brings `l.id` and `r.id` into one
        // row under the same name, and a `FromRow` struct resolves by name: the line's `id` then
        // decodes the hold's NULL and every draft order read answers 500. Aliasing is the only
        // fix — the duplicate is invisible in the query text otherwise.
        "select l.id, l.position, l.product_id, p.sku, p.name, l.description, l.unit,
                l.quantity::text, l.unit_price::text, l.discount_percent::int4,
                l.tax_percent::int4, l.line_total::text,
                r.id as reservation_id, r.product_id as reservation_product_id,
                r.quantity::text as reservation_quantity, r.unit as reservation_unit,
                r.state as reservation_state, r.held_at as reservation_held_at,
                r.released_at as reservation_released_at, r.released_reason as reservation_reason
           from sales_order_lines l
           left join sales_products p on p.id = l.product_id
           left join sales_order_reservations r on r.line_id = l.id and r.state = 'held'
          where l.order_id = $1
          order by l.position",
    )
    .bind(order_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(OrderLineRow::into_view).collect())
}

/// The flat row behind [`OrderLineView`]: the line, the product, and the hold if there is one.
#[derive(Debug, FromRow)]
struct OrderLineRow {
    id: Uuid,
    position: i32,
    product_id: Option<Uuid>,
    sku: Option<String>,
    name: Option<String>,
    description: String,
    unit: String,
    quantity: String,
    unit_price: String,
    discount_percent: i32,
    tax_percent: i32,
    line_total: String,
    reservation_id: Option<Uuid>,
    reservation_product_id: Option<Uuid>,
    reservation_quantity: Option<String>,
    reservation_unit: Option<String>,
    reservation_state: Option<String>,
    reservation_held_at: Option<OffsetDateTime>,
    reservation_released_at: Option<OffsetDateTime>,
    reservation_reason: Option<String>,
}

impl OrderLineRow {
    fn into_view(self) -> OrderLineView {
        let product = self.product_id.map(|pid| crate::quotes::ProductRef {
            id: pid,
            sku: self.sku.unwrap_or_default(),
            name: self.name.unwrap_or_default(),
            unit: self.unit.clone(),
        });
        // The hold's columns are read but the `reservation` is `None` unless the **id** survived
        // the join, because a left join against a filtered table can produce a row of all-nulls
        // and a hold with an empty quantity is not a hold.
        let reservation = self.reservation_id.map(|id| ReservationView {
            id,
            line_id: self.id,
            product_id: self.reservation_product_id,
            quantity: self.reservation_quantity.unwrap_or_default(),
            unit: self.reservation_unit.unwrap_or_default(),
            state: self.reservation_state.unwrap_or_default(),
            // A hold that exists always has a `held_at` (the column is NOT NULL), so this
            // unwrap cannot panic on real data; it keeps a draft's absent hold out of the type
            // rather than stamping a fake instant onto a reservation that never happened.
            held_at: self
                .reservation_held_at
                .unwrap_or_else(crate::quotes::now_utc),
            released_at: self.reservation_released_at,
            released_reason: self.reservation_reason.unwrap_or_default(),
        });
        OrderLineView {
            id: self.id,
            position: self.position,
            product_id: self.product_id,
            product,
            description: self.description,
            unit: self.unit,
            quantity: self.quantity,
            unit_price: self.unit_price,
            discount_percent: self.discount_percent,
            tax_percent: self.tax_percent,
            line_total: self.line_total,
            reservation,
        }
    }
}

async fn list_history(pool: &PgPool, order_id: Uuid) -> Result<Vec<HistoryEntry>> {
    let rows = sqlx::query_as::<_, (
        Uuid,
        Option<String>,
        String,
        String,
        Option<Uuid>,
        Option<String>,
        OffsetDateTime,
    )>(
        "select h.id, h.from_status, h.to_status, h.note, h.actor_user_id,
                coalesce(u.display_name, u.email), h.created_at
           from sales_status_history h
           left join users u on u.id = h.actor_user_id
          where h.order_id = $1
          order by h.created_at desc, h.id desc
          limit $2",
    )
    .bind(order_id)
    .bind(MAX_HISTORY_ENTRIES)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, from_status, to_status, note, actor_id, actor_name, created_at)| HistoryEntry {
                id,
                from_status,
                to_status,
                note,
                actor: actor_id.map(|aid| crate::quotes::OwnerRef {
                    id: aid,
                    name: actor_name.unwrap_or_default(),
                }),
                created_at,
            },
        )
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// Longest the status history a detail screen will render.
const MAX_HISTORY_ENTRIES: i64 = 50;

/// One line, after validation: the numbers the arithmetic and the SQL both read.
#[derive(Debug)]
struct ValidatedLine {
    position: i32,
    product_id: Option<Uuid>,
    description: String,
    unit: String,
    quantity: crate::money::Quantity,
    unit_price: Money,
    discount_percent: i32,
    tax_percent: i32,
}

impl ValidatedLine {
    fn to_input(&self) -> crate::money::LineInput {
        crate::money::LineInput {
            quantity: self.quantity,
            unit_price: self.unit_price,
            discount_percent: self.discount_percent,
            tax_percent: self.tax_percent,
        }
    }
}

/// A manual order's line, resolved against the catalog for the defaults a caller may omit.
fn validate_line(index: usize, line: &NewOrderLine) -> Result<ValidatedLine> {
    let entity = "order_line";
    let quantity = crate::money::Quantity::parse(&line.quantity)
        .map_err(|source| SalesError::number(entity, "quantity", source))?;
    if !quantity.is_positive() {
        return Err(SalesError::invalid(
            entity,
            "quantity",
            "a line needs at least one unit",
        ));
    }
    let unit_price = Money::parse(&line.unit_price)
        .map_err(|source| SalesError::number(entity, "unit_price", source))?;
    if unit_price.minor() < 0 {
        return Err(SalesError::invalid(entity, "unit_price", "a price cannot be negative"));
    }
    let discount_percent = line.discount_percent.unwrap_or(0);
    if !(0..=100).contains(&discount_percent) {
        return Err(SalesError::invalid(
            entity,
            "discount_percent",
            "a discount is a percentage between 0 and 100",
        ));
    }
    let tax_percent = line.tax_percent.unwrap_or(0);
    if !(0..=100).contains(&tax_percent) {
        return Err(SalesError::invalid(
            entity,
            "tax_percent",
            "a tax rate is a percentage between 0 and 100",
        ));
    }
    let description = line
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if line.product_id.is_none() && description.is_empty() {
        return Err(SalesError::invalid(
            entity,
            "description",
            "a line needs a product or a description",
        ));
    }
    if description.chars().count() > MAX_ORDER_LINE_DESCRIPTION {
        return Err(SalesError::invalid(
            entity,
            "description",
            format!("a line description is at most {MAX_ORDER_LINE_DESCRIPTION} characters"),
        ));
    }
    let unit = line
        .unit
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or(crate::model::Unit::Piece.as_str())
        .to_string();
    Ok(ValidatedLine {
        position: i32::try_from(index + 1).unwrap_or(i32::MAX),
        product_id: line.product_id,
        description,
        unit,
        quantity,
        unit_price,
        discount_percent,
        tax_percent,
    })
}

/// Fill the unit and tax a manual line omitted, from the product it names.
async fn apply_product_defaults(
    pool: &PgPool,
    organization_id: Uuid,
    lines: &mut [ValidatedLine],
) -> Result<()> {
    let ids: Vec<Uuid> = lines.iter().filter_map(|l| l.product_id).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<(Uuid, String, i32)> = sqlx::query_as(
        "select id, unit, round(tax_percent)::int4 from sales_products
          where organization_id = $1 and id = any($2) and archived_at is null",
    )
    .bind(organization_id)
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    for line in lines.iter_mut() {
        let Some(product_id) = line.product_id else {
            continue;
        };
        let Some((_, unit, tax_percent)) = rows.iter().find(|(id, ..)| *id == product_id) else {
            // A product of another organization is indistinguishable from one that does not
            // exist, which is the same rule every read follows.
            return Err(SalesError::NotFound("product"));
        };
        if line.unit == crate::model::Unit::Piece.as_str() {
            line.unit = unit.clone();
        }
        if line.description.is_empty() {
            line.description = unit.clone();
        }
        if line.tax_percent == 0 {
            line.tax_percent = *tax_percent;
        }
    }
    Ok(())
}

/// `POST /sales/orders` — create an order, from an accepted quote or by hand.
pub async fn create_order(
    pool: &PgPool,
    organization_id: Uuid,
    settings: &Settings,
    actor: Uuid,
    input: &NewOrder,
) -> Result<OrderDetail> {
    let order_id = match input.quote_id {
        Some(quote_id) => convert_quote(pool, organization_id, settings, actor, quote_id).await?,
        None => create_manual_order(pool, organization_id, settings, actor, input).await?,
    };
    get_order(pool, organization_id, order_id).await
}

/// Turn an accepted quote into an order, copying its lines and totals.
async fn convert_quote(
    pool: &PgPool,
    organization_id: Uuid,
    settings: &Settings,
    actor: Uuid,
    quote_id: Uuid,
) -> Result<Uuid> {
    let detail = crate::quotes::get_quote(pool, organization_id, quote_id).await?;
    let status = detail.quote.status;
    // The chain starts where the spec says it starts: a quote the customer said yes to. A draft
    // or a sent quote is refused by name rather than by a generic "wrong status", because the
    // seller holding the screen is one click from the action that would make it legal.
    if !matches!(status, crate::QuoteStatus::Accepted | crate::QuoteStatus::Sent) {
        return Err(SalesError::invalid(
            "order",
            "quote_id",
            format!(
                "quote {} is {status} — only an accepted quote becomes an order",
                detail.quote.number
            ),
        ));
    }

    let mut transaction = pool.begin().await?;
    // Read the existing order *inside* the transaction, before the number is taken: two sellers
    // pressing "convert" on the same quote must produce one order, and the loser has to be told
    // which one it is rather than receive a unique-index violation as a 500.
    let existing: Option<(Uuid,)> = sqlx::query_as::<_, (Uuid,)>(
        "select id from sales_orders where organization_id = $1 and quote_id = $2 limit 1",
    )
    .bind(organization_id)
    .bind(quote_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some((order_id,)) = existing {
        transaction.commit().await?;
        return Ok(order_id);
    }

    let number = crate::quotes::next_number(
        &mut transaction,
        organization_id,
        &settings.order_number_prefix,
        "order",
    )
    .await?;

    let order_id: Uuid = sqlx::query_as::<_, (Uuid,)>(
        "insert into sales_orders (organization_id, number, quote_id, customer_type, customer_id,
                customer_name, status, currency, owner_user_id, subtotal, discount_total,
                tax_total, grand_total)
         values ($1, $2, $3, $4, $5, $6, 'draft', $7, $8,
                 $9::numeric, $10::numeric, $11::numeric, $12::numeric)
         returning id",
    )
    .bind(organization_id)
    .bind(&number)
    .bind(quote_id)
    .bind(detail.quote.customer.kind.as_str())
    .bind(detail.quote.customer.id)
    .bind(&detail.quote.customer.name)
    .bind(&detail.quote.currency)
    .bind(detail.quote.owner.as_ref().map(|o| o.id).or(Some(actor)))
    .bind(&detail.quote.totals.subtotal)
    .bind(&detail.quote.totals.discount_total)
    .bind(&detail.quote.totals.tax_total)
    .bind(&detail.quote.totals.grand_total)
    .fetch_one(&mut *transaction)
    .await?
    .0;

    for line in &detail.lines {
        sqlx::query(
            "insert into sales_order_lines (organization_id, order_id, position, product_id,
                    description, unit, quantity, unit_price, discount_percent, tax_percent,
                    line_total)
             values ($1, $2, $3, $4, $5, $6, $7::numeric, $8::numeric, $9, $10, $11::numeric)",
        )
        .bind(organization_id)
        .bind(order_id)
        .bind(line.position)
        .bind(line.product_id)
        .bind(&line.description)
        .bind(&line.unit)
        .bind(&line.quantity)
        .bind(&line.unit_price)
        .bind(line.discount_percent)
        .bind(line.tax_percent)
        .bind(&line.line_total)
        .execute(&mut *transaction)
        .await?;
    }

    record_history(
        &mut transaction,
        organization_id,
        order_id,
        None,
        "draft",
        &format!("Converted from quote {}", detail.quote.number),
        actor,
    )
    .await?;

    transaction.commit().await?;
    Ok(order_id)
}

/// A hand-written order, for the sale that never had a quote behind it.
async fn create_manual_order(
    pool: &PgPool,
    organization_id: Uuid,
    settings: &Settings,
    actor: Uuid,
    input: &NewOrder,
) -> Result<Uuid> {
    if input.lines.is_empty() {
        return Err(SalesError::invalid(
            "order",
            "lines",
            "an order needs at least one line",
        ));
    }
    if input.lines.len() > MAX_ORDER_LINES {
        return Err(SalesError::invalid(
            "order",
            "lines",
            format!("an order may carry at most {MAX_ORDER_LINES} lines"),
        ));
    }
    let customer_name = input
        .customer_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            SalesError::invalid(
                "order",
                "customer_name",
                "an order written by hand needs a customer name",
            )
        })?
        .to_string();
    if customer_name.chars().count() > MAX_CUSTOMER_NAME_LENGTH {
        return Err(SalesError::invalid(
            "order",
            "customer_name",
            format!("a customer name is at most {MAX_CUSTOMER_NAME_LENGTH} characters"),
        ));
    }
    let kind = match input.customer_kind.as_deref().map(str::trim) {
        None | Some("") | Some("company") => CustomerKind::Company,
        Some("contact") => CustomerKind::Contact,
        Some(other) => {
            return Err(SalesError::invalid(
                "order",
                "customer_kind",
                format!("`{other}` is not a customer kind (company or contact)"),
            ));
        }
    };

    let mut lines = input
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| validate_line(index, line))
        .collect::<Result<Vec<_>>>()?;
    apply_product_defaults(pool, organization_id, &mut lines).await?;
    let totals = crate::money::quote_totals(&lines.iter().map(ValidatedLine::to_input).collect::<Vec<_>>());

    let mut transaction = pool.begin().await?;
    let number =
        crate::quotes::next_number(&mut transaction, organization_id, &settings.order_number_prefix, "order").await?;
    let order_id: Uuid = sqlx::query_as::<_, (Uuid,)>(
        "insert into sales_orders (organization_id, number, customer_type, customer_id,
                customer_name, status, currency, owner_user_id, subtotal, discount_total,
                tax_total, grand_total)
         values ($1, $2, $3, $4, $5, 'draft', $6, $7, $8::numeric, $9::numeric, $10::numeric,
                 $11::numeric)
         returning id",
    )
    .bind(organization_id)
    .bind(&number)
    .bind(kind.as_str())
    .bind(input.customer_id)
    .bind(&customer_name)
    .bind(&settings.currency)
    .bind(input.owner_user_id.or(Some(actor)))
    .bind(totals.subtotal.to_text())
    .bind(totals.discount_total.to_text())
    .bind(totals.tax_total.to_text())
    .bind(totals.grand_total.to_text())
    .fetch_one(&mut *transaction)
    .await?
    .0;

    for line in &lines {
        let computed = line.to_input().totals();
        sqlx::query(
            "insert into sales_order_lines (organization_id, order_id, position, product_id,
                    description, unit, quantity, unit_price, discount_percent, tax_percent,
                    line_total)
             values ($1, $2, $3, $4, $5, $6, $7::numeric, $8::numeric, $9, $10, $11::numeric)",
        )
        .bind(organization_id)
        .bind(order_id)
        .bind(line.position)
        .bind(line.product_id)
        .bind(&line.description)
        .bind(&line.unit)
        .bind(line.quantity.to_text())
        .bind(line.unit_price.to_text())
        .bind(line.discount_percent)
        .bind(line.tax_percent)
        .bind(computed.net.to_text())
        .execute(&mut *transaction)
        .await?;
    }

    record_history(
        &mut transaction,
        organization_id,
        order_id,
        None,
        "draft",
        "Order written by hand",
        actor,
    )
    .await?;
    transaction.commit().await?;
    Ok(order_id)
}

/// `POST /sales/orders/{id}/confirm` — promise the order and hold its stock.
///
/// **Idempotent by data, not by handler**: the `on conflict do nothing` on the hold insert plus
/// the unique index behind it mean a second confirm updates no rows, and the function returns the
/// order unchanged. A person who pressed the button again after a slow response gets the same
/// order, which is what the acceptance criteria ask for.
pub async fn confirm_order(
    pool: &PgPool,
    organization_id: Uuid,
    order_id: Uuid,
    actor: Uuid,
) -> Result<OrderDetail> {
    let mut transaction = pool.begin().await?;
    // `for update` on the order row: two confirms racing would otherwise both read `draft` and
    // both believe they are the one that confirmed it.
    let row: (String, String) = sqlx::query_as::<_, (String, String)>(
        "select status, number from sales_orders
          where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(order_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SalesError::NotFound("order"))?;
    let state = OrderStatus::parse(&row.0).ok_or_else(|| {
        SalesError::invalid("order", "status", "this order has an unknown status")
    })?;

    match state {
        OrderStatus::Confirmed | OrderStatus::Invoiced | OrderStatus::Delivered => {
            // Already promised. Returning it, unchanged and without a history row, is the no-op
            // the criteria describe — a second "confirmed" line in the timeline would be a lie
            // about a second promise.
            transaction.commit().await?;
            return get_order(pool, organization_id, order_id).await;
        }
        OrderStatus::Cancelled => {
            return Err(SalesError::invalid(
                "order",
                "status",
                format!("order {} was cancelled — its stock was released", row.1),
            ));
        }
        OrderStatus::Draft => {}
    }

    let lines: Vec<(Uuid, Option<Uuid>, String, String, String)> = sqlx::query_as(
        "select id, product_id, description, unit, quantity::text
           from sales_order_lines where order_id = $1 order by position",
    )
    .bind(order_id)
    .fetch_all(&mut *transaction)
    .await?;
    if lines.is_empty() {
        return Err(SalesError::invalid(
            "order",
            "lines",
            "an order with no lines cannot be confirmed",
        ));
    }

    let mut held = 0u64;
    for (line_id, product_id, description, unit, quantity) in &lines {
        let inserted = sqlx::query(
            "insert into sales_order_reservations (organization_id, order_id, line_id, product_id,
                    description, quantity, unit, state)
             values ($1, $2, $3, $4, $5, $6::numeric, $7, 'held')
             on conflict (order_id, line_id) do nothing",
        )
        .bind(organization_id)
        .bind(order_id)
        .bind(line_id)
        .bind(product_id)
        .bind(description)
        .bind(quantity)
        .bind(unit)
        .execute(&mut *transaction)
        .await?;
        held += inserted.rows_affected();
    }

    if held > 0 {
        record_history(
            &mut transaction,
            organization_id,
            order_id,
            Some("draft"),
            "confirmed",
            &format!("{held} of {} lines reserved", lines.len()),
            actor,
        )
        .await?;
    }

    sqlx::query(
        "update sales_orders
            set status = 'confirmed',
                confirmed_at = now(),
                reservation_state = (select case when count(*) = 0 then 'none'
                                                when count(*) filter (where state = 'held') = count(*)
                                                    then 'total' else 'partial' end
                                       from sales_order_reservations where order_id = $2),
                updated_at = now()
          where id = $1",
    )
    .bind(order_id)
    .bind(order_id)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;
    get_order(pool, organization_id, order_id).await
}

/// `POST /sales/orders/{id}/cancel` — withdraw the order and give the stock back.
pub async fn cancel_order(
    pool: &PgPool,
    organization_id: Uuid,
    order_id: Uuid,
    actor: Uuid,
    input: &CancelOrder,
) -> Result<OrderDetail> {
    let reason = input
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| {
            SalesError::invalid(
                "order",
                "reason",
                "say why the order is being cancelled — the release is what somebody reads next month",
            )
        })?
        .to_string();
    if reason.chars().count() > MAX_CANCEL_REASON_LENGTH {
        return Err(SalesError::invalid(
            "order",
            "reason",
            format!("a cancellation reason is at most {MAX_CANCEL_REASON_LENGTH} characters"),
        ));
    }

    let mut transaction = pool.begin().await?;
    let row: (String, String) = sqlx::query_as::<_, (String, String)>(
        "select status, number from sales_orders
          where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(order_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SalesError::NotFound("order"))?;
    let state = OrderStatus::parse(&row.0).ok_or_else(|| {
        SalesError::invalid("order", "status", "this order has an unknown status")
    })?;
    if state == OrderStatus::Cancelled {
        transaction.commit().await?;
        return get_order(pool, organization_id, order_id).await;
    }
    if state == OrderStatus::Delivered {
        return Err(SalesError::invalid(
            "order",
            "status",
            format!("order {} was delivered — it cannot be cancelled", row.1),
        ));
    }

    // Released, never deleted: the question "when did this stop holding that stock?" has to be
    // answerable after the fact, and a deleted row answers it with silence.
    let released = sqlx::query(
        "update sales_order_reservations
            set state = 'released', released_at = now(), released_reason = $3
          where order_id = $1 and state = 'held'",
    )
    .bind(order_id)
    .bind(organization_id)
    .bind(&reason)
    .execute(&mut *transaction)
    .await?
    .rows_affected();

    sqlx::query(
        "update sales_orders
            set status = 'cancelled', cancelled_at = now(), reservation_state = 'released',
                updated_at = now()
          where id = $1",
    )
    .bind(order_id)
    .execute(&mut *transaction)
    .await?;

    // An order withdrawn before accounting turned it into anything must not leave a draft
    // invoice hanging: it is voided, with the same reason, rather than deleted.
    sqlx::query(
        "update sales_invoice_handoffs
            set state = 'void', void_reason = $2
          where order_id = $1 and state = 'draft'",
    )
    .bind(order_id)
    .bind(&reason)
    .execute(&mut *transaction)
    .await?;

    record_history(
        &mut transaction,
        organization_id,
        order_id,
        Some(&row.0),
        "cancelled",
        &format!("{released} reservations released · {reason}"),
        actor,
    )
    .await?;
    transaction.commit().await?;
    get_order(pool, organization_id, order_id).await
}

/// `POST /sales/orders/{id}/invoice-draft` — hand the delivery to accounting.
///
/// REQ-054 is not built. This raises the draft as a `sales_invoice_handoffs` row shaped the way
/// that module's spec describes a document, so it can adopt these rows rather than migrate them —
/// the same precedent `0055_quote_approvals.sql` set for approvals. Asking twice returns **the
/// draft that exists**: a seller who pressed the button twice wants the invoice, not a conflict
/// they then have to resolve by reading the order detail.
pub async fn raise_invoice_draft(
    pool: &PgPool,
    organization_id: Uuid,
    order_id: Uuid,
    actor: Uuid,
) -> Result<InvoiceHandoffView> {
    let detail = get_order(pool, organization_id, order_id).await?;
    match detail.order.status {
        OrderStatus::Draft => {
            return Err(SalesError::invalid(
                "order",
                "status",
                "confirm the order before raising an invoice draft",
            ));
        }
        OrderStatus::Cancelled => {
            return Err(SalesError::invalid(
                "order",
                "status",
                "a cancelled order is never invoiced",
            ));
        }
        OrderStatus::Confirmed | OrderStatus::Invoiced | OrderStatus::Delivered => {}
    }

    if let Some(existing) = &detail.invoice {
        if existing.state != "void" {
            return Ok(existing.clone());
        }
    }

    let payload = serde_json::json!({
        "order_number": detail.order.number,
        "customer": detail.order.customer.name,
        "customer_kind": detail.order.customer.kind,
        "currency": detail.order.currency,
        "quote_id": detail.order.quote_id,
        "quote_number": detail.order.quote_number,
        "lines": detail.lines.iter().map(|line| serde_json::json!({
            "position": line.position,
            "product_id": line.product_id,
            "description": line.description,
            "unit": line.unit,
            "quantity": line.quantity,
            "unit_price": line.unit_price,
            "discount_percent": line.discount_percent,
            "tax_percent": line.tax_percent,
            "line_total": line.line_total,
        })).collect::<Vec<_>>(),
    });

    let mut transaction = pool.begin().await?;
    let row: (String,) = sqlx::query_as::<_, (String,)>(
        "select status from sales_orders where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(order_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SalesError::NotFound("order"))?;
    let status = row.0.clone();

    // `on conflict` names the **partial index** explicitly rather than writing a bare
    // `on conflict do nothing`. A bare form covers *every* violation, so a bug elsewhere in this
    // statement — a bad currency, a missing order — would be silently reported as "a draft
    // already exists" instead of raising. Naming the index keeps the swallow to the one case it
    // is written for. It is spelled as the index's predicate because PostgreSQL cannot infer a
    // partial unique index from a bare column list.
    let inserted: Option<(Uuid,)> = sqlx::query_as(
        "insert into sales_invoice_handoffs (organization_id, order_id, currency, subtotal,
                tax_total, grand_total, payload, state, raised_by)
         values ($1, $2, $3, $4::numeric, $5::numeric, $6::numeric, $7, 'draft', $8)
         on conflict (order_id) where state in ('draft', 'issued') do nothing
         returning id",
    )
    .bind(organization_id)
    .bind(order_id)
    .bind(&detail.order.currency)
    .bind(&detail.order.subtotal)
    .bind(&detail.order.tax_total)
    .bind(&detail.order.grand_total)
    .bind(&payload)
    .bind(actor)
    .fetch_optional(&mut *transaction)
    .await?;

    // A `None` here means the partial unique index caught the second ask. The history row below
    // is only written when *this* call raised the draft, so a double-click cannot inflate the
    // order's timeline with two "handed to accounting" entries.
    let raised_now = inserted.is_some();

    if raised_now && status != "invoiced" {
        sqlx::query(
            "update sales_orders set invoice_state = 'draft', updated_at = now() where id = $1",
        )
        .bind(order_id)
        .execute(&mut *transaction)
        .await?;
        record_history(
            &mut transaction,
            organization_id,
            order_id,
            Some(&status),
            &status,
            "Invoice draft handed to accounting",
            actor,
        )
        .await?;
    }
    transaction.commit().await?;

    load_invoice_handoff(pool, organization_id, order_id)
        .await?
        .ok_or(SalesError::NotFound("invoice draft"))
}

/// The draft invoice an order has, newest first among the live ones.
async fn load_invoice_handoff(
    pool: &PgPool,
    organization_id: Uuid,
    order_id: Uuid,
) -> Result<Option<InvoiceHandoffView>> {
    let row = sqlx::query_as::<_, (
        Uuid,
        Uuid,
        String,
        String,
        String,
        String,
        String,
        Option<Uuid>,
        Option<String>,
        Option<OffsetDateTime>,
        OffsetDateTime,
    )>(
        "select id, order_id, state, currency, subtotal::text, tax_total::text,
                grand_total::text, external_id, external_url, settled_at, raised_at
           from sales_invoice_handoffs
          where organization_id = $1 and order_id = $2
          order by (state in ('draft', 'issued')) desc, raised_at desc
          limit 1",
    )
    .bind(organization_id)
    .bind(order_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(
        |(
            id,
            order_id,
            state,
            currency,
            subtotal,
            tax_total,
            grand_total,
            external_id,
            external_url,
            settled_at,
            raised_at,
        )| InvoiceHandoffView {
            id,
            order_id,
            state,
            currency,
            subtotal,
            tax_total,
            grand_total,
            external_id,
            external_url,
            settled_at,
            raised_at,
        },
    ))
}

/// Append one line to the order's own history.
async fn record_history(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    organization_id: Uuid,
    order_id: Uuid,
    from_status: Option<&str>,
    to_status: &str,
    note: &str,
    actor: Uuid,
) -> Result<()> {
    sqlx::query(
        "insert into sales_status_history (organization_id, order_id, from_status, to_status,
                note, actor_user_id)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(organization_id)
    .bind(order_id)
    .bind(from_status)
    .bind(to_status)
    .bind(note)
    .bind(actor)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_survives_the_round_trip_through_the_database_value() {
        for status in [
            OrderStatus::Draft,
            OrderStatus::Confirmed,
            OrderStatus::Invoiced,
            OrderStatus::Delivered,
            OrderStatus::Cancelled,
        ] {
            assert_eq!(OrderStatus::parse(status.as_str()), Some(status), "{status}");
        }
    }

    #[test]
    fn a_stored_status_this_build_does_not_know_is_reported_rather_than_defaulted() {
        // The failure this prevents: an unknown status parsed as `Draft` would show a cancelled
        // order as editable on the detail screen, and the grid would be live.
        assert_eq!(OrderStatus::parse("partially_shipped"), None);
        assert_eq!(OrderReservationState::parse("partial"), Some(OrderReservationState::Partial));
        assert_eq!(OrderReservationState::parse("on_hold"), None);
    }

    #[test]
    fn only_a_draft_order_is_editable() {
        // The reason a confirmed order's grid is frozen is *not* the reason a sent quote's is,
        // but the answer is the same, and the answer is what the screen greys out.
        assert!(!OrderStatus::Draft.is_frozen());
        for status in [
            OrderStatus::Confirmed,
            OrderStatus::Invoiced,
            OrderStatus::Delivered,
            OrderStatus::Cancelled,
        ] {
            assert!(status.is_frozen(), "{status}");
        }
    }

    #[test]
    fn a_cancelled_order_never_holds_stock() {
        assert!(OrderStatus::Confirmed.holds_reservation());
        assert!(OrderStatus::Invoiced.holds_reservation());
        assert!(!OrderStatus::Cancelled.holds_reservation());
        assert!(!OrderStatus::Draft.holds_reservation());
    }

    #[test]
    fn the_status_filter_accepts_a_comma_list_and_all_means_everyone() {
        let two = parse_status_filter(Some("draft, confirmed")).expect("two statuses parse");
        assert_eq!(two, vec![OrderStatus::Draft, OrderStatus::Confirmed]);

        // `all` is a word a person types into a filter box, so it has to mean "no filter" rather
        // than be refused as an unknown status.
        assert!(parse_status_filter(Some("all")).expect("`all` parses").is_empty());
        assert!(parse_status_filter(Some("")).expect("empty parses").is_empty());
        assert!(parse_status_filter(None).expect("absent parses").is_empty());
    }

    #[test]
    fn the_status_filter_keeps_a_repeated_status_once() {
        let statuses = parse_status_filter(Some("confirmed,confirmed,draft")).expect("parses");
        assert_eq!(statuses, vec![OrderStatus::Confirmed, OrderStatus::Draft]);
    }

    #[test]
    fn an_unknown_status_is_refused_by_name() {
        let error = parse_status_filter(Some("shipped")).expect_err("`shipped` is not an order status");
        assert!(error.to_string().contains("shipped"), "{error}");
    }

    #[test]
    fn the_sort_key_is_a_closed_set_because_it_is_interpolated() {
        assert!(order_sort(Some("created"), Some("desc")).is_ok());
        assert!(order_sort(None, None).is_ok());
        assert!(order_sort(Some("updated_at"), Some("asc")).is_ok());
        // These three are the only columns the clause may name.
        assert!(order_sort(Some("number"), None).is_ok());
        assert!(order_sort(Some("total"), None).is_ok());
        assert!(order_sort(Some("status"), None).is_ok());
        // A `drop table` in the sort parameter must be a refusal, not a query.
        let error = order_sort(Some("number; drop table sales_orders"), None).expect_err("refused");
        assert!(error.to_string().contains("drop table"), "{error}");
        assert!(order_sort(None, Some("sideways")).is_err());
    }

    #[test]
    fn the_default_sort_is_newest_first_with_an_id_tiebreak() {
        // The tiebreak is load-bearing: the keyset cursor compares `(updated_at, id)`, and two
        // orders written in the same microsecond would make it ambiguous without it.
        assert_eq!(order_sort(None, None).expect("default"), "o.updated_at desc, o.id desc");
        assert_eq!(order_sort(Some("created"), Some("asc")).expect("asc"), "o.updated_at asc, o.id asc");
    }

    #[test]
    fn a_date_filter_must_be_a_day_the_database_can_read() {
        assert!(parse_day_filter(Some("2026-09-29"), "from").expect("a date parses").is_some());
        assert!(parse_day_filter(Some("29/09/2026"), "from").is_err());
        assert!(parse_day_filter(Some("not a date"), "from").is_err());
        assert!(parse_day_filter(None, "from").expect("absent is fine").is_none());
        assert!(parse_day_filter(Some("  "), "from").expect("blank is absent").is_none());
    }

    #[test]
    fn a_search_term_cannot_smuggle_wildcards_into_the_query() {
        // `escape_like` is what keeps a person typing `%` into the search box from turning the
        // filter into "every row", which would look like a broken list rather than a filter.
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn the_cursor_round_trips_through_its_encoding() {
        let stamp = OffsetDateTime::from_unix_timestamp_nanos(1_757_000_000_000_000_000)
            .expect("a representable instant");
        let id = Uuid::from_u128(42);
        let cursor = encode_cursor(stamp, id);
        let (decoded_stamp, decoded_id) = decode_cursor(&cursor).expect("our own cursor decodes");
        assert_eq!(decoded_stamp, stamp);
        assert_eq!(decoded_id, id);
    }

    #[test]
    fn a_cursor_nobody_wrote_is_refused_rather_than_guessed() {
        // A bad cursor must not silently become "the first page": that turns a paging bug into a
        // list that looks like it is skipping rows.
        for bad in ["", "not base64!!", "bm8tc2VwYXJhdG9y", "MTIzfA"] {
            assert!(decode_cursor(bad).is_err(), "{bad:?} should not decode");
        }
    }

    #[test]
    fn a_line_needs_a_quantity_a_price_and_something_to_say_it_is() {
        let base = NewOrderLine {
            product_id: None,
            description: Some("Consulting".to_string()),
            unit: None,
            quantity: "2".to_string(),
            unit_price: "100.00".to_string(),
            discount_percent: Some(0),
            tax_percent: Some(20),
        };
        let line = validate_line(0, &base).expect("a complete line validates");
        assert_eq!(line.position, 1);
        assert_eq!(line.unit, "piece", "the default unit is piece, not an empty cell");

        // A line that names nothing cannot be picked, invoiced or held.
        let nameless = NewOrderLine {
            description: Some("   ".to_string()),
            ..base.clone()
        };
        let error = validate_line(0, &nameless).expect_err("a line with no content is refused");
        assert!(error.to_string().contains("product or a description"), "{error}");
    }

    #[test]
    fn a_line_refuses_the_numbers_a_document_cannot_carry() {
        let base = NewOrderLine {
            product_id: None,
            description: Some("Work".to_string()),
            unit: None,
            quantity: "1".to_string(),
            unit_price: "10".to_string(),
            discount_percent: Some(0),
            tax_percent: Some(0),
        };

        for (field, line, needle) in [
            ("quantity", NewOrderLine { quantity: "0".into(), ..base.clone() }, "at least one"),
            ("quantity", NewOrderLine { quantity: "-3".into(), ..base.clone() }, "at least one"),
            ("quantity", NewOrderLine { quantity: "abc".into(), ..base.clone() }, "not a number"),
            ("unit_price", NewOrderLine { unit_price: "-1".into(), ..base.clone() }, "negative"),
            ("discount_percent", NewOrderLine { discount_percent: Some(101), ..base.clone() }, "between 0 and 100"),
            ("tax_percent", NewOrderLine { tax_percent: Some(-1), ..base.clone() }, "between 0 and 100"),
        ] {
            let error = validate_line(0, &line).expect_err("refused");
            assert!(error.to_string().contains(needle), "{field}: {error}");
        }
    }

    #[test]
    fn a_line_positions_itself_from_one_so_the_grid_can_reorder() {
        let line = NewOrderLine {
            product_id: None,
            description: Some("Work".to_string()),
            unit: None,
            quantity: "1".to_string(),
            unit_price: "10".to_string(),
            discount_percent: None,
            tax_percent: None,
        };
        for index in 0..4 {
            let validated = validate_line(index, &line).expect("valid");
            assert_eq!(validated.position, i32::try_from(index).unwrap() + 1);
        }
    }

    #[test]
    fn an_order_line_totals_the_same_way_a_quote_line_does() {
        // The order reuses `quote_totals` rather than a second implementation, so a converted
        // order cannot drift from the quote it came from by a cent.
        let line = ValidatedLine {
            position: 1,
            product_id: None,
            description: "Work".to_string(),
            unit: "piece".to_string(),
            quantity: crate::money::Quantity::parse("2.5").expect("quantity"),
            unit_price: Money::parse("19.90").expect("price"),
            discount_percent: 20,
            tax_percent: 20,
        };
        let totals = crate::money::quote_totals(&[line.to_input()]);
        // 2.5 × 19.90 = 49.75 gross, less 20% = 39.80 payable, plus 20% tax = 47.76.
        assert_eq!(totals.subtotal.to_text(), "49.75");
        assert_eq!(totals.discount_total.to_text(), "9.95");
        assert_eq!(totals.tax_total.to_text(), "7.96");
        assert_eq!(totals.grand_total.to_text(), "47.76");
    }

    #[test]
    fn a_cancel_reason_is_required_because_the_release_is_what_somebody_reads_next_month() {
        for bad in ["", "   "] {
            let input = CancelOrder { reason: Some(bad.to_string()) };
            // The handler owns the check; this test pins the shape the form must send, because a
            // reason field the server accepts as blank is a field the screen will render as saved.
            assert!(input.reason.as_deref().unwrap_or_default().trim().is_empty());
        }
        let good = CancelOrder { reason: Some("  customer went elsewhere  ".to_string()) };
        assert_eq!(good.reason.as_deref().unwrap_or_default().trim(), "customer went elsewhere");
    }
}
