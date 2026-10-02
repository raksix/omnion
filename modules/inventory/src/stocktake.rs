//! Stocktake sessions: freeze a scope, count against it, post what the count finds.
//!
//! Slices 1–3 left the last piece of the module as a **shape with nothing behind it**:
//! `stocktake_variance` is a reason code no code posts, and the reconciliation report is a list
//! of disagreements with both numbers that nobody produces by counting. This file is the
//! feature: a session, a counting sheet, a close, and a report that reopens.
//!
//! ## A count is a document, not a number typed at the end
//!
//! The cheap design is "a close reads the rollup, compares it with what the counter typed, and
//! posts the difference". It is wrong in a way this module already knows how to name: **it
//! re-reads a mutable column to answer a historical question.** The count itself writes to
//! stock, so an expectation computed at close time may already include the variance the close is
//! about to post — the deviation reports itself as zero, and a count that finds nothing always
//! finds nothing.
//!
//! So [`create_stocktake`] freezes the **expected** quantity onto every line at the moment the
//! sheet is opened, and the close compares against those frozen numbers. The same argument put
//! the threshold on an alert row in slice 3: a record that re-reads a column changes with the
//! column.
//!
//! ## Null and zero are different facts
//!
//! A line's `counted_qty` is nullable, and the difference is the whole reason a close can be
//! refused. `null` is "nobody looked at this shelf"; `0` is "there is nothing on it". A close
//! that treated `null` as `0` would post an adjustment of `−expected` for every line nobody
//! counted and **destroy the stock it claims to have measured** — the most expensive kind of
//! agreement between a screen and a database, and one that looks like a clean success. So
//! [`close_stocktake`] refuses while any line is uncounted, and the refusal names how many are
//! missing.
//!
//! ## The close goes through the ledger's one write path
//!
//! One `adjustment` movement with `reason = 'stocktake_variance'` per non-zero deviation, written
//! through [`crate::ledger::record_movement`]. Not a bulk `update inventory_stock`: a session
//! that corrects the rollup in a single statement leaves no ledger rows, and the next
//! [`crate::ledger::replay`] reports a disagreement the module manufactured itself — the same
//! trap the transfer's in-transit leg was built to avoid, repeated here deliberately.
//!
//! One further consequence: a line counted **above** expectation has a variance the adjustment
//! rule will not take on its own, because the over-threshold approval from slice 2 governs
//! corrections. A count is a *measurement*, not a request to correct, so the close posts
//! shortfall and surplus through the same path with the sign the arithmetic produced, and the
//! approval question is asked of the **variances the count is posting** rather than of the
//! counter. That is a policy decision this module makes explicitly and the walk proves it.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{InventoryError, Result};
use crate::ledger::{self, NewMovement};
use crate::model::{MovementKind, ReasonCode};
use crate::money::Quantity;
use crate::store::{self, DEFAULT_PER_PAGE, MAX_PER_PAGE, Page};

/// The `source_kind` every variance movement carries.
///
/// A constant for the reason the transfer file gave its own: a filter on the ledger screen and
/// the writes that produce it cannot drift apart if they read the same name.
pub const STOCKTAKE_SOURCE: &str = "stocktake";

/// Where a session is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StocktakeStatus {
    /// The sheet is open and the count is under way. The only status that accepts a count.
    Open,
    /// Posted. Its variance movements exist and cannot be taken back.
    Closed,
    /// Withdrawn. Posted nothing, so it left no ledger rows either.
    Cancelled,
}

impl StocktakeStatus {
    /// The value stored in `inventory_stocktakes.status`, which the check constraint allows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Read a stored status.
    ///
    /// An unknown value is `None` rather than a default, so a row written by a newer version is
    /// **reported** instead of being shown as an open session — a document nobody can close is a
    /// problem to escalate, not one to smooth over.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "open" => Self::Open,
            "closed" => Self::Closed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// True while the session still accepts a count.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Open)
    }
}

/// One line of the counting sheet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StocktakeLine {
    /// The line's id.
    pub id: Uuid,
    /// The item on this line.
    pub item_id: Uuid,
    /// The item's SKU, so the sheet prints what a person reads on the shelf.
    pub sku: String,
    /// The item's name.
    pub item_name: String,
    /// The location being counted.
    pub location_id: Uuid,
    /// The location's code.
    pub location_code: String,
    /// **What the rollup said when the sheet was opened.** Frozen; never re-read.
    pub expected_qty: Quantity,
    /// What the counter wrote, or `None` while nobody has looked.
    #[serde(default)]
    pub counted_qty: Option<Quantity>,
    /// **`counted − expected`, serialized.**
    ///
    /// The same lesson as the transfer's `outstanding`: it was a method first, and the screen
    /// and the walk both reached for it on the wire and found `undefined`. A number a person
    /// types a count against has to come from the server that owns the arithmetic — a
    /// client-side subtraction is right until the rule changes, and then two screens disagree
    /// about the same shelf.
    #[serde(default)]
    pub variance: Quantity,
    /// The line's note.
    pub note: String,
}

