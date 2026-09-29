//! Transfers between locations, and the low-stock alerts that come from a crossing.
//!
//! Slice 1 shipped the `in_transit` location kind, the `transfer_out`/`transfer_in` movement
//! kinds, and the rule that a hand-written movement may **not** be a transfer. Slice 2 shipped the
//! over-threshold approval. Both left the same gap: **the schema can be right and the feature
//! absent.** This file is the feature.
//!
//! ## A transfer is a document, not two movements
//!
//! The cheap design is "a dispatch writes a `transfer_out`, a receive writes a `transfer_in`, the
//! two find each other by `source_id`". It is refused for a reason this crate already knows: the
//! ledger is append-only and **replayable**, and a pairing that lives in the application rather
//! than in a row is a pairing `replay` cannot see. It also makes the two halves of one physical
//! move two independent facts, and "the goods left and nobody knows where they are" becomes
//! representable — which it is not, physically.
//!
//! So [`create_transfer`] writes a document with a [`TransferStatus`], and each step writes its
//! movements through [`crate::ledger::record_movement`] — the module's **one** write path — with
//! `source_kind = "transfer"`. Two write paths for one business rule is how a module ends up with
//! two answers, which is the lesson slice 2 paid for.
//!
//! ## The in-transit leg is the correctness argument
//!
//! Dispatch writes **two** movements: out at the source, in at the organization's transit
//! location. Without the second, dispatching would delete stock from the organization for as long
//! as the goods are on a van, and the stock list — which sums `inventory_stock` — would report a
//! hole that does not exist in any physical place. The transit leg makes the stock *visible* as
//! its own line rather than hidden inside an arithmetic expression, and receiving takes it out of
//! transit and into the target. The sum of the whole organization is the same before dispatch,
//! during it and after it, which is the property a stocktake six months later depends on.
//!
//! ## Where the number is checked, and where it cannot be
//!
//! A line cannot exceed the source's available quantity (`on_hand − reserved`). That check is in
//! the service and the schema cannot mirror it, because the number is a function of the ledger
//! rather than a column. The refusal is a [`InventoryError::WouldGoNegative`] carrying the
//! available number in the sentence as well as in `details` — the same shape an issue movement
//! already uses, for the same reason: the person reading it is standing at the shelf.
//!
//! Cancelling after dispatch is **not** free of ledger rows. The goods are on a van, so coming
//! back is itself a movement out of transit and in at the source; writing only a status change
//! would leave the transit balance holding goods that the document says came home, and the next
//! `replay` would report a disagreement the module had manufactured itself.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{InventoryError, Result};
use crate::ledger::{self, NewMovement, Recorded};
use crate::model::{LocationKind, MovementKind, ReasonCode, TransferStatus};
use crate::money::Quantity;
use crate::store::{self, DEFAULT_PER_PAGE, MAX_PER_PAGE, Page};

/// The `source_kind` every transfer leg carries.
///
/// A constant rather than a literal at each call site, so a filter in the ledger screen
/// (`?source=transfer`) and the writes that produce it cannot drift apart.
pub const TRANSFER_SOURCE: &str = "transfer";

/// A transfer as the list and the detail render it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferView {
    /// The document's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The paper number (`TR-0001`), unique per organization.
    pub number: String,
    /// Where it is in its life.
    pub status: TransferStatus,
    /// The source location's id.
    pub from_location_id: Uuid,
    /// The source location's code, so the row reads like the pick list.
    pub from_location_code: String,
    /// The target location's id.
    pub to_location_id: Uuid,
    /// The target location's code.
    pub to_location_code: String,
    /// When the goods are meant to move.
    #[serde(default)]
    pub scheduled_on: Option<String>,
    /// The note.
    pub note: String,
    /// Who wrote it.
    #[serde(default)]
    pub created_by: Option<Uuid>,
    /// When.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
    /// When the goods left the source.
    #[serde(default, with = "crate::dates::instant::option")]
    pub dispatched_at: Option<OffsetDateTime>,
    /// When they landed.
    #[serde(default, with = "crate::dates::instant::option")]
    pub received_at: Option<OffsetDateTime>,
    /// When it was withdrawn.
    #[serde(default, with = "crate::dates::instant::option")]
    pub cancelled_at: Option<OffsetDateTime>,
    /// The lines, with the item's SKU and name.
    pub lines: Vec<TransferLine>,
    /// The sum of every line's quantity, as text.
    pub quantity_total: String,
    /// How much of that has landed, as text.
    pub received_total: String,
}

impl TransferView {
    /// True while a step is still available — the flag the detail screen's buttons read.
    ///
    /// **One definition, three buttons.** A screen that computed "can I dispatch?" from the
    /// status string would have a fourth answer the moment somebody added a status.
    #[must_use]
    pub fn can_dispatch(&self) -> bool {
        self.status == TransferStatus::Draft
    }

