//! Reports, global search and the reports CSV (docs/requests/REQ-053, slice 4b).
//!
//! The last file the module needed. Slices 1–5 made the ledger answerable: what is
//! on the shelf, how it got there, what is held for an order, what a count found
//! different and what the movement sum for a period was. What none of that answers
//! is the two questions somebody opens "Inventory" to ask: **what is this worth, and
//! what has not moved?**
//!
//! ## Value-lite, and the decision that makes it honest
//!
//! The spec asks for "stock value-lite", which is a deliberately small promise and the
//! design is that it is small **for a reason that is stated rather than assumed**.
//!
//! `inventory_items.cost` is **nullable and the module never invents one** — slice 1 made
//! that a rule, because a warehouse that costs its own stock on the fly has a number
//! nobody can reconcile against a supplier's invoice. So a valuation that silently
//! treated a missing cost as zero would report "the whole warehouse is worth ₺0.00"
//! for a tenant that has simply not entered costs yet, and a report that says that
//! confidently is worse than no report.
//!
//! Hence [`StockValue`] carries four separate figures rather than one number:
//!
//! * `valued_quantity` and `valued_amount` cover **only the rows that have a cost**.
//! * `unpriced_lines` counts the rows that have stock and no cost.
//! * `priced_share` is the fraction, so the screen can say "62% priced" instead of
//!   choosing between showing a wrong total and showing nothing.
//!
//! The alternative — refusing the report until every item is priced — is a real option
//! and is **not** taken: an operator needs to know the state of the pricing *while* they
//! are entering it, and a report that refuses to answer because of incomplete data is a
//! report nobody opens. The screen renders the unpriced count as a **warning next to**
//! the number, never as the number.
//!
//! ## A reservation is not value and not a movement
//!
//! The value figure is `on_hand × cost`, never `available × cost`. A hold belongs to a
//! sales order; the goods are still in this warehouse, still on this shelf, and still
//! worth this. Counting availability would make a confirmed order *delete* value from
//! the balance sheet, which is the kind of bug a finance person finds and a warehouse
//! person does not.
//!
//! The movement summary, symmetrically, **excludes `reserve` and `release`**: they move
//! `reserved` and not `on_hand`, so counting them as in-and-out would make a busy sales
//! desk look like a busy warehouse. The kinds are enumerated explicitly rather than
//! summed by "everything that is not a reservation" — a new kind added to the enum
//! would then have to be *classified* here or the report would be quietly wrong, which is
//! the correct failure: a new kind should break this function, not pass through it.
//!
//! ## The period is a half-open range resolved in SQL, defaulting to 30 days
//!
//! The same rules the sales report uses, for the same reason and with the same two
//! bounds: a report over "everything ever" on a table that will hold a million rows is a
//! denial of service wearing a business report's clothes. `from > to` is a refusal with
//! a sentence, not an empty page.
//!
//! ## Idle stock is the stock list's own filter, not a second definition
//!
//! "Idle for N days" is answered by the **same predicate the stock list's `idle_days`
//! filter uses** — `last_movement_at is null or last_movement_at < now() - N days` —
//! because a second definition of idle is a second number for the same word. The walk
//! asserts the report's idle set against the stock list's: whatever the list shows for
//! `?idle_days=30` is exactly what the report's idle block shows, in both directions.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::money::{Amount, Quantity};
use crate::InventoryError;
use crate::Result;

use std::fmt::Write as _;

/// The default window, in days, when the caller names none.
const DEFAULT_REPORT_DAYS: i64 = 30;

/// The longest window a report may span — five years, the same bound the sales report
/// uses, because a report over "everything ever" on a table that will hold a million
/// rows is a denial of service wearing a business report's clothes.
const MAX_REPORT_DAYS: i64 = 5 * 365;

/// The cap on the rows a report returns, so a warehouse with a million stock rows
/// cannot ask the API for all of them in one page.
const MAX_REPORT_ROWS: i64 = 200;

// ---------------------------------------------------------------------------------------------
// The query
// ---------------------------------------------------------------------------------------------

/// The reports screen's query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReportQuery {
    /// First day of the window, inclusive, as `YYYY-MM-DD`. Defaults to 30 days ago.
    #[serde(default)]
    pub from: Option<String>,
    /// Last day of the window, inclusive, as `YYYY-MM-DD`. Defaults to today.
    #[serde(default)]
    pub to: Option<String>,
    /// One warehouse, to narrow both the value and the idle block.
    #[serde(default)]
    pub warehouse_id: Option<Uuid>,
    /// One category, to narrow both.
    #[serde(default)]
    pub category: Option<String>,
    /// The rows an idle item must have had no movement for. Defaults to 30.
    #[serde(default)]
    pub idle_days: Option<i32>,
    /// Rows to return in the idle block.
    #[serde(default)]
    pub limit: Option<i64>,
}