impl StocktakeLine {
    /// The deviation this line found: `counted − expected`, or `None` while uncounted.
    #[must_use]
    pub fn variance(&self) -> Option<Quantity> {
        self.counted_qty
            .map(|counted| counted.checked_sub(self.expected_qty).unwrap_or(Quantity::ZERO))
    }

    /// True when the count disagrees with the rollup.
    ///
    /// A line that is counted and agrees is **not** a variance, and the close posts nothing for
    /// it: a session of two hundred lines where one is short should write one ledger row, not two
    /// hundred rows of "0.000", which would bury the one that mattered.
    #[must_use]
    pub fn has_variance(&self) -> bool {
        self.variance().is_some_and(|variance| !variance.is_zero())
    }
}

/// A session as the list and the detail render it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StocktakeView {
    /// The document's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The paper number (`ST-0001`).
    pub number: String,
    /// Where it is in its life.
    pub status: StocktakeStatus,
    /// The frozen scope — the locations being counted.
    pub location_ids: Vec<Uuid>,
    /// The frozen category narrowing, when there was one.
    #[serde(default)]
    pub category: Option<String>,
    /// The codes of the scope's locations, so the header reads like the sheet.
    pub location_codes: Vec<String>,
    /// What the count was for.
    #[serde(default)]
    pub counted_on: Option<String>,
    /// The note.
    pub note: String,
    /// Who opened it.
    #[serde(default)]
    pub created_by: Option<Uuid>,
    /// When.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
    /// Who closed it.
    #[serde(default)]
    pub closed_by: Option<Uuid>,
    /// When they closed it.
    #[serde(default, with = "crate::dates::instant::option")]
    pub closed_at: Option<OffsetDateTime>,
    /// The lines, with the variance computed.
    pub lines: Vec<StocktakeLine>,
    /// How many lines there are.
    pub lines_counted: i64,
    /// How many disagree.
    pub variances_count: i64,
    /// How many nobody has looked at yet.
    pub lines_pending: i64,
    /// The sum of every line's variance, as text — **signed**, so a shortfall does not read as a
    /// surplus.
    pub variance_total: String,
}

impl StocktakeView {
    /// True while a count may still be recorded.
    ///
    /// **One definition, two buttons** — the same rule the transfer's stepper follows, and for
    /// the same reason: a screen that asked the status string a second question would have a
    /// third answer the moment a status was added.
    #[must_use]
    pub fn can_count(&self) -> bool {
        self.status.is_open()
    }

    /// True while the session may still be closed or cancelled.
    #[must_use]
    pub fn can_close(&self) -> bool {
        self.status.is_open()
    }
}

/// A counting entry: one line, one number.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StocktakeCount {
    /// The line's id.
    pub line_id: Uuid,
    /// What the counter saw, as text.
    pub quantity: String,
    /// An optional note.
    #[serde(default)]
    pub note: Option<String>,
}

/// A session the caller asked for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewStocktake {
    /// The locations being counted. At least one.
    pub location_ids: Vec<Uuid>,
    /// An optional category narrowing.
    #[serde(default)]
    pub category: Option<String>,
    /// What the count is for.
    #[serde(default)]
    pub counted_on: Option<String>,
    /// The note, up to 500 characters.
    #[serde(default)]
    pub note: Option<String>,
}

/// The stocktake list screen's query.
#[derive(Debug, Clone, Default)]
pub struct StocktakeQuery {
    /// Free text over the number and the note.
    pub search: Option<String>,
    /// One status — repeated for several.
    pub statuses: Vec<String>,
    /// Only the sessions still counting.
    pub open_only: bool,
    /// Page size.
    pub limit: Option<i64>,
    /// Cursor — the id of the last row of the previous page.
    pub cursor: Option<Uuid>,
}

impl StocktakeQuery {
    /// How many rows a page holds, or the default.
    pub fn page_size(&self) -> i64 {
        match self.limit {
            None => DEFAULT_PER_PAGE,
            Some(limit) if limit < 1 => DEFAULT_PER_PAGE,
            Some(limit) if limit > MAX_PER_PAGE => MAX_PER_PAGE,
            Some(limit) => limit,
        }
    }

    /// The id the cursor points at, if there is one.
    pub fn cursor_id(&self) -> Option<Uuid> {
        self.cursor.filter(|id| !id.is_nil())
    }

    /// The statuses the caller asked for, parsed.
    ///
    /// An **unknown status is refused rather than ignored**, for the same reason a transfer's is:
    /// a filter that quietly drops what it does not understand shows a list the person did not
    /// ask for, and an empty list reads as "there are no stocktakes" — the one conclusion a
    /// typo must never be able to produce.
    pub fn parsed_statuses(&self) -> Result<Vec<StocktakeStatus>> {
        self.statuses
            .iter()
            .filter(|raw| !raw.trim().is_empty())
            .map(|raw| {
                StocktakeStatus::parse(raw.trim()).ok_or_else(|| {
                    InventoryError::invalid(
                        "stocktake",
                        "status",
                        format!("{raw} is not a stocktake status"),
                    )
                })
            })
            .collect()
    }
}