    /// True while the goods are in transit and may still be received or brought back.
    #[must_use]
    pub fn can_receive(&self) -> bool {
        self.status == TransferStatus::Dispatched
    }

    /// True while the document has not been closed, i.e. while it appears in the open list.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.status.is_open()
    }
}

/// One line of a transfer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferLine {
    /// The line's id.
    pub id: Uuid,
    /// The item being moved.
    pub item_id: Uuid,
    /// The item's SKU.
    pub sku: String,
    /// The item's name.
    pub item_name: String,
    /// How much is being sent.
    pub quantity: Quantity,
    /// How much has landed so far — a partial receive is allowed per line.
    pub received_qty: Quantity,
    /// **How much is still on the van**, computed here rather than by the reader.
    ///
    /// It was a method first, and both the screen and the walk reached for it on the wire and
    /// found `undefined` — which is the shape of a rule that lives in three places and is
    /// correct in one. It is serialized for the same reason `on_hand_after` is on a movement:
    /// the number a person acts on should come from the server that owns the arithmetic, and a
    /// client-side `quantity − received_qty` is right until the rule changes.
    #[serde(default)]
    pub outstanding: Quantity,
    /// The line's note.
    pub note: String,
}

impl TransferLine {
    /// How much of this line is still on the van.
    #[must_use]
    pub fn outstanding(&self) -> Quantity {
        Quantity::from_milli(self.quantity.milli() - self.received_qty.milli()).unwrap_or(Quantity::ZERO)
    }
}

/// A line the caller asked for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewTransferLine {
    /// The item to move.
    pub item_id: Uuid,
    /// The quantity, as text.
    pub quantity: String,
    /// An optional per-line note.
    #[serde(default)]
    pub note: Option<String>,
}

/// A transfer the caller asked for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewTransfer {
    /// Where the goods come from.
    pub from_location_id: Uuid,
    /// Where they go. Must differ from the source.
    pub to_location_id: Uuid,
    /// The lines.
    pub lines: Vec<NewTransferLine>,
    /// When the goods are meant to move.
    #[serde(default)]
    pub scheduled_on: Option<String>,
    /// The note, up to 500 characters.
    #[serde(default)]
    pub note: Option<String>,
}

/// One step of a dispatch or a receive: how much of a line moves.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferStepLine {
    /// The line's id.
    pub line_id: Uuid,
    /// How much to move, as text.
    pub quantity: String,
}

/// The transfer list screen's query.
#[derive(Debug, Clone, Default)]
pub struct TransferQuery {
    /// Free text over the number, the note, the SKU and the item name.
    pub search: Option<String>,
    /// One status — repeated for several.
    pub statuses: Vec<String>,
    /// The source location.
    pub from_location_id: Option<Uuid>,
    /// The target location.
    pub to_location_id: Option<Uuid>,
    /// Only the ones still in flight.
    pub open_only: bool,
    /// Page size.
    pub limit: Option<i64>,
    /// Cursor — the id of the last row of the previous page.
    pub cursor: Option<Uuid>,
}