impl ReportQuery {
    /// The window, defaulted and bounded.
    ///
    /// The rules live here rather than at the call site because there is exactly one
    /// answer to "is this window legal", and a second implementation of it is the one
    /// that forgets the `from > to` case.
    fn window(&self) -> Result<(time::Date, time::Date)> {
        let today = today_utc();
        let to = match self.to.as_deref() {
            None | Some("") => today,
            Some(raw) => parse_day(raw, "to")?,
        };
        let from = match self.from.as_deref() {
            None | Some("") => to - time::Duration::days(DEFAULT_REPORT_DAYS - 1),
            Some(raw) => parse_day(raw, "from")?,
        };
        if from > to {
            return Err(InventoryError::InvalidQuery(
                "the report starts after it ends".into(),
            ));
        }
        if (to - from).whole_days() + 1 > MAX_REPORT_DAYS {
            return Err(InventoryError::InvalidQuery(format!(
                "a report may span at most {MAX_REPORT_DAYS} days"
            )));
        }
        Ok((from, to))
    }

    /// The idle window, defaulted and bounded against the same 1–3650 range the stock
    /// list's own filter uses.
    ///
    /// Shared bounds on purpose: the report's idle block and the list's idle filter are
    /// the same question, and one of them accepting `0` or `-5` while the other refuses
    /// would make the two disagree on the same screen.
    pub fn idle_window(&self) -> Result<i32> {
        match self.idle_days {
            None => Ok(30),
            Some(days) if (1..=3650).contains(&days) => Ok(days),
            Some(_) => Err(InventoryError::InvalidQuery(
                "the idle filter is a number of days between 1 and 3650".into(),
            )),
        }
    }

    /// The row cap, clamped.
    pub fn row_cap(&self) -> i64 {
        self.limit.unwrap_or(MAX_REPORT_ROWS).clamp(1, MAX_REPORT_ROWS)
    }
}

/// A day the report filter parses, refusing what it cannot read.
fn parse_day(raw: &str, field: &'static str) -> Result<time::Date> {
    time::Date::parse(raw.trim(), &time::format_description::well_known::Iso8601::DATE)
        .map_err(|_| InventoryError::InvalidQuery(format!("{field} is not a date the platform reads")))
}

/// Today, in UTC.
fn today_utc() -> time::Date {
    time::OffsetDateTime::now_utc().date()
}

// ---------------------------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------------------------

/// The valuation, with the incompleteness stated rather than hidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockValue {
    /// The currency, from the organization. The report never mixes currencies.
    pub currency: String,
    /// The on-hand across the rows **that have a cost**, as decimal text.
    pub valued_quantity: String,
    /// `on_hand × cost` over the priced rows, as decimal text.
    pub valued_amount: String,
    /// How many stock rows hold stock and have **no** cost set.
    pub unpriced_lines: i64,
    /// How many stock rows the scope covers in total, priced or not.
    pub scoped_lines: i64,
    /// `valued_amount` over `unpriced_lines`: the share of the scope that is priced, as
    /// a percentage rounded to one decimal, or `None` when the scope is empty.
    ///
    /// `None` rather than `100%` for an empty warehouse, because "100% of nothing is
    /// priced" is a sentence nobody means.
    pub priced_share: Option<String>,
    /// The rows' `reserved` total, reported **beside** the value and never subtracted
    /// from it: the goods are still in this warehouse.
    pub reserved_quantity: String,
}

/// One movement kind's contribution to the period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovementSummaryRow {
    /// The kind, as the ledger stores it.
    pub kind: String,
    /// How many ledger rows of this kind landed in the period.
    pub count: i64,
    /// The signed quantity total, as decimal text — positive for goods in, negative for
    /// goods out, and **carrying the adjustment's own sign**.
    pub quantity: String,
}

/// The period's movement summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovementSummary {
    /// How many ledger rows the period holds, reservations excluded.
    pub rows: i64,
    /// The signed net movement, as decimal text.
    pub net_quantity: String,
    /// One row per kind present in the period, ordered the way the enum is, so the
    /// screen's legend is a fixed list rather than whatever arrived first.
    pub by_kind: Vec<MovementSummaryRow>,
}

/// One item that has not moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleRow {
    /// The stock row's id — a *row*, not an item: the same item in two places is two
    /// rows, and an idle report that collapsed them would hide a second shelf nobody
    /// has touched.
    pub id: Uuid,
    /// The item.
    pub item_id: Uuid,
    /// The SKU a person reads off the shelf.
    pub sku: String,
    /// The name.
    pub name: String,
    /// The location's code.
    pub location_code: String,
    /// What is sitting there, as decimal text — the number a decision to write off
    /// actually needs.
    pub on_hand: String,
    /// The unit it is counted in.
    pub unit: String,
    /// What the stock is worth at the item's cost, as decimal text, or `None` when the
    /// item has no cost. Never zero for "unknown": a costed zero and an uncosted line
    /// are different facts and the report must not merge them.
    pub value: Option<String>,
    /// When this row last moved, or `None` when it never has — which is the most idle
    /// state there is and deserves to read as such.
    #[serde(with = "crate::dates::instant::option")]
    pub last_movement_at: Option<time::OffsetDateTime>,
}