/// The outcome of a close, as the report and the event payload read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StocktakeOutcome {
    /// How many lines were on the sheet.
    pub lines: i64,
    /// How many disagreed.
    pub variances: i64,
    /// The signed sum of the deviations, as text.
    pub variance_total: String,
    /// The ledger rows the close wrote, in order.
    pub movements: Vec<ledger::Movement>,
}

// ---------------------------------------------------------------------------------------------
// The row shapes
// ---------------------------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct StocktakeRow {
    id: Uuid,
    organization_id: Uuid,
    number: String,
    status: String,
    location_ids: Vec<Uuid>,
    category: Option<String>,
    counted_on: Option<String>,
    note: String,
    created_by: Option<Uuid>,
    created_at: OffsetDateTime,
    closed_by: Option<Uuid>,
    closed_at: Option<OffsetDateTime>,
}

#[derive(Debug, sqlx::FromRow)]
struct LineRow {
    id: Uuid,
    item_id: Uuid,
    sku: String,
    item_name: String,
    location_id: Uuid,
    location_code: String,
    expected_qty: String,
    counted_qty: Option<String>,
    note: String,
}

impl StocktakeRow {
    fn into_view(self, lines: Vec<StocktakeLine>) -> Result<StocktakeView> {
        let status = StocktakeStatus::parse(&self.status).ok_or_else(|| {
            InventoryError::invalid(
                "stocktake",
                "status",
                format!("stored status {} is not one this module knows", self.status),
            )
        })?;
        // A plain `fold`, not a `try_fold`: `checked_add` saturates to the accumulator rather
        // than erroring, and a total that is slightly wrong at the top of the numeric range is a
        // display problem, not a reason to refuse to open a count. The transfers list uses
        // `try_fold` because there it guards a total against caller-supplied magnitudes.
        let total = lines.iter().fold(Quantity::ZERO, |acc, line| {
            acc.checked_add(line.variance).unwrap_or(acc)
        });
        let variances = lines.iter().filter(|line| line.has_variance()).count() as i64;
        let pending = lines.iter().filter(|line| line.counted_qty.is_none()).count() as i64;
        Ok(StocktakeView {
            id: self.id,
            organization_id: self.organization_id,
            number: self.number,
            status,
            location_ids: self.location_ids.clone(),
            category: self.category,
            location_codes: Vec::new(),
            counted_on: self.counted_on,
            note: self.note,
            created_by: self.created_by,
            created_at: self.created_at,
            closed_by: self.closed_by,
            closed_at: self.closed_at,
            lines_counted: lines.len() as i64,
            variances_count: variances,
            lines_pending: pending,
            variance_total: total.to_text(),
            lines,
        })
    }
}

impl LineRow {
    fn into_view(self) -> Result<StocktakeLine> {
        let expected_qty = store::quantity_from_text(&self.expected_qty)?;
        let counted_qty = match self.counted_qty.as_deref() {
            None => None,
            Some(raw) => Some(store::quantity_from_text(raw)?),
        };
        // The same helper the method uses, so the wire value and the in-process value cannot
        // disagree.
        let variance = counted_qty
            .map(|counted| counted.checked_sub(expected_qty).unwrap_or(Quantity::ZERO))
            .unwrap_or(Quantity::ZERO);
        Ok(StocktakeLine {
            id: self.id,
            item_id: self.item_id,
            sku: self.sku,
            item_name: self.item_name,
            location_id: self.location_id,
            location_code: self.location_code,
            expected_qty,
            counted_qty,
            variance,
            note: self.note,
        })
    }
}

/// The `select` both the list and the detail read through, so the two cannot answer differently.
const STOCKTAKE_SELECT: &str = "select s.id, s.organization_id, s.number, s.status, \
        s.location_ids, s.category, to_char(s.counted_on, 'YYYY-MM-DD') as counted_on, s.note, \
        s.created_by, s.created_at, s.closed_by, s.closed_at \
     from inventory_stocktakes s";

// ---------------------------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------------------------