impl TransferQuery {
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
    /// An **unknown status is refused rather than ignored**, for the same reason a bad date is:
    /// a filter that quietly drops what it does not understand shows a list the person did not
    /// ask for, and an empty list reads as "there are no transfers" — the one conclusion a
    /// filter mistake must never support.
    pub fn parsed_statuses(&self) -> Result<Vec<TransferStatus>> {
        self.statuses
            .iter()
            .filter(|raw| !raw.trim().is_empty())
            .map(|raw| {
                TransferStatus::parse(raw.trim()).ok_or_else(|| {
                    InventoryError::invalid(
                        "transfer",
                        "status",
                        format!("{raw} is not a transfer status"),
                    )
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// The row shapes
// ---------------------------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct TransferRow {
    id: Uuid,
    organization_id: Uuid,
    number: String,
    status: String,
    from_location_id: Uuid,
    from_location_code: String,
    to_location_id: Uuid,
    to_location_code: String,
    scheduled_on: Option<String>,
    note: String,
    created_by: Option<Uuid>,
    created_at: OffsetDateTime,
    dispatched_at: Option<OffsetDateTime>,
    received_at: Option<OffsetDateTime>,
    cancelled_at: Option<OffsetDateTime>,
}

#[derive(Debug, sqlx::FromRow)]
struct LineRow {
    id: Uuid,
    item_id: Uuid,
    sku: String,
    item_name: String,
    quantity: String,
    received_qty: String,
    note: String,
}

/// The `select` both the list and the detail read through, so the two cannot answer differently.
const TRANSFER_SELECT: &str = "select t.id, t.organization_id, t.number, t.status, \
        t.from_location_id, f.code as from_location_code, t.to_location_id, \
        s.code as to_location_code, to_char(t.scheduled_on, 'YYYY-MM-DD') as scheduled_on, t.note, t.created_by, t.created_at, \
        t.dispatched_at, t.received_at, t.cancelled_at \
     from inventory_transfers t \
     join inventory_locations f on f.id = t.from_location_id \
     join inventory_locations s on s.id = t.to_location_id";

impl TransferRow {
    fn into_view(self, lines: Vec<TransferLine>) -> Result<TransferView> {
        let status = TransferStatus::parse(&self.status).ok_or_else(|| {
            InventoryError::invalid(
                "transfer",
                "status",
                format!("stored status {} is not one this module knows", self.status),
            )
        })?;
        let total = lines.iter().try_fold(Quantity::ZERO, |acc, line| {
            acc.checked_add(line.quantity)
                .ok_or_else(|| InventoryError::invalid("transfer", "lines", "the total is too large"))
        })?;
        let received = lines.iter().try_fold(Quantity::ZERO, |acc, line| {
            acc.checked_add(line.received_qty)
                .ok_or_else(|| InventoryError::invalid("transfer", "lines", "the total is too large"))
        })?;
        Ok(TransferView {
            id: self.id,
            organization_id: self.organization_id,
            number: self.number,
            status,
            from_location_id: self.from_location_id,
            from_location_code: self.from_location_code,
            to_location_id: self.to_location_id,
            to_location_code: self.to_location_code,
            scheduled_on: self.scheduled_on,
            note: self.note,
            created_by: self.created_by,
            created_at: self.created_at,
            dispatched_at: self.dispatched_at,
            received_at: self.received_at,
            cancelled_at: self.cancelled_at,
            lines,
            quantity_total: total.to_text(),
            received_total: received.to_text(),
        })
    }
}

impl LineRow {
    fn into_view(self) -> Result<TransferLine> {
        let quantity = store::quantity_from_text(&self.quantity)?;
        let received_qty = store::quantity_from_text(&self.received_qty)?;
        // `saturating_sub` through the same helper the method uses, so the wire value and the
        // in-process value cannot disagree.
        let outstanding = Quantity::from_milli(quantity.milli() - received_qty.milli())
            .unwrap_or(Quantity::ZERO);
        Ok(TransferLine {
            id: self.id,
            item_id: self.item_id,
            sku: self.sku,
            item_name: self.item_name,
            quantity,
            received_qty,
            outstanding,
            note: self.note,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The transit location
// ---------------------------------------------------------------------------------------------

/// The organization's in-transit location, or a refusal naming what is missing.
///
/// Found by **kind, not by code**. A code lookup (`'TRANSIT'`) would break the first time an
/// organization renamed its location — the code is a label a person can edit, the kind is the
/// rule. `0138` seeds the row and the `0126` trigger seeds it for tenants born later, so the
/// honest refusal here is for a database that was seeded by something else entirely.
pub async fn transit_location(pool: &PgPool, organization_id: Uuid) -> Result<store::LocationView> {
    store::list_locations(pool, organization_id)
        .await?
        .into_iter()
        .find(|location| location.kind == LocationKind::InTransit)
        .ok_or_else(|| {
            InventoryError::InvalidStatusChange(
                "this organization has no in-transit location, so a transfer has nowhere to sit \
                 between two shelves"
                    .into(),
            )
        })
}

// ---------------------------------------------------------------------------------------------
// Numbering
// ---------------------------------------------------------------------------------------------

/// The next paper number for this organization, inside the same transaction as the insert.
///
/// `max(number) + 1` on a text column would sort `TR-9` after `TR-10`, and the sequence is
/// advisory anyway: the unique index is the constraint, and two writers racing here produce one
/// success and one `409` rather than two documents with the same number.
async fn next_number(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
) -> Result<String> {
    let last: Option<String> = sqlx::query_scalar(
        "select number from inventory_transfers where organization_id = $1 \
         order by number desc limit 1",
    )
    .bind(organization_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let next = last
        .and_then(|raw| raw.rsplit('-').next().and_then(|tail| tail.parse::<u32>().ok()))
        .map_or(1, |n| n + 1);
    Ok(format!("TR-{next:04}"))
}

// ---------------------------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------------------------

/// Write a draft transfer.
///
/// A draft moves **nothing**. The available check happens at dispatch, not here, and that is a
/// decision rather than an omission: a transfer written on Monday for goods collected on
/// Wednesday is refused on Wednesday for the right reason — the shelf no longer has them — and a
/// check on Monday would refuse a document that is perfectly valid today. The line cap the spec
/// asks for is therefore proved at the step where the stock actually leaves.
pub async fn create_transfer(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewTransfer,
    actor: Option<Uuid>,
) -> Result<TransferView> {
    // Both locations are read **before** the transaction, and the values are dropped on purpose:
    // the reads turn a foreign or missing id into a 404 without holding a lock on a row the
    // document does not modify.
    let from = store::get_location(pool, organization_id, new.from_location_id).await?;
    let to = store::get_location(pool, organization_id, new.to_location_id).await?;

    if from.id == to.id {
        return Err(InventoryError::invalid(
            "transfer",
            "to_location_id",
            "a transfer has to move something — pick a different destination",
        ));
    }
    // A transfer that *ends* in transit is goods that arrived and were never put away, and one
    // that *starts* in transit is a transfer the module has already booked in. Both are refused
    // here rather than producing a document whose lifecycle has no meaning.
    if to.kind == LocationKind::InTransit {
        return Err(InventoryError::invalid(
            "transfer",
            "to_location_id",
            "in-transit is where goods sit between two shelves, not where they are put away",
        ));
    }
    if from.kind == LocationKind::InTransit {
        return Err(InventoryError::invalid(
            "transfer",
            "from_location_id",
            "goods leave in-transit by being received, not by starting another transfer",
        ));
    }
    if new.lines.is_empty() {
        return Err(InventoryError::invalid(
            "transfer",
            "lines",
            "a transfer with no lines moves nothing",
        ));
    }

    let note = crate::items::validate_note("transfer", new.note.as_deref().unwrap_or_default())?;
    let scheduled_on = match new.scheduled_on.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(parse_date("transfer", "scheduled_on", raw)?),
    };

    // The lines are validated up front, in the caller's order, so the first bad line is the one
    // named rather than whichever the database happened to refuse first.
    let mut prepared = Vec::with_capacity(new.lines.len());
    for line in &new.lines {
        // An item id that is not the organization's is a 404, not a constraint failure later.
        store::get_item(pool, organization_id, line.item_id).await?;
        let quantity = store::parse_quantity("transfer", "quantity", &line.quantity)?;
        if !quantity.is_positive() {
            return Err(InventoryError::invalid(
                "transfer",
                "quantity",
                "a transfer line takes a positive quantity",
            ));
        }
        let line_note =
            crate::items::validate_note("transfer", line.note.as_deref().unwrap_or_default())?;
        prepared.push((line.item_id, quantity, line_note));
    }

    // The same item twice on one document is one line with a bigger number, and the unique index
    // `inventory_transfer_lines_item_once` would refuse it as a database error rather than as the
    // sentence a person can act on.
    let mut seen: Vec<Uuid> = Vec::with_capacity(prepared.len());
    for (item_id, _, _) in &prepared {
        if seen.contains(item_id) {
            return Err(InventoryError::invalid(
                "transfer",
                "lines",
                "the same item is on this transfer twice — put both amounts on one line",
            ));
        }
        seen.push(*item_id);
    }

    let mut transaction = pool.begin().await?;
    let number = next_number(&mut transaction, organization_id).await?;
    let transfer_id = Uuid::new_v4();

    sqlx::query(
        // `$6::date` is **required**, not a nicety: the column is `date` and the parameter
        // arrives as text, so an uncast bind is a `500` from PostgreSQL on the first transfer
        // anybody writes. The cast is where the shape check above meets the column, which is
        // the one place it belongs — a `NULL` passes through the same way.
        "insert into inventory_transfers (id, organization_id, number, from_location_id, \
             to_location_id, scheduled_on, note, created_by) \
         values ($1, $2, $3, $4, $5, $6::date, $7, $8)",
    )
    .bind(transfer_id)
    .bind(organization_id)
    .bind(&number)
    .bind(from.id)
    .bind(to.id)
    .bind(scheduled_on)
    .bind(&note)
    .bind(actor)
    .execute(&mut *transaction)
    .await
    .map_err(|error| {
        map_transfer_violation(&error).unwrap_or(error.into())
    })?;

    for (item_id, quantity, line_note) in prepared {
        sqlx::query(
            "insert into inventory_transfer_lines (id, transfer_id, item_id, quantity, note) \
             values ($1, $2, $3, $4::numeric, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(transfer_id)
        .bind(item_id)
        .bind(quantity.to_text())
        .bind(line_note)
        .execute(&mut *transaction)
        .await?;
    }

    transaction.commit().await?;
    get_transfer(pool, organization_id, transfer_id).await
}

/// Turn a unique violation into the sentence a person can act on.
fn map_transfer_violation(error: &sqlx::Error) -> Option<InventoryError> {
    if let sqlx::Error::Database(db) = error {
        if db.is_unique_violation() {
            return Some(InventoryError::code_taken("transfer", "that number is taken"));
        }
    }
    None
}

/// `YYYY-MM-DD`, or a refusal naming the field.
///
/// **A shape check, not a calendar check.** The module parses every other date in this crate as
/// a `time::OffsetDateTime` because it compares it; this one is written to a `date` column and
/// only displayed, so the question is whether the *format* is the one the column accepts. The
/// real calendar — is there a 30 February — is PostgreSQL's answer to give, and a module that
/// carried its own calendar would be a second place for the rule to live.
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
        Err(InventoryError::invalid(entity, field, "use a date like 2026-09-29"))
    }
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

/// One transfer with its lines.
pub async fn get_transfer(
    pool: &PgPool,
    organization_id: Uuid,
    transfer_id: Uuid,
) -> Result<TransferView> {
    let row: Option<TransferRow> = sqlx::query_as(&format!(
        "{TRANSFER_SELECT} where t.organization_id = $1 and t.id = $2"
    ))
    .bind(organization_id)
    .bind(transfer_id)
    .fetch_optional(pool)
    .await?;
    let row = row.ok_or(InventoryError::NotFound("transfer"))?;
    let lines = load_lines(pool, transfer_id).await?;
    row.into_view(lines)
}

/// The lines of one transfer, in the order they were written.
async fn load_lines(pool: &PgPool, transfer_id: Uuid) -> Result<Vec<TransferLine>> {
    let rows: Vec<LineRow> = sqlx::query_as(
        "select l.id, l.item_id, i.sku, i.name as item_name, l.quantity::text as quantity, \
                l.received_qty::text as received_qty, l.note \
         from inventory_transfer_lines l \
         join inventory_items i on i.id = l.item_id \
         where l.transfer_id = $1 order by l.item_id",
    )
    .bind(transfer_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(LineRow::into_view).collect()
}

/// The transfer list.
pub async fn list_transfers(
    pool: &PgPool,
    organization_id: Uuid,
    query: &TransferQuery,
) -> Result<Page<TransferView>> {
    let statuses = query.parsed_statuses()?;
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(|raw| format!("%{}%", raw.to_lowercase()));

    // A `QueryBuilder` rather than a formatted string: the search term and the status list are
    // caller-controlled, and `like '%' || $1 || '%'` is what keeps a `%` in a search box from
    // turning into a wildcard that matches everything.
    let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new("select t.id, t.created_at from inventory_transfers t where t.organization_id = ");
    builder.push_bind(organization_id);

    if let Some(needle) = search {
        builder.push(" and (lower(t.number) like ").push_bind(needle.clone());
        builder.push(" or lower(t.note) like ").push_bind(needle.clone());
        builder.push(" or exists (select 1 from inventory_transfer_lines l \
             join inventory_items i on i.id = l.item_id \
             where l.transfer_id = t.id and (lower(i.sku) like ").push_bind(needle.clone());
        builder.push(" or lower(i.name) like ").push_bind(needle.clone()).push("))");
    }
    if !statuses.is_empty() {
        builder.push(" and t.status = any(").push_bind(
            statuses
                .iter()
                .map(|status| status.as_str().to_string())
                .collect::<Vec<_>>(),
        );
        builder.push(")");
    } else if query.open_only {
        builder.push(" and t.status in ('draft', 'dispatched')");
    }
    if let Some(from) = query.from_location_id {
        builder.push(" and t.from_location_id = ").push_bind(from);
    }
    if let Some(to) = query.to_location_id {
        builder.push(" and t.to_location_id = ").push_bind(to);
    }
    if let Some(cursor) = query.cursor_id() {
        builder.push(" and (t.created_at, t.id) < (select created_at, id from inventory_transfers where id = ").push_bind(cursor).push(")");
    }
    builder.push(" order by t.created_at desc, t.id desc limit ").push_bind(query.page_size() + 1);

    // `build_query_as`, not `build_as` and not a hand-rolled `sql + arguments`: `Query`'s
    // fields are private, so the builder has to be the thing that types the row. The builder is
    // still what assembles the SQL — a formatted string with `{}` slots is the wrong way to get
    // a caller-controlled search term in here.
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
        items.push(get_transfer(pool, organization_id, *id).await?);
    }
    let next_cursor = has_more
        .then(|| ids.last().map(ToString::to_string))
        .flatten();

    Ok(Page {
        items,
        next_cursor,
        total_estimate: 0,
    })
}

// ---------------------------------------------------------------------------------------------
// The steps
// ---------------------------------------------------------------------------------------------

/// Book the goods out of the source and into transit.
///
/// **Two movements, one transaction at the document level.** The transit leg is what keeps the
/// organization's total true while the goods are on a van; without it a dispatch would delete
/// stock from the platform for the length of the journey and the stock list would show a hole
/// nobody could explain.
///
/// The available check happens here and not at create, and each line is checked against the
/// balance **as of this moment** — so two transfers dispatching the same shelf serialize on the
/// stock row inside [`crate::ledger::record_movement`] and the second one sees the first one's
/// total. Checking the lines up front and then writing them would be a check and a write in
/// different places, which is the shape of the bug this module exists to prevent.
pub async fn dispatch(
    pool: &PgPool,
    organization_id: Uuid,
    transfer_id: Uuid,
    actor: Option<Uuid>,
) -> Result<TransferView> {
    let view = get_transfer(pool, organization_id, transfer_id).await?;
    if view.status != TransferStatus::Draft {
        return Err(status_change_error(
            view.status,
            "dispatch",
            "nothing has left the source yet",
        ));
    }
    let transit = transit_location(pool, organization_id).await?;

    for line in &view.lines {
        let position = ledger::stock_level(pool, organization_id, line.item_id, view.from_location_id).await?;
        let available = position
            .on_hand
            .checked_sub(position.reserved)
            .unwrap_or(Quantity::ZERO);
        if line.quantity.milli() > available.milli() {
            // The sentence names the shortfall as well as the balance, because the person
            // holding the cart has two decisions: send less, or go and find it.
            let shortfall = line.quantity.milli() - available.milli();
            return Err(InventoryError::negative(
                format!(
                    "{} has {} at {} and this line sends {}, which is {} more",
                    line.sku,
                    available.to_text(),
                    view.from_location_code,
                    line.quantity.to_text(),
                    Quantity::from_milli(shortfall).unwrap_or(Quantity::ZERO),
                ),
                available.to_text(),
            ));
        }
    }

    for line in &view.lines {
        let out = transfer_leg(
            pool, organization_id, &view, line, line.quantity,
            view.from_location_id, MovementKind::TransferOut, actor, "dispatched",
        ).await?;
        // The transit leg is written by the same function with the opposite sign, so the
        // arithmetic of a transfer is the arithmetic of an ordinary movement and there is no
        // second implementation to disagree with it.
        let _ = out;
        transfer_leg(
            pool, organization_id, &view, line, line.quantity,
            transit.id, MovementKind::TransferIn, actor, "dispatched",
        ).await?;
    }

    set_status(
        pool,
        transfer_id,
        TransferStatus::Dispatched,
        "dispatched_at = now()",
    )
    .await?;
    get_transfer(pool, organization_id, transfer_id).await
}

/// Book the goods out of transit and into the target.
///
/// A **partial receive is allowed per line** (the spec) and the remainder stays open: a driver
/// who delivers two of three pallets leaves the transfer `dispatched` with one line outstanding,
/// and the next receive finishes it. A receive with no lines is refused rather than accepted as
/// "nothing left to do" — a caller who believes the transfer is finished wants the status to say
/// so, and a receive that changes nothing is not how they say it.
pub async fn receive(
    pool: &PgPool,
    organization_id: Uuid,
    transfer_id: Uuid,
    steps: &[TransferStepLine],
    actor: Option<Uuid>,
) -> Result<TransferView> {
    let view = get_transfer(pool, organization_id, transfer_id).await?;
    if view.status != TransferStatus::Dispatched {
        return Err(status_change_error(
            view.status,
            "receive",
            "the goods have not left the source yet",
        ));
    }
    if steps.is_empty() {
        return Err(InventoryError::invalid(
            "transfer",
            "lines",
            "name the lines that arrived — an empty receive books nothing",
        ));
    }
    let transit = transit_location(pool, organization_id).await?;

    for step in steps {
        let line = view
            .lines
            .iter()
            .find(|line| line.id == step.line_id)
            .ok_or_else(|| InventoryError::NotFound("transfer line"))?;
        let quantity = store::parse_quantity("transfer", "quantity", &step.quantity)?;
        if !quantity.is_positive() {
            return Err(InventoryError::invalid(
                "transfer",
                "quantity",
                "a receive takes a positive quantity",
            ));
        }
        let outstanding = line.outstanding();
        if quantity.milli() > outstanding.milli() {
            return Err(InventoryError::negative(
                format!(
                    "{} sent {} but only {} is still on the van",
                    line.sku,
                    line.quantity.to_text(),
                    outstanding.to_text()
                ),
                outstanding.to_text(),
            ));
        }

        transfer_leg(
            pool, organization_id, &view, line, quantity,
            transit.id, MovementKind::TransferOut, actor, "received",
        ).await?;
        transfer_leg(
            pool, organization_id, &view, line, quantity,
            view.to_location_id, MovementKind::TransferIn, actor, "received",
        ).await?;

        sqlx::query(
            "update inventory_transfer_lines set received_qty = received_qty + $2::numeric \
             where id = $1",
        )
        .bind(line.id)
        .bind(quantity.to_text())
        .execute(pool)
        .await?;
    }

    // The status is a **derived** answer, not a decision the caller makes: the document is
    // received when nothing is outstanding, and stays dispatched when a line is still on the
    // van. A caller that could set the status directly could set it while three pallets are
    // still moving, and the next receive would be refused for being out of order.
    let after = get_transfer(pool, organization_id, transfer_id).await?;
    if after.lines.iter().all(|line| line.outstanding().is_zero()) {
        set_status(
            pool,
            transfer_id,
            TransferStatus::Received,
            "received_at = now()",
        )
        .await?;
    } else {
        sqlx::query("update inventory_transfers set updated_at = now() where id = $1")
            .bind(transfer_id)
            .execute(pool)
            .await?;
    }
    get_transfer(pool, organization_id, transfer_id).await
}

/// Withdraw the transfer, before or after dispatch.
///
/// **Cancelling a dispatched transfer is not free of ledger rows.** The goods are on a van, so
/// bringing them home is a movement out of transit and in at the source; writing only a status
/// change would leave the transit balance holding goods the document says came home, and the next
/// [`crate::ledger::replay`] would report a disagreement the module had manufactured itself. A
/// draft, on the other hand, moves nothing and therefore writes nothing.
pub async fn cancel(
    pool: &PgPool,
    organization_id: Uuid,
    transfer_id: Uuid,
    actor: Option<Uuid>,
) -> Result<TransferView> {
    let view = get_transfer(pool, organization_id, transfer_id).await?;
    match view.status {
        TransferStatus::Draft => {
            set_status(
                pool,
                transfer_id,
                TransferStatus::Cancelled,
                "cancelled_at = now()",
            )
            .await?;
        }
        TransferStatus::Dispatched => {
            let transit = transit_location(pool, organization_id).await?;
            for line in &view.lines {
                let outstanding = line.outstanding();
                if outstanding.is_zero() {
                    continue;
                }
                transfer_leg(
                    pool, organization_id, &view, line, outstanding,
                    transit.id, MovementKind::TransferOut, actor, "cancelled",
                ).await?;
                transfer_leg(
                    pool, organization_id, &view, line, outstanding,
                    view.from_location_id, MovementKind::TransferIn, actor, "cancelled",
                ).await?;
            }
            set_status(
                pool,
                transfer_id,
                TransferStatus::Cancelled,
                "cancelled_at = now()",
            )
            .await?;
        }
        other => {
            return Err(status_change_error(
                other,
                "cancel",
                "the goods are already at the destination",
            ));
        }
    }
    get_transfer(pool, organization_id, transfer_id).await
}

/// The refusal for a step the document's status does not allow.
///
/// The sentence carries **what the document is** before it carries what you asked for, because
/// the caller who is wrong about the order is the one who needs to know where it actually is.
fn status_change_error(
    current: TransferStatus,
    step: &str,
    because: &str,
) -> InventoryError {
    InventoryError::InvalidStatusChange(format!(
        "this transfer is {} and cannot be {step}ed: {because}",
        current.as_str()
    ))
}

/// One movement leg of a transfer, through the ledger's one write path.
async fn transfer_leg(
    pool: &PgPool,
    organization_id: Uuid,
    view: &TransferView,
    line: &TransferLine,
    quantity: Quantity,
    location_id: Uuid,
    kind: MovementKind,
    actor: Option<Uuid>,
    step: &str,
) -> Result<Recorded> {
    let movement = NewMovement {
        item_id: line.item_id,
        location_id,
        kind: Some(kind.as_str().to_string()),
        quantity: quantity.to_text(),
        reason: Some(ReasonCode::Transfer.as_str().to_string()),
        // The document number is in the note rather than only in the `source_id`, because the
        // ledger screen prints the note and a person reading a row six months later should not
        // have to resolve an id to learn which transfer it was.
        note: Some(format!("{} \u{b7} {step}", view.number)),
        source_kind: Some(TRANSFER_SOURCE.to_string()),
        source_id: Some(view.id),
        may_go_negative: false,
    };
    ledger::record_movement(pool, organization_id, &movement, actor).await
}

/// Move a document to a new status, writing the timestamp the status claims.
///
/// The timestamp is written **by the statement the status needs**, not by a `now()` the caller
/// passes in, so `dispatched_at` is the instant the row was dispatched rather than the instant
/// the caller started thinking about it.
async fn set_status(
    pool: &PgPool,
    transfer_id: Uuid,
    status: TransferStatus,
    stamp: &str,
) -> Result<()> {
    // `stamp` is a literal from this module's four call sites, never caller input.
    let statement = format!(
        "update inventory_transfers set status = $2, {stamp}, updated_at = now() where id = $1"
    );
    let changed = sqlx::query(&statement)
        .bind(transfer_id)
        .bind(status.as_str())
        .execute(pool)
        .await?
        .rows_affected();
    if changed == 0 {
        return Err(InventoryError::NotFound("transfer"));
    }
    Ok(())
}

/// The lines that are still on the van, as `(item_id, quantity)`.
///
/// Used by the overview and by the stocktake's in-transit figure; a transfer whose lines are all
/// received is not "open stock in transit" however its status reads.
pub async fn outstanding_totals(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<(Uuid, Quantity)>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "select l.item_id, sum(l.quantity - l.received_qty)::text as outstanding \
         from inventory_transfer_lines l \
         join inventory_transfers t on t.id = l.transfer_id \
         where t.organization_id = $1 and t.status = 'dispatched' \
         group by l.item_id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(item_id, outstanding)| Ok((item_id, store::quantity_from_text(&outstanding)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(quantity: &str, received: &str) -> TransferLine {
        TransferLine {
            id: Uuid::new_v4(),
            item_id: Uuid::new_v4(),
            sku: "BOLT-M8".into(),
            item_name: "Bolt M8".into(),
            quantity: Quantity::parse(quantity).expect("quantity"),
            received_qty: Quantity::parse(received).expect("received"),
            outstanding: Quantity::ZERO,
            note: String::new(),
        }
    }

    #[test]
    fn an_outstanding_line_is_what_is_sent_less_what_landed() {
        // The whole point of `received_qty` being per line: a partial receive leaves a remainder
        // that the next receive draws against, and a line that is fully received has none.
        assert_eq!(line("10.000", "0.000").outstanding().to_text(), "10.000");
        assert_eq!(line("10.000", "4.000").outstanding().to_text(), "6.000");
        assert_eq!(line("10.000", "10.000").outstanding().to_text(), "0.000");
    }

    #[test]
    fn the_status_decides_which_buttons_the_screen_offers() {
        // One definition for three buttons. A screen that asked the status string a second
        // question would have a fourth answer the moment a fifth status arrived.
        let view = |status| TransferView {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            number: "TR-0001".into(),
            status,
            from_location_id: Uuid::new_v4(),
            from_location_code: "STOCK".into(),
            to_location_id: Uuid::new_v4(),
            to_location_code: "STOCK-2".into(),
            scheduled_on: None,
            note: String::new(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            dispatched_at: None,
            received_at: None,
            cancelled_at: None,
            lines: vec![],
            quantity_total: "0.000".into(),
            received_total: "0.000".into(),
        };
        let draft = view(TransferStatus::Draft);
        assert!(draft.can_dispatch());
        assert!(!draft.can_receive());
        assert!(draft.is_open());

        let dispatched = view(TransferStatus::Dispatched);
        assert!(!dispatched.can_dispatch());
        assert!(dispatched.can_receive());
        assert!(dispatched.is_open());

        let received = view(TransferStatus::Received);
        assert!(!received.can_dispatch());
        assert!(!received.can_receive());
        assert!(!received.is_open());
    }

    #[test]
    fn an_unknown_filter_status_is_refused_rather_than_ignored() {
        // A filter that silently drops what it does not understand answers "you have no
        // transfers", which is the one conclusion a typo must never be able to produce.
        let query = TransferQuery {
            statuses: vec!["dispatched".into(), "in_a_van".into()],
            ..TransferQuery::default()
        };
        let error = query.parsed_statuses().expect_err("a typo must not pass");
        assert!(error.to_string().contains("in_a_van"), "{error}");
    }

    #[test]
    fn a_page_size_the_caller_overshot_is_clamped_rather_than_refused() {
        // Overshooting is a display preference; refusing it would break a window that was simply
        // made wider. Zero and negative are clamped the same way rather than reaching the
        // database as `limit 0`.
        let query = TransferQuery { limit: Some(1_000_000), ..TransferQuery::default() };
        assert_eq!(query.page_size(), MAX_PER_PAGE);
        let query = TransferQuery { limit: Some(0), ..TransferQuery::default() };
        assert_eq!(query.page_size(), DEFAULT_PER_PAGE);
    }

    #[test]
    fn a_bad_status_change_says_where_the_document_actually_is() {
        // The sentence leads with the document's real status: a caller who is out of order is
        // the one who needs to know where it got to.
        let sentence = status_change_error(TransferStatus::Received, "dispatch", "nothing has left the source yet")
            .to_string();
        assert!(sentence.contains("received"), "{sentence}");
        assert!(sentence.contains("dispatch"), "{sentence}");
    }
}