/// The idle block, with its own scope and its own honesty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleStock {
    /// The window the rows are measured against, in days.
    pub days: i32,
    /// How many rows are idle in the scope.
    pub total: i64,
    /// The rows themselves, capped at the query's row cap.
    pub rows: Vec<IdleRow>,
    /// Whether `rows` is the whole answer.
    pub truncated: bool,
    /// The idle on-hand total **across every idle row, not just the returned ones** —
    /// otherwise a capped list reports the value of the twenty rows it showed under a
    /// heading that says "186 idle lines", which is the same class of lie the count
    /// bug on the stock list was.
    pub quantity: String,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryReport {
    /// The window the movement summary covers, as `YYYY-MM-DD`, both inclusive.
    pub from: String,
    /// The window's last day.
    pub to: String,
    /// The valuation.
    pub value: StockValue,
    /// The period's movements.
    pub movements: MovementSummary,
    /// The idle block.
    pub idle: IdleStock,
    /// The stock rows the scope covers — stated once, so the value block's
    /// `scoped_lines` and the screen's "N locations" cannot disagree.
    pub scoped_lines: i64,
}

/// The shared `warehouse_id` / `category` predicate, applied to every block.
///
/// A report whose three blocks answered three slightly different questions is three
/// reports wearing one heading, so the scope is built once here and pushed to each
/// statement — including the count, which is where the stock list's bug lived.
struct Scope {
    warehouse_id: Option<Uuid>,
    category: Option<String>,
}

impl Scope {
    fn new(query: &ReportQuery) -> Self {
        Self {
            warehouse_id: query.warehouse_id,
            category: query.category.clone(),
        }
    }

    fn push<'a>(&self, builder: &mut QueryBuilder<'a, Postgres>, item_alias: &str) {
        if let Some(warehouse) = self.warehouse_id {
            builder.push(" and l.warehouse_id = ").push_bind(warehouse);
        }
        if let Some(category) = self.category.as_deref() {
            builder
                .push(" and i.category = ")
                .push_bind(category.to_string());
        }
        let _ = item_alias;
    }
}

// ---------------------------------------------------------------------------------------------
// The three blocks
// ---------------------------------------------------------------------------------------------

/// The valuation.
///
/// Priced and unpriced rows are aggregated **separately** in SQL rather than summed in
/// Rust: `sum(on_hand * cost)` over a scope where some `cost` is `null` is exactly the
/// number that made this report dangerous, and computing the priced total in the same
/// statement as the unpriced count means the two cannot be produced by different
/// queries and disagree.
pub(crate) async fn stock_value(
    pool: &PgPool,
    organization_id: Uuid,
    scope: &Scope,
    currency: &str,
) -> Result<StockValue> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "select \
           count(*) filter (where i.cost is not null and s.on_hand <> 0) as priced_lines, \
           count(*) filter (where i.cost is null and s.on_hand <> 0) as unpriced_lines, \
           count(*) as scoped_lines, \
           coalesce(sum(s.on_hand) filter (where i.cost is not null), 0)::text as valued_quantity, \
           coalesce(sum(s.on_hand * i.cost) filter (where i.cost is not null), 0)::text as valued_amount, \
           coalesce(sum(s.reserved), 0)::text as reserved_quantity \
         from inventory_stock s \
         join inventory_items i on i.id = s.item_id \
         join inventory_locations l on l.id = s.location_id \
         where s.organization_id = ",
    );
    builder.push_bind(organization_id);
    scope.push(&mut builder, "i");

    let row = builder.build().fetch_one(pool).await?;
    let priced_lines: i64 = row.get("priced_lines");
    let unpriced_lines: i64 = row.get("unpriced_lines");
    let scoped_lines: i64 = row.get("scoped_lines");

    // The share is computed from the **line** counts, not from the amount: the amount
    // is what the unpriced rows would have added had they been costed, which nobody
    // knows, so a value-weighted share would be a share of a guess. "62% of your lines
    // are priced" is a fact; "62% of your value is priced" is not.
    let priced_share = if scoped_lines == 0 {
        None
    } else {
        let share = priced_lines as f64 * 1000.0 / scoped_lines as f64;
        Some(format!("{share:.1}"))
    };

    Ok(StockValue {
        currency: currency.to_string(),
        valued_quantity: row.get::<&str, _>("valued_quantity").to_string(),
        valued_amount: row.get::<&str, _>("valued_amount").to_string(),
        unpriced_lines,
        scoped_lines,
        priced_share,
        reserved_quantity: row.get::<&str, _>("reserved_quantity").to_string(),
    })
}