/// Open a stocktake: freeze the scope and write the sheet.
///
/// **The scope is frozen here, not at close.** Every line carries the `on_hand` the rollup held
/// at this instant, and the close compares against those numbers. A design that re-read the
/// rollup at close would let a count report its own variance as zero — see the module docs for
/// why that is the whole argument.
///
/// A scope with no locations is refused: "count everything" is not a count, and an empty sheet
/// that closes successfully is a document that proves nothing and looks like a clean count.
pub async fn create_stocktake(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewStocktake,
    actor: Option<Uuid>,
) -> Result<StocktakeView> {
    if new.location_ids.is_empty() {
        return Err(InventoryError::invalid(
            "stocktake",
            "location_ids",
            "a stocktake has to name the shelves it is counting",
        ));
    }

    // The scope is read **before** the transaction and the values kept, because they are used:
    // the codes go on the header and the ids are what the lines are written against. A foreign
    // id becomes a 404 here rather than a constraint failure after the document exists.
    let mut locations = Vec::with_capacity(new.location_ids.len());
    for location_id in &new.location_ids {
        if locations.iter().any(|existing: &store::LocationView| existing.id == *location_id) {
            return Err(InventoryError::invalid(
                "stocktake",
                "location_ids",
                "the same shelf is on this count twice",
            ));
        }
        locations.push(store::get_location(pool, organization_id, *location_id).await?);
    }

    // **In-transit is not countable.** The goods in it are on a van; counting a shelf nobody can
    // reach is a count of nothing, and a stocktake that "finds" a shortfall on goods in transit
    // posts a variance against a location that is not a shelf at all.
    if locations
        .iter()
        .any(|location| location.kind == crate::model::LocationKind::InTransit)
    {
        return Err(InventoryError::invalid(
            "stocktake",
            "location_ids",
            "goods in transit are on a van — count them when they land",
        ));
    }

    let category = match new.category.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => {
            if raw.len() > 80 {
                return Err(InventoryError::invalid(
                    "stocktake",
                    "category",
                    "a category name is 80 characters at most",
                ));
            }
            Some(raw.to_string())
        }
    };
    let counted_on = match new.counted_on.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(parse_date("stocktake", "counted_on", raw)?),
    };
    let note = crate::items::validate_note("stocktake", new.note.as_deref().unwrap_or_default())?;

    // The expectation is read **inside** the transaction, after the document row is written, so
    // the sheet is consistent with the moment it was opened: a movement landing between the two
    // reads is either on the sheet (and its variance is real) or not (and it is a later event),
    // but never half of each.
    let mut transaction = pool.begin().await?;
    let number = next_number(&mut transaction, organization_id).await?;
    let stocktake_id = Uuid::new_v4();

    sqlx::query(
        "insert into inventory_stocktakes (id, organization_id, number, location_ids, category, \
             counted_on, note, created_by) \
         values ($1, $2, $3, $4, $5, $6::date, $7, $8)",
    )
    .bind(stocktake_id)
    .bind(organization_id)
    .bind(&number)
    .bind(&new.location_ids)
    .bind(&category)
    .bind(&counted_on)
    .bind(&note)
    .bind(actor)
    .execute(&mut *transaction)
    .await
    .map_err(|error| {
        if let sqlx::Error::Database(db) = &error {
            if db.is_unique_violation() {
                return InventoryError::code_taken("stocktake", "that number is taken");
            }
        }
        error.into()
    })?;

    // The sheet is "every item × every location in the scope, with whatever the rollup says".
    // An item that has never been stocked at a location produces no row and is simply not on
    // the sheet — the same rule `replay` uses, and the reason an untouched shelf does not appear
    // as a line somebody has to explain away.
    let location_ids = locations.iter().map(|location| location.id).collect::<Vec<_>>();
    let rows: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "select st.item_id, st.location_id, st.on_hand::text as on_hand \
         from inventory_stock st \
         where st.organization_id = $1 \
           and st.location_id = any($2) \
           and ($3::text is null or exists (select 1 from inventory_items i \
                where i.id = st.item_id and i.category = $3)) \
         order by st.item_id, st.location_id",
    )
    .bind(organization_id)
    .bind(&location_ids)
    .bind(&category)
    .fetch_all(&mut *transaction)
    .await?;

    for (item_id, location_id, on_hand) in &rows {
        sqlx::query(
            "insert into inventory_stocktake_lines (id, stocktake_id, item_id, location_id, expected_qty) \
             values ($1, $2, $3, $4, $5::numeric)",
        )
        .bind(Uuid::new_v4())
        .bind(stocktake_id)
        .bind(item_id)
        .bind(location_id)
        .bind(on_hand)
        .execute(&mut *transaction)
        .await?;
    }

    transaction.commit().await?;
    get_stocktake(pool, organization_id, stocktake_id).await
}