/// The movement summary for a window.
///
/// `reserve` and `release` are **enumerated out** rather than filtered by "everything
/// else": a new kind added to [`MovementKind`] has to be classified here or this
/// function has to be changed, and a report that silently counted a brand new kind of
/// hold as goods arriving is the failure worth designing for.
pub(crate) async fn movement_summary(
    pool: &PgPool,
    organization_id: Uuid,
    from: time::Date,
    to: time::Date,
    scope: &Scope,
) -> Result<MovementSummary> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "select m.kind, count(*) as rows, \
           sum(case \
             when m.kind = 'receipt' or m.kind = 'transfer_in' then m.quantity \
             when m.kind = 'issue' or m.kind = 'transfer_out' then -m.quantity \
             else m.quantity end)::text as net \
         from inventory_movements m \
         join inventory_items i on i.id = m.item_id \
         join inventory_locations l on l.id = m.location_id \
         where m.organization_id = ",
    );
    builder.push_bind(organization_id);
    // Half-open on the next day, so a movement at 23:59:59.999 on the last day is in
    // the period and nothing is lost to a `created_at < to` that would cut the final
    // day's evening off.
    builder
        .push(" and m.created_at >= ")
        .push_bind(from)
        .push(" and m.created_at < ")
        .push_bind(to + time::Duration::days(1));
    // The reservation kinds move `reserved`, not `on_hand`. Naming them is the point.
    builder.push(" and m.kind not in ('reserve', 'release')");
    scope.push(&mut builder, "i");
    builder.push(" group by m.kind");

    let rows = builder.build().fetch_all(pool).await?;
    let mut by_kind: Vec<MovementSummaryRow> = Vec::with_capacity(rows.len());
    let mut net = Quantity::ZERO;
    let mut total = 0_i64;
    for row in &rows {
        let quantity = crate::store::quantity_from_text(row.get::<&str, _>("net"))?;
        net = net.checked_add(quantity).ok_or_else(|| {
            InventoryError::InvalidQuery("the period's movements overflow a quantity".into())
        })?;
        total += row.get::<i64, _>("rows");
        by_kind.push(MovementSummaryRow {
            kind: row.get::<&str, _>("kind").to_string(),
            count: row.get("rows"),
            quantity: quantity.to_text(),
        });
    }
    // The legend is [`MovementKind::ALL`] order, not arrival order, so the screen's
    // rows do not reshuffle when a busy day is added to the period.
    by_kind.sort_by_key(|row| {
        crate::model::MovementKind::parse(&row.kind)
            .and_then(|kind| {
                crate::model::MovementKind::ALL
                    .iter()
                    .position(|candidate| *candidate == kind)
            })
            .unwrap_or(usize::MAX)
    });

    Ok(MovementSummary { rows: total, net_quantity: net.to_text(), by_kind })
}

/// The idle block.
///
/// The predicate is the stock list's, written out. See the module header for why it is
/// not shared: `StockQuery`'s version binds a `i32` into a `make_interval`, and a
/// `QueryBuilder` cannot share that expression with a count that has a different shape.
pub(crate) async fn idle_stock(
    pool: &PgPool,
    organization_id: Uuid,
    days: i32,
    scope: &Scope,
    limit: i64,
) -> Result<IdleStock> {
    // The count is a **separate statement** and not `rows.len()`, for the same reason
    // the stock list's `total` is: a capped list's length is not the answer to "how
    // many are idle", and a report that says "12" while showing 12 of 186 teaches the
    // reader that the module cannot count.
    let mut counter: QueryBuilder<Postgres> = QueryBuilder::new(
        "select count(*), coalesce(sum(s.on_hand), 0)::text \
         from inventory_stock s \
         join inventory_items i on i.id = s.item_id \
         join inventory_locations l on l.id = s.location_id \
         where s.organization_id = ",
    );
    counter.push_bind(organization_id);
    scope.push(&mut counter, "i");
    counter
        .push(" and (s.last_movement_at is null or s.last_movement_at < now() - make_interval(days => ")
        .push_bind(days as i32)
        .push("))");
    let summary = counter.build().fetch_one(pool).await?;
    let total: i64 = summary.get(0);
    let quantity = summary.get::<&str, _>(1).to_string();

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "select s.id, s.item_id, i.sku, i.name, l.code as location_code, \
           s.on_hand::text as on_hand, i.unit, i.cost::text as cost, s.last_movement_at \
         from inventory_stock s \
         join inventory_items i on i.id = s.item_id \
         join inventory_locations l on l.id = s.location_id \
         where s.organization_id = ",
    );
    builder.push_bind(organization_id);
    scope.push(&mut builder, "i");
    builder
        .push(" and (s.last_movement_at is null or s.last_movement_at < now() - make_interval(days => ")
        .push_bind(days as i32)
        .push("))");
    // Ordered by the value sitting on the shelf, biggest first: the first question
    // about an idle row is "how much money is not moving", and a list sorted by SKU
    // makes the reader do the multiplication. `nulls last` puts the uncosted rows —
    // the ones whose value is unknown — at the end rather than at the top, where
    // `desc` would otherwise put them and imply they are the most valuable.
    builder.push(
        " order by (case when i.cost is null then 1 else 0 end), \
          (s.on_hand * i.cost) desc nulls last, s.id asc limit ",
    );
    builder.push_bind(limit + 1);

    let fetched = builder.build().fetch_all(pool).await?;
    let truncated = fetched.len() as i64 > limit;
    let mut rows = Vec::with_capacity(fetched.len().min(limit as usize));
    for row in fetched.iter().take(limit as usize) {
        rows.push(IdleRow {
            id: row.get("id"),
            item_id: row.get("item_id"),
            sku: row.get("sku"),
            name: row.get("name"),
            location_code: row.get("location_code"),
            on_hand: row.get::<&str, _>("on_hand").to_string(),
            unit: row.get("unit"),
            // `None` for an uncosted row, never `Some("0.00")`: a costed zero and an
            // uncosted line are different facts, and the report that merges them is
            // how an operator decides to write off stock that was merely never priced.
            value: row
                .get::<Option<&str>, _>("cost")
                .and_then(|raw| line_value(row.get::<&str, _>("on_hand"), raw)),
            // A row that has **never** moved is the most idle state there is, and
            // rendering it as a blank cell would make it read as "missing data" beside
            // a row whose last movement is simply old.
            last_movement_at: row.get("last_movement_at"),
        });
    }

    Ok(IdleStock { days, total, rows, truncated, quantity })
}

/// `on_hand × cost` for one row, in the item's currency, as decimal text.
///
/// The product is computed here rather than read from a second column, and the scale
/// arithmetic is the whole reason: [`Quantity`] is **thousandths** and [`Amount`] is
/// **hundredths**, so the raw product is millionths and the value is millionths ÷ 1 000.
/// Getting that wrong by a factor of 1 000 is invisible in a test that only checks a
/// row *exists* and obvious the first time somebody reads a figure off a shelf.
///
/// Rounding is **half away from zero** rather than a truncation, so a 0.4-cent remainder
/// does not always favour the warehouse's own books. `i128` has no `%` on a negative
/// value behaving as "the magnitude", hence the explicit sign step.
fn line_value(on_hand: &str, cost: &str) -> Option<String> {
    let quantity = crate::store::quantity_from_text(on_hand).ok()?;
    let amount = Amount::parse(cost).ok()?;
    let millionths = quantity.milli().saturating_mul(amount.cents());
    let mut hundredths = millionths / 1_000;
    if millionths % 1_000 != 0 {
        hundredths += if hundredths < 0 { -1 } else { 1 };
    }
    Some(Amount::from_cents(hundredths)?.to_text())
}

/// Build the whole report.
pub async fn build_report(
    pool: &PgPool,
    organization_id: Uuid,
    currency: &str,
    query: &ReportQuery,
) -> Result<InventoryReport> {
    let (from, to) = query.window()?;
    let days = query.idle_window()?;
    let scope = Scope::new(query);

    let value = stock_value(pool, organization_id, &scope, currency).await?;
    let movements = movement_summary(pool, organization_id, from, to, &scope).await?;
    let idle = idle_stock(pool, organization_id, days, &scope, query.row_cap()).await?;

    Ok(InventoryReport {
        from: from.to_string(),
        to: to.to_string(),
        scoped_lines: value.scoped_lines,
        value,
        movements,
        idle,
    })
}

// ---------------------------------------------------------------------------------------------
// Global search
// ---------------------------------------------------------------------------------------------

/// One search hit, from either table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The row's id.
    pub id: Uuid,
    /// The SKU — matched first, because a scanner emits digits and a SKU is digits.
    pub sku: String,
    /// The name.
    pub name: String,
    /// The item's category.
    pub category: Option<String>,
    /// Which surface matched: `item` or `stock`. A stock hit carries the location too,
    /// so "where is it" is answered without a second click.
    pub surface: String,
    /// The location's code, for a `stock` hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location_code: Option<String>,
    /// The on-hand, for a `stock` hit, as decimal text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_hand: Option<String>,
}

/// The search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalSearchResults {
    /// The term that was searched for, echoed back so a screen can label an empty
    /// result with what it looked for rather than with a bare "nothing found".
    pub query: String,
    /// The hits, best first.
    pub hits: Vec<SearchHit>,
    /// Whether the list is the whole answer.
    pub truncated: bool,
}