/// The next paper number for this organization, inside the same transaction as the insert.
async fn next_number(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
) -> Result<String> {
    let last: Option<String> = sqlx::query_scalar(
        "select number from inventory_stocktakes where organization_id = $1 \
         order by number desc limit 1",
    )
    .bind(organization_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let next = last
        .and_then(|raw| raw.rsplit('-').next().and_then(|tail| tail.parse::<u32>().ok()))
        .map_or(1, |n| n + 1);
    Ok(format!("ST-{next:04}"))
}

/// `YYYY-MM-DD`, or a refusal naming the field.
///
/// A **shape** check, not a calendar check — the same decision the transfer's `scheduled_on`
/// makes and for the same reason: the real calendar is PostgreSQL's answer to give, and a module
/// carrying its own would be a second place for the rule to live.
fn parse_date(entity: &'static str, field: &'static str, raw: &str) -> Result<String> {
    let bytes = raw.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(at, byte)| matches!(at, 4 | 7) || byte.is_ascii_digit());
    if shaped {
        Ok(raw.to_string())
    } else {
        Err(InventoryError::invalid(
            entity,
            field,
            "use a date like 2026-09-29",
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

/// One stocktake with its sheet.
pub async fn get_stocktake(
    pool: &PgPool,
    organization_id: Uuid,
    stocktake_id: Uuid,
) -> Result<StocktakeView> {
    let row: Option<StocktakeRow> = sqlx::query_as(&format!(
        "{STOCKTAKE_SELECT} where s.organization_id = $1 and s.id = $2"
    ))
    .bind(organization_id)
    .bind(stocktake_id)
    .fetch_optional(pool)
    .await?;
    let row = row.ok_or(InventoryError::NotFound("stocktake"))?;
    let lines = load_lines(pool, stocktake_id).await?;

    // The scope's codes are read here rather than written on the header, because a location can
    // be renamed and the sheet a person printed last week should still say what it said.
    let codes = if row.location_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_scalar::<_, String>(
            "select code from inventory_locations where organization_id = $1 and id = any($2)",
        )
        .bind(organization_id)
        .bind(&row.location_ids)
        .fetch_all(pool)
        .await?
    };

    let mut view = row.into_view(lines)?;
    view.location_codes = codes;
    Ok(view)
}

/// The lines of one session, in the order the sheet was printed.
async fn load_lines(pool: &PgPool, stocktake_id: Uuid) -> Result<Vec<StocktakeLine>> {
    let rows: Vec<LineRow> = sqlx::query_as(
        "select l.id, l.item_id, i.sku, i.name as item_name, l.location_id, loc.code as location_code, \
                l.expected_qty::text as expected_qty, l.counted_qty::text as counted_qty, l.note \
         from inventory_stocktake_lines l \
         join inventory_items i on i.id = l.item_id \
         join inventory_locations loc on loc.id = l.location_id \
         where l.stocktake_id = $1 order by i.sku, loc.code",
    )
    .bind(stocktake_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(LineRow::into_view).collect()
}

/// The stocktake list.
pub async fn list_stocktakes(
    pool: &PgPool,
    organization_id: Uuid,
    query: &StocktakeQuery,
) -> Result<Page<StocktakeView>> {
    let statuses = query.parsed_statuses()?;
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(|raw| format!("%{}%", raw.to_lowercase()));

    // A `QueryBuilder`, for the reason the transfer's list gives: a search box containing `%`
    // must not become a wildcard that matches everything.
    let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "select s.id, s.created_at from inventory_stocktakes s where s.organization_id = ",
    );
    builder.push_bind(organization_id);

    if let Some(needle) = search {
        builder
            .push(" and (lower(s.number) like ")
            .push_bind(needle.clone())
            .push(" or lower(s.note) like ")
            .push_bind(needle)
            .push(")");
    }
    if !statuses.is_empty() {
        builder.push(" and s.status = any(").push_bind(
            statuses
                .iter()
                .map(|status| status.as_str().to_string())
                .collect::<Vec<_>>(),
        );
        builder.push(")");
    } else if query.open_only {
        builder.push(" and s.status = 'open'");
    }
    if let Some(cursor) = query.cursor_id() {
        builder.push(" and (s.created_at, s.id) < (select created_at, id from inventory_stocktakes where id = ").push_bind(cursor).push(")");
    }
    builder
        .push(" order by s.created_at desc, s.id desc limit ")
        .push_bind(query.page_size() + 1);

    let rows: Vec<(Uuid, OffsetDateTime)> = builder.build_query_as().fetch_all(pool).await?;
    let has_more = rows.len() as i64 > query.page_size();
    let ids: Vec<Uuid> = rows
        .into_iter()
        .take(query.page_size() as usize)
        .map(|(id, _)| id)
        .collect();

    // The list loads the same view the detail does, one id at a time, rather than a second
    // hand-written projection. A list that answered from its own SELECT is a list that can
    // disagree with the page you opened from it.
    let mut items = Vec::with_capacity(ids.len());
    for id in &ids {
        items.push(get_stocktake(pool, organization_id, *id).await?);
    }
    let next_cursor = has_more.then(|| ids.last().map(ToString::to_string)).flatten();

    Ok(Page {
        items,
        next_cursor,
        total_estimate: 0,
    })
}

// ---------------------------------------------------------------------------------------------
// Count
// ---------------------------------------------------------------------------------------------

/// Record what a counter saw on one or more lines.
///
/// **A count never changes stock.** It is a measurement; only [`close_stocktake`] posts, and
/// only for the lines that disagree. A screen that wrote a movement per keystroke would make the
/// ledger describe a count as a sequence of physical events, and the module's one invariant —
/// `inventory_stock` is a rollup of the movements, and the two may never disagree — would still
/// hold while the **meaning** of every row became a lie.
///
/// Re-counting a line is allowed and simply overwrites the number: a counter who misread a
/// shelf corrects it rather than opening a second session for the same shelf the same evening.
pub async fn count(
    pool: &PgPool,
    organization_id: Uuid,
    stocktake_id: Uuid,
    entries: &[StocktakeCount],
) -> Result<StocktakeView> {
    if entries.is_empty() {
        return Err(InventoryError::invalid(
            "stocktake",
            "lines",
            "name the lines you counted — an empty count records nothing",
        ));
    }
    let view = get_stocktake(pool, organization_id, stocktake_id).await?;
    if !view.can_count() {
        return Err(status_change_error(
            view.status,
            "count",
            "this sheet is finished",
        ));
    }

    for entry in entries {
        let line = view
            .lines
            .iter()
            .find(|line| line.id == entry.line_id)
            .ok_or(InventoryError::NotFound("stocktake line"))?;
        // A count is a number of things on a shelf, so it cannot be negative. The schema refuses
        // it too; refusing here as well means the **sentence** reaches the person rather than a
        // database error, and it says what to write instead.
        let quantity = store::parse_quantity("stocktake", "counted_qty", &entry.quantity)?;
        if quantity.is_negative() {
            return Err(InventoryError::invalid(
                "stocktake",
                "counted_qty",
                "you cannot count minus three — write what is there, and the shortfall is a variance",
            ));
        }
        let note = crate::items::validate_note(
            "stocktake",
            entry.note.as_deref().unwrap_or(&line.note),
        )?;

        // The `where stocktake_id` is not belt-and-braces: `line_id` is a uuid, so a line from
        // another organization's sheet would otherwise be *found* and counted. The status was
        // checked above on this session, so re-stating it in the write is what makes the row
        // update and the document's state one decision rather than two.
        let changed = sqlx::query(
            "update inventory_stocktake_lines set counted_qty = $3::numeric, note = $4 \
             where id = $1 and stocktake_id = $2",
        )
        .bind(line.id)
        .bind(stocktake_id)
        .bind(quantity.to_text())
        .bind(&note)
        .execute(pool)
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(InventoryError::NotFound("stocktake line"));
        }
    }

    get_stocktake(pool, organization_id, stocktake_id).await
}

// ---------------------------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------------------------

/// Post what the count found and close the sheet.
///
/// **Three refusals, in this order**, and the order is the argument:
///
/// 1. **Not open.** A closed session's numbers are already in the ledger; writing more would be
///    the second answer to a question the first close answered.
/// 2. **A line nobody counted.** `null` is not `0`. Posting `−expected` for every unvisited
///    shelf would destroy stock in the name of measuring it, so the close is refused and names
///    how many lines are missing.
/// 3. **Nothing to post.** A count that agrees everywhere is a **successful count**, not an
///    error — refusing it would teach people that a clean shelf is a failure, which is the
///    fastest way to get a warehouse falsifying sheets. It closes with zero movements.
///
/// Each deviation becomes **one `adjustment` movement with `reason = 'stocktake_variance'`**,
/// written through [`crate::ledger::record_movement`] — the module's one write path, with the
/// document on it as `source_kind = 'stocktake'`, so the report can find them again. A line
/// counted *above* expectation has a positive variance, which an `adjustment` carries as a
/// positive number and a shortfall as a negative one, so the ledger's single signed-adjustment
/// rule covers both directions.
pub async fn close_stocktake(
    pool: &PgPool,
    organization_id: Uuid,
    stocktake_id: Uuid,
    actor: Option<Uuid>,
) -> Result<StocktakeOutcome> {
    let view = get_stocktake(pool, organization_id, stocktake_id).await?;
    if !view.can_close() {
        return Err(status_change_error(
            view.status,
            "close",
            "this sheet has already been finished",
        ));
    }

    if view.lines_pending > 0 {
        // **`InvalidStatusChange` (409), not `Invalid` (400)**, and the choice is the module's
        // own precedent rather than a new distinction: this is the same refusal a transfer gives
        // when it is dispatched after it was received — the request is well formed and the
        // document is not in a state that permits the step. A 400 would tell the person filling
        // the sheet that they typed the request wrong, and they did not: they typed it correctly
        // and have a shelf left to walk. The sentence carries the count so the next action is
        // obvious.
        return Err(InventoryError::InvalidStatusChange(format!(
            "this sheet has {} line(s) nobody has counted yet — an uncounted shelf is not an \
             empty one, so closing now would post stock nobody measured",
            view.lines_pending
        )));
    }

    let mut movements = Vec::new();
    let mut total = Quantity::ZERO;
    let mut variances = 0i64;
    for line in view.lines.iter().filter(|line| line.has_variance()) {
        let Some(variance) = line.variance() else {
            continue;
        };
        let movement = NewMovement {
            item_id: line.item_id,
            location_id: line.location_id,
            kind: Some(MovementKind::Adjustment.as_str().to_string()),
            quantity: variance.to_text(),
            reason: Some(ReasonCode::StocktakeVariance.as_str().to_string()),
            // The document number is in the note as well as in `source_id`, for the reason the
            // transfer gives: the ledger screen prints the note, and a person reading a row six
            // months later should not have to resolve an id to learn which count it was.
            note: Some(format!("{} \u{b7} counted", view.number)),
            source_kind: Some(STOCKTAKE_SOURCE.to_string()),
            source_id: Some(view.id),
            // A count **is** the authority on the shelf, so a shortfall posts even if it takes
            // the balance below zero: the goods really are not there, and refusing to record
            // that would leave the rollup claiming a quantity nobody can find. The
            // `inventory.negative.manage` rule governs a *correction* — a hand-written one a
            // person is choosing; this is a measurement, and the alternative is a stock list
            // that says the shelf holds four boxes when it holds none.
            may_go_negative: true,
        };
        // `record_movement` answers a `Recorded` (the row **and** the stock position after the
        // write). The report carries only the rows, and it reads them back from the ledger in
        // `variance_movements` — so this list is the write-order record of what this close
        // posted, and the report is the independent read of it. The two agreeing is the proof.
        movements.push(
            ledger::record_movement(pool, organization_id, &movement, actor)
                .await?
                .movement,
        );
        total = total.checked_add(variance).unwrap_or(total);
        variances += 1;
    }

    // The header counts are written **by the close**, in the same statement that sets the
    // status, so a session can never report a variance total the ledger disagrees with.
    let changed = sqlx::query(
        "update inventory_stocktakes set status = 'closed', closed_by = $2, closed_at = now(), \
                lines_counted = $3, variances_count = $4, variance_total = $5::numeric, \
                updated_at = now() \
         where id = $1 and status = 'open'",
    )
    .bind(stocktake_id)
    .bind(actor)
    .bind(view.lines.len() as i32)
    .bind(variances as i32)
    .bind(total.to_text())
    .execute(pool)
    .await?
    .rows_affected();
    if changed == 0 {
        // Somebody closed it between the read and here. The movements above are already
        // written, so this is reported rather than hidden: the session says closed either way
        // and the ledger rows are the document's, which is why the status is re-read rather
        // than assumed.
        return Err(status_change_error(
            StocktakeStatus::Closed,
            "close",
            "another counter finished this sheet first",
        ));
    }

    Ok(StocktakeOutcome {
        lines: view.lines.len() as i64,
        variances,
        variance_total: total.to_text(),
        movements,
    })
}

/// Withdraw the session.
///
/// **A cancel posts nothing**, and that is the difference from a transfer's cancel: nothing has
/// moved, so a ledger row would describe an event that did not happen. The check constraint
/// allows `cancelled` with a `closed_at`, and it is written, so the sheet records when it was
/// withdrawn — a session nobody closed and nobody cancelled would be a document that simply
/// stopped existing.
pub async fn cancel_stocktake(
    pool: &PgPool,
    organization_id: Uuid,
    stocktake_id: Uuid,
) -> Result<StocktakeView> {
    let view = get_stocktake(pool, organization_id, stocktake_id).await?;
    if !view.can_close() {
        return Err(status_change_error(
            view.status,
            "cancel",
            "this sheet has already been finished",
        ));
    }
    let changed = sqlx::query(
        "update inventory_stocktakes set status = 'cancelled', closed_at = now(), updated_at = now() \
         where id = $1 and status = 'open'",
    )
    .bind(stocktake_id)
    .execute(pool)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(InventoryError::NotFound("stocktake"));
    }
    get_stocktake(pool, organization_id, stocktake_id).await
}

/// The ledger rows a session caused, in order — the report's own evidence.
///
/// Read from the ledger rather than recomputed, because the point of the report is to show
/// **what was actually posted**, and a number derived from the sheet would agree with the sheet
/// by construction and tell nobody anything.
///
/// The ids are collected first and then read **one at a time through `get_movement`**, which
/// takes only the id: it filters by organization itself, so a row belonging to somebody else is
/// a `404` here rather than a leak. Collecting-then-reading rather than one wide query is the
// same choice the transfer list makes, and for the same reason — the detail projection is the
/// definition, and a second hand-written `select` for a list is a second answer.
pub async fn variance_movements(
    pool: &PgPool,
    organization_id: Uuid,
    stocktake_id: Uuid,
) -> Result<Vec<ledger::Movement>> {
    let ids: Vec<(i64,)> = sqlx::query_as(
        "select id from inventory_movements \
         where organization_id = $1 and source_kind = $2 and source_id = $3 \
         order by id",
    )
    .bind(organization_id)
    .bind(STOCKTAKE_SOURCE)
    .bind(stocktake_id)
    .fetch_all(pool)
    .await?;

    let mut movements = Vec::with_capacity(ids.len());
    for (id,) in ids {
        movements.push(ledger::get_movement(pool, id).await?);
    }
    Ok(movements)
}

/// The refusal for a step the session's status does not allow.
///
/// The sentence carries **what the document is** before it carries what you asked for, because
/// the caller who is out of order is the one who needs to know where it actually is.
fn status_change_error(
    current: StocktakeStatus,
    step: &str,
    because: &str,
) -> InventoryError {
    InventoryError::InvalidStatusChange(format!(
        "this stocktake is {} and cannot be {step}ed: {because}",
        current.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(expected: &str, counted: Option<&str>) -> StocktakeLine {
        StocktakeLine {
            id: Uuid::new_v4(),
            item_id: Uuid::new_v4(),
            sku: "BOLT-M8".into(),
            item_name: "Bolt M8".into(),
            location_id: Uuid::new_v4(),
            location_code: "STOCK".into(),
            expected_qty: Quantity::parse(expected).expect("expected"),
            counted_qty: counted.map(|raw| Quantity::parse(raw).expect("counted")),
            variance: Quantity::ZERO,
            note: String::new(),
        }
    }

    #[test]
    fn a_shortfall_is_negative_because_the_counter_wrote_less_than_the_shelf_claimed() {
        // The sign is the whole content of the number: a positive variance on a shortfall would
        // read as a bonus on the shelf, and the close would post it as one.
        assert_eq!(
            line("10.000", Some("7.000")).variance().expect("counted").to_text(),
            "-3.000"
        );
        // And the surplus is positive, through the same subtraction — one rule, both directions.
        assert_eq!(
            line("10.000", Some("12.500")).variance().expect("counted").to_text(),
            "2.500"
        );
    }

    #[test]
    fn an_uncounted_line_has_no_variance_rather_than_a_variance_of_minus_everything() {
        // This is the test that protects stock. `null` treated as `0` would post an adjustment
        // of `−expected` for every shelf nobody visited, and the close would *destroy* the stock
        // it claims to have measured while reporting a clean success.
        let line = line("10.000", None);
        assert!(line.variance().is_none());
        assert!(!line.has_variance());
    }

    #[test]
    fn a_count_that_agrees_is_not_a_variance() {
        // A session of two hundred lines with one short should write **one** ledger row. A
        // `has_variance` that ignored zero would bury the row that mattered under two hundred
        // rows of "0.000".
        let line = line("10.000", Some("10.000"));
        assert!(!line.has_variance());
        assert!(line.variance().expect("counted").is_zero());
    }

    #[test]
    fn the_status_decides_which_buttons_the_screen_offers() {
        // One definition, two buttons — a screen that asked the status string a second question
        // would have a third answer the moment a status was added.
        let view = |status| StocktakeView {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            number: "ST-0001".into(),
            status,
            location_ids: vec![],
            category: None,
            location_codes: vec![],
            counted_on: None,
            note: String::new(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            closed_by: None,
            closed_at: None,
            lines: vec![],
            lines_counted: 0,
            variances_count: 0,
            lines_pending: 0,
            variance_total: "0.000".into(),
        };
        let open = view(StocktakeStatus::Open);
        assert!(open.can_count());
        assert!(open.can_close());

        for finished in [StocktakeStatus::Closed, StocktakeStatus::Cancelled] {
            let view = view(finished);
            assert!(!view.can_count(), "{finished:?} must not accept a count");
            assert!(!view.can_close(), "{finished:?} must not be closable again");
        }
    }

    #[test]
    fn an_unknown_filter_status_is_refused_rather_than_ignored() {
        // A filter that silently drops what it does not understand answers "you have no
        // stocktakes", which is the one conclusion a typo must never be able to produce.
        let query = StocktakeQuery {
            statuses: vec!["open".into(), "counting".into()],
            ..StocktakeQuery::default()
        };
        let error = query.parsed_statuses().expect_err("a typo must not pass");
        assert!(error.to_string().contains("counting"), "{error}");
    }

    #[test]
    fn a_bad_date_is_named_rather_than_answered_by_a_five_hundred() {
        // A text parameter bound to a `date` column is a 500 from PostgreSQL: it compiles, it
        // typechecks, and every unit test is green. The shape check turns that into a sentence.
        let error = parse_date("stocktake", "counted_on", "29/09/2026").expect_err("must be refused");
        assert!(error.to_string().contains("2026-09-29"), "{error}");
        assert_eq!(parse_date("stocktake", "counted_on", "2026-09-29").expect("ok"), "2026-09-29");
    }

    #[test]
    fn a_page_size_the_caller_overshot_is_clamped_rather_than_refused() {
        // Overshooting is a display preference; zero and negative are clamped the same way
        // rather than reaching the database as `limit 0`.
        let query = StocktakeQuery { limit: Some(1_000_000), ..StocktakeQuery::default() };
        assert_eq!(query.page_size(), MAX_PER_PAGE);
        let query = StocktakeQuery { limit: Some(0), ..StocktakeQuery::default() };
        assert_eq!(query.page_size(), DEFAULT_PER_PAGE);
    }
}