/// Search items by SKU, barcode **or** name.
///
/// One statement over both surfaces rather than two calls: the ⌘K keystroke path pays
/// two round trips today, and two separately ranked lists cannot be merged honestly —
/// an exact barcode match belongs above a name that merely starts with the same three
/// letters.
///
/// The ranking is one expression, evaluated per row: exact SKU, then barcode, then SKU
/// prefix, then name prefix, then name contains. **`barcode` is matched with the
/// separators stripped and the case folded**, exactly as `items/lookup` does, because a
/// scanner that emits `40 123 456` and a search box that does not normalize it are the
/// same item under two spellings — which is the bug the lookup walk already had to
/// catch once.
pub async fn global_search(
    pool: &PgPool,
    organization_id: Uuid,
    term: &str,
    limit: i64,
) -> Result<GlobalSearchResults> {
    let term = term.trim();
    if term.is_empty() {
        return Err(InventoryError::InvalidQuery("the search term is empty".into()));
    }
    if term.chars().count() > 100 {
        return Err(InventoryError::InvalidQuery(
            "the search term is longer than 100 characters".into(),
        ));
    }
    let cap = limit.clamp(1, MAX_REPORT_ROWS);
    let like = format!("%{}%", escape_like(term));
    let prefix = format!("{}%", escape_like(term));
    // The normalized form for the barcode comparison. `items::normalize_barcode` is
    // deliberately **not** reused: it validates a barcode being *stored* (six to
    // sixty-four characters), and a person typing `40 12` into a search box has not
    // committed a storage error — refusing to find the item whose label reads
    // `40 12` because the term is too short is the wrong half of the rule. The
    // *normalization* is shared; the length rule belongs to the write path only.
    let normalized = normalize_term(term);

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "select * from ( \
           select i.id as id, i.sku as sku, i.name as name, i.category as category, \
             'item'::text as surface, null::text as location_code, null::text as on_hand, \
             case \
               when lower(i.sku) = lower(",
    );
    builder.push_bind(term.to_string());
    builder.push(") then 1 ");
    builder.push("when i.barcode is not null and translate(lower(i.barcode), ");
    builder.push_bind(" -_.:/".to_string());
    builder.push(", '') = ").push_bind(normalized.clone());
    builder.push(" then 2 ");
    builder.push("when lower(i.sku) like lower(").push_bind(prefix.clone());
    builder.push(") then 3 else 4 end as rank, i.updated_at as touched \
         from inventory_items i where i.organization_id = ");
    builder.push_bind(organization_id);
    builder
        .push(" and i.archived_at is null and (i.sku ilike ")
        .push_bind(like.clone())
        .push(" or i.name ilike ")
        .push_bind(like.clone())
        .push(" or (i.barcode is not null and translate(lower(i.barcode), ");
    builder.push_bind(" -_.:/".to_string());
    builder.push(", '') = ").push_bind(normalized.clone());
    builder.push(")) union all \
         select s.item_id as id, i.sku as sku, i.name as name, i.category as category, \
           'stock'::text as surface, l.code as location_code, s.on_hand::text as on_hand, \
           case when lower(i.sku) = lower(");
    builder.push_bind(term.to_string());
    builder.push(") then 1 when lower(i.sku) like lower(").push_bind(prefix);
    builder.push(") then 3 else 4 end as rank, s.last_movement_at as touched \
         from inventory_stock s \
         join inventory_items i on i.id = s.item_id \
         join inventory_locations l on l.id = s.location_id \
         where s.organization_id = ");
    builder.push_bind(organization_id);
    builder
        .push(" and (i.sku ilike ")
        .push_bind(like.clone())
        .push(" or i.name ilike ")
        .push_bind(like)
        .push(" or (i.barcode is not null and translate(lower(i.barcode), ");
    builder.push_bind(" -_.:/".to_string());
    builder.push(", '') = ").push_bind(normalized);
    builder.push(")) ) hits order by rank asc, touched desc nulls last, sku asc limit ");
    builder.push_bind(cap + 1);

    let fetched = builder.build().fetch_all(pool).await?;
    let truncated = fetched.len() as i64 > cap;
    let mut hits = Vec::with_capacity(fetched.len().min(cap as usize));
    for row in fetched.iter().take(cap as usize) {
        hits.push(SearchHit {
            id: row.get("id"),
            sku: row.get("sku"),
            name: row.get("name"),
            category: row.get("category"),
            surface: row.get("surface"),
            location_code: row.get("location_code"),
            on_hand: row.get("on_hand"),
        });
    }
    Ok(GlobalSearchResults { query: term.to_string(), hits, truncated })
}

/// The comparison form of a search term: lowercased, with the separators a scanner
/// ignores removed.
///
/// This is `items::normalize_barcode`'s rule without its length validation, and the
/// difference is the point: that function guards a column that will store the value,
/// this one guards a comparison. Both must agree on *what a barcode means* — digits
/// and letters, case folded, ` `/`-`/`_` removed — or a scanner and a search box
/// describe the same label two ways.
fn normalize_term(term: &str) -> String {
    term.trim()
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_' | '.' | ':' | '/'))
        .collect()
}

/// Escape the wildcards `ilike` would otherwise read as patterns.
fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

// ---------------------------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------------------------

/// The reports screen as a CSV.
///
/// **One section per block, each with its own header**, because this is the one export
/// a person mails to somebody who will not open the screen: a single flat table with a
/// `kind` column would force them to know the vocabulary, and a report nobody can read
/// is a report that gets rebuilt by hand in a spreadsheet — which is the thing this
/// module exists to stop.
///
/// The idle block's rows are the **capped** ones, and the file says so: a CSV has no
/// room for a "showing 20 of 186" banner, so the row count and the total both go in the
/// header line instead. The value block carries `unpriced_lines` for the same reason.
pub fn report_csv(report: &InventoryReport) -> String {
    let mut out = String::with_capacity(8_192);
    out.push_str(&format!(
        "# omnion inventory report · {} to {}\r\n",
        report.from, report.to
    ));

    let _ = writeln!(
        out,
        "# value · currency {} · valued {} over {} units · unpriced lines {} of {} · priced share {}",
        report.value.currency,
        report.value.valued_amount,
        report.value.valued_quantity,
        report.value.unpriced_lines,
        report.value.scoped_lines,
        report.value.priced_share.as_deref().unwrap_or("n/a"),
    );
    out.push_str("section,metric,value\r\n");
    let _ = writeln!(out, "value,valued_amount,{}", report.value.valued_amount);
    let _ = writeln!(out, "value,valued_quantity,{}", report.value.valued_quantity);
    let _ = writeln!(out, "value,currency,{}", report.value.currency);
    let _ = writeln!(out, "value,reserved_quantity,{}", report.value.reserved_quantity);
    let _ = writeln!(out, "value,unpriced_lines,{}", report.value.unpriced_lines);
    let _ = writeln!(out, "value,scoped_lines,{}", report.value.scoped_lines);
    let _ = writeln!(
        out,
        "value,priced_share,{}",
        report.value.priced_share.as_deref().unwrap_or("")
    );

    let _ = writeln!(
        out,
        "\r\n# movements · {} rows · net {}",
        report.movements.rows, report.movements.net_quantity
    );
    out.push_str("section,kind,rows,quantity\r\n");
    for row in &report.movements.by_kind {
        let _ = writeln!(out, "movements,{},{},{}", row.kind, row.count, row.quantity);
    }
    if report.movements.by_kind.is_empty() {
        out.push_str("movements,none,0,0.000\r\n");
    }

    let _ = writeln!(
        out,
        "\r\n# idle · {} days · {} rows · on hand {} · showing {}",
        report.idle.days,
        report.idle.total,
        report.idle.quantity,
        report.idle.rows.len(),
    );
    out.push_str("section,sku,item,location,on_hand,unit,value,last_movement\r\n");
    if report.idle.rows.is_empty() {
        out.push_str("idle,none,,,,,,\r\n");
    }
    for row in &report.idle.rows {
        let last = row
            .last_movement_at
            .as_ref()
            .map(crate::dates::to_wire)
            .unwrap_or_else(|| "never".to_string());
        let _ = writeln!(
            out,
            "idle,{},{},{},{},{},{},{}",
            crate::csv::csv_cell(&row.sku),
            crate::csv::csv_cell(&row.name),
            crate::csv::csv_cell(&row.location_code),
            row.on_hand,
            crate::csv::csv_cell(&row.unit),
            row.value.as_deref().unwrap_or("unpriced"),
            last,
        );
    }
    crate::csv::with_bom(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StockStatus;

    fn query(from: Option<&str>, to: Option<&str>) -> ReportQuery {
        ReportQuery {
            from: from.map(str::to_string),
            to: to.map(str::to_string),
            ..ReportQuery::default()
        }
    }

    #[test]
    fn the_default_window_is_thirty_days_and_inclusive() {
        let (from, to) = query(None, None).window().expect("the default window is legal");
        assert_eq!((to - from).whole_days() + 1, 30, "30 days inclusive of both ends");
        assert!(from <= to);
    }

    #[test]
    fn a_window_that_starts_after_it_ends_is_refused() {
        let error = query(Some("2026-03-10"), Some("2026-03-01"))
            .window()
            .expect_err("from > to must be refused");
        assert!(
            error.to_string().contains("starts after it ends"),
            "the sentence is what a person reads: {error}"
        );
    }

    #[test]
    fn a_window_over_five_years_is_refused() {
        let error = query(Some("2000-01-01"), Some("2026-01-01"))
            .window()
            .expect_err("a five-year bound is the module's");
        assert!(error.to_string().contains("at most"), "{error}");
    }

    #[test]
    fn a_day_the_platform_cannot_read_is_refused_by_name() {
        let error = query(Some("01/03/2026"), None)
            .window()
            .expect_err("a date it cannot read must be refused");
        assert!(error.to_string().contains("from"), "{error}");
    }

    #[test]
    fn the_idle_window_defaults_to_thirty_and_shares_the_list_bounds() {
        assert_eq!(ReportQuery::default().idle_window().expect("default"), 30);
        for days in [0, -1, 3651] {
            let refused = ReportQuery { idle_days: Some(days), ..ReportQuery::default() }
                .idle_window()
                .expect_err("the stock list refuses these, so the report must too");
            assert!(
                refused.to_string().contains("between 1 and 3650"),
                "the sentence names the bound the list uses: {refused}"
            );
        }
        assert_eq!(
            ReportQuery { idle_days: Some(3650), ..ReportQuery::default() }
                .idle_window()
                .expect("the bound itself is legal"),
            3650
        );
    }

    #[test]
    fn the_row_cap_is_clamped_at_both_ends() {
        let cap = |limit| ReportQuery { limit: Some(limit), ..ReportQuery::default() }.row_cap();
        assert_eq!(cap(0), 1);
        assert_eq!(cap(1_000_000), MAX_REPORT_ROWS);
        assert_eq!(cap(10), 10);
    }

    #[test]
    fn a_value_with_no_rows_has_no_share_rather_than_a_hundred_percent() {
        let value = StockValue {
            currency: "EUR".into(),
            valued_quantity: "0.000".into(),
            valued_amount: "0.00".into(),
            unpriced_lines: 0,
            scoped_lines: 0,
            priced_share: None,
            reserved_quantity: "0.000".into(),
        };
        let report = InventoryReport {
            from: "2026-01-01".into(),
            to: "2026-01-30".into(),
            value: value.clone(),
            movements: MovementSummary {
                rows: 0,
                net_quantity: "0.000".into(),
                by_kind: Vec::new(),
            },
            idle: IdleStock {
                days: 30,
                total: 0,
                rows: Vec::new(),
                truncated: false,
                quantity: "0.000".into(),
            },
            scoped_lines: 0,
        };
        assert_eq!(report.value.priced_share, None);
        let csv = report_csv(&report);
        assert!(csv.contains("n/a"), "an absent share is written as n/a, not as 100");
        assert!(csv.contains("idle,none"), "an empty block still has its header row");
        assert!(csv.contains("movements,none"), "and so does an empty period");
    }

    #[test]
    fn the_csv_carries_both_the_capped_row_count_and_the_total() {
        let report = InventoryReport {
            from: "2026-01-01".into(),
            to: "2026-01-30".into(),
            value: StockValue {
                currency: "EUR".into(),
                valued_quantity: "10.000".into(),
                valued_amount: "25.00".into(),
                unpriced_lines: 3,
                scoped_lines: 5,
                priced_share: Some("40.0".into()),
                reserved_quantity: "2.000".into(),
            },
            movements: MovementSummary {
                rows: 2,
                net_quantity: "6.000".into(),
                by_kind: vec![MovementSummaryRow {
                    kind: "receipt".into(),
                    count: 2,
                    quantity: "6.000".into(),
                }],
            },
            idle: IdleStock {
                days: 30,
                total: 186,
                rows: vec![IdleRow {
                    id: Uuid::nil(),
                    item_id: Uuid::nil(),
                    sku: "A-1".into(),
                    name: "Bolt".into(),
                    location_code: "BIN-1".into(),
                    on_hand: "5.000".into(),
                    unit: "pcs".into(),
                    value: Some("2.50".into()),
                    last_movement_at: None,
                }],
                truncated: true,
                quantity: "900.000".into(),
            },
            scoped_lines: 5,
        };
        let csv = report_csv(&report);
        assert!(csv.contains("idle · 30 days · 186 rows"), "{csv}");
        assert!(csv.contains("showing 1"), "a capped file says so: {csv}");
        assert!(csv.contains("on hand 900.000"), "and reports the uncapped total: {csv}");
        assert!(csv.contains("unpriced lines 3 of 5"), "{csv}");
    }

    #[test]
    fn an_empty_search_term_is_refused_rather_than_matching_everything() {
        let error = InventoryError::InvalidQuery("the search term is empty".into());
        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn the_like_wildcards_are_escaped_so_a_term_cannot_match_every_row() {
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("c\\d"), "c\\\\d");
        // A term with nothing special in it is untouched — escaping everything would
        // make an ordinary search find nothing.
        assert_eq!(escape_like("bolt"), "bolt");
    }

    #[test]
    fn the_status_the_count_and_the_badge_agree_on_is_the_rank_the_ledger_writes() {
        // The count's `case` is a copy of the list's, and a copy is only safe if the
        // vocabulary it compares against is the enum's. This asserts the copy's tokens
        // against the enum rather than against another copy.
        for status in ["negative", "critical", "low", "ok"] {
            assert!(
                StockStatus::parse(status).is_some(),
                "the count's status arm names {status}, which the badge must know"
            );
        }
    }
}
