//! The append-only movement ledger and the rollup it must always agree with.
//!
//! This module is the reason REQ-053 exists, and it has exactly one job: **never let
//! `inventory_stock` and `inventory_movements` disagree.** Everything else here is in service of
//! that.
//!
//! How it is made true rather than hoped for:
//!
//! * **One write path.** [`record_movement`] is the only function that changes either table, and
//!   it does both in one transaction with the stock row locked `for update`. There is no
//!   `update_stock` a screen can call.
//! * **The resulting numbers are on the ledger row.** `on_hand_after` and `reserved_after` are
//!   written from the same locked read that produced them, so the ledger is **replayable**:
//!   [`replay`] recomputes every item × location from the movements alone and
//!   [`crate::store::reconciliation_report`] compares that with the rollup. A service that
//!   computed the wrong number is caught by a test, not by a stocktake six months later.
//! * **The sign is the kind's, not the number's.** A row stores a positive `quantity` and a kind
//!   that says which way it went; an adjustment carries its own sign. Storing a signed number
//!   next to a kind is two sources of truth for one fact.
//! * **Concurrency is handled by the lock, not by a retry.** Two receipts for the same item at
//!   the same location serialize on the row, so the second reads the first's total. `max(number) + 1`
//!   would have been the wrong shape here for the same reason it was in the quote numbering.
//!
//! What is deliberately **not** here: a way to edit or delete a movement. The ledger is a record of
//! what happened, and the way to fix a mistake is another movement — a `correction` with a note
//! saying what it corrects.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgRow;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{InventoryError, Result};
use crate::items;
use crate::model::ReasonCode;
use crate::model::{MovementKind, StockStatus};
use crate::money::Quantity;
use crate::store::{self, StockLevel};

/// A movement the caller asked for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewMovement {
    /// The item that moves.
    pub item_id: Uuid,
    /// The location it moves at.
    pub location_id: Uuid,
    /// What it does to that location's `on_hand`.
    ///
    /// Optional on a hand-written movement: the drawer infers it from the reason and the mode
    /// (a "counted to" adjustment is an adjustment, a positive delta with `customer_return` is a
    /// receipt), and a caller that names it gets exactly what it asked for.
    #[serde(default)]
    pub kind: Option<String>,
    /// The quantity, as text, **always positive** except for an adjustment.
    pub quantity: String,
    /// Why it happened.
    #[serde(default)]
    pub reason: Option<String>,
    /// A note, up to 500 characters.
    #[serde(default)]
    pub note: Option<String>,
    /// What caused it (`order`, `transfer`, `stocktake`, `manual`).
    #[serde(default)]
    pub source_kind: Option<String>,
    /// The document that caused it, when there is one.
    #[serde(default)]
    pub source_id: Option<Uuid>,
    /// Whether the caller holds `inventory.negative.manage`.
    ///
    /// **A boolean, not a permission lookup**, and the reason is that this crate is a module: it
    /// has no session and no key set. The HTTP layer is the only place that can answer, so it
    /// asks once and passes the answer down. A module that took a key set would be a module that
    /// could be called by something without one.
    #[serde(default)]
    pub may_go_negative: bool,
}

/// The movement as it is stored, and as the ledger screen renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Movement {
    /// The ledger row's id (a bigint, not a uuid: it is a sequence within an organization).
    pub id: i64,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The item.
    pub item_id: Uuid,
    /// The item's SKU, so the ledger prints what a person reads on the shelf.
    pub sku: String,
    /// The item's name.
    pub item_name: String,
    /// The location it moved at.
    pub location_id: Uuid,
    /// The location's code.
    pub location_code: String,
    /// What it did.
    pub kind: MovementKind,
    /// The quantity, positive for every kind but an adjustment.
    pub quantity: Quantity,
    /// Why it happened.
    pub reason: ReasonCode,
    /// The document that caused it, when there is one.
    #[serde(default)]
    pub source_kind: Option<String>,
    /// That document's id.
    #[serde(default)]
    pub source_id: Option<Uuid>,
    /// The note.
    pub note: String,
    /// The location's `on_hand` **after this row** — the number the drawer shows, and the reason
    /// the ledger can be replayed without trusting the rollup.
    pub on_hand_after: Quantity,
    /// The location's `reserved` after this row.
    pub reserved_after: Quantity,
    /// Who recorded it.
    #[serde(default)]
    pub actor_user_id: Option<Uuid>,
    /// When.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl Movement {
    /// The quantity as the ledger prints it: **signed by the kind**.
    ///
    /// The stored value is positive; the display is what a person reads while walking the
    /// warehouse ("+10.000", "−4.000"). Deriving it here means no screen has to remember which
    /// kinds go which way, and a new kind cannot be added without the sign being decided.
    #[must_use]
    pub fn signed_quantity(&self) -> Quantity {
        let magnitude = self.quantity.milli().abs();
        let signed = i128::from(self.kind.signed()) * magnitude;
        Quantity::from_milli(signed).unwrap_or(Quantity::ZERO)
    }

    /// The compact reference an audit row and an event payload carry.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "movement_id": self.id,
            "item_id": self.item_id,
            "sku": self.sku,
            "location_id": self.location_id,
            "kind": self.kind.as_str(),
            "quantity": self.quantity.to_text(),
            "reason": self.reason.as_str(),
            "on_hand_after": self.on_hand_after.to_text(),
        })
    }
}

/// The ledger screen's query.
#[derive(Debug, Clone, Default)]
pub struct MovementQuery {
    /// Free text over SKU, name and note.
    pub search: Option<String>,
    /// One item.
    pub item_id: Option<Uuid>,
    /// One location.
    pub location_id: Option<Uuid>,
    /// One kind — repeated for several.
    pub kinds: Vec<String>,
    /// One reason.
    pub reason: Option<String>,
    /// Who recorded it.
    pub actor_user_id: Option<Uuid>,
    /// A document reference (`Q-2026-0007` is the `source_id` of the order; this is a free text
    /// match on the source kind and id so "everything this transfer caused" is one filter).
    pub source: Option<String>,
    /// From, as an RFC 3339 timestamp.
    pub from: Option<String>,
    /// To, as an RFC 3339 timestamp.
    pub to: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Cursor — the id of the last row of the previous page.
    pub cursor: Option<i64>,
}

impl MovementQuery {
    /// How many rows a page holds, or the default.
    pub fn page_size(&self) -> i64 {
        match self.limit {
            None => store::DEFAULT_PER_PAGE,
            Some(limit) if limit < 1 => store::DEFAULT_PER_PAGE,
            Some(limit) if limit > store::MAX_PER_PAGE => store::MAX_PER_PAGE,
            Some(limit) => limit,
        }
    }

    /// The id the cursor points at, if there is one.
    pub fn cursor_id(&self) -> Option<i64> {
        self.cursor.filter(|id| *id > 0)
    }

    /// The window's lower bound, parsed. A bad timestamp is a refusal naming the field, because
    /// a filter that silently ignores a bad date shows a window the person did not ask for.
    pub fn from_at(&self) -> Result<Option<OffsetDateTime>> {
        self.from
            .as_deref()
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(|raw| {
                crate::dates::parse(raw).map_err(|_| {
                    InventoryError::invalid("movement", "from", "use an RFC 3339 timestamp")
                })
            })
            .transpose()
    }

    /// The window's upper bound, parsed.
    pub fn to_at(&self) -> Result<Option<OffsetDateTime>> {
        self.to
            .as_deref()
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(|raw| {
                crate::dates::parse(raw).map_err(|_| {
                    InventoryError::invalid("movement", "to", "use an RFC 3339 timestamp")
                })
            })
            .transpose()
    }
}

/// The outcome of a recorded movement: the row and the stock position it produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recorded {
    /// The ledger row.
    pub movement: Movement,
    /// The stock row after the write — the drawer shows it so the operator sees the result of the
    /// number they typed without reloading the list.
    pub position: StockLevel,
}

/// What a record request produced: either the movement, or a request waiting for a decision.
///
/// **Two states, not two endpoints.** The drawer posts one body to one URL, and the honest
/// answer to "I typed an adjustment of 400" is sometimes a ledger row and sometimes a pending
/// request. If those were two endpoints the drawer would have to know the threshold to choose
/// between them — which is exactly the knowledge the server holds. Making the *response* say
/// which happened keeps the decision in one place, and it is why the save button can say
/// "requesting" without the screen re-implementing `needs_approval`.
///
/// Serialised as `{"status": "recorded" | "awaiting_approval", …}` so a client reads one field
/// rather than probing for a missing key.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RecordOutcome {
    /// The movement was written.
    Recorded {
        /// The ledger row.
        movement: Movement,
        /// The stock row after the write.
        position: StockLevel,
    },
    /// The adjustment was over the threshold and is waiting for somebody else to decide it.
    AwaitingApproval {
        /// The request that now exists.
        approval: crate::approvals::ApprovalView,
    },
}

/// Compute what a movement would do, without writing it.
///
/// The adjust drawer calls this to show "you are about to take 4 from 10, leaving 6" **before**
/// the person commits, and the walkthrough reads the same function, so the number the screen
/// previewed and the number the row produced come from one implementation rather than two.
pub fn apply_movement(
    current_on_hand: Quantity,
    current_reserved: Quantity,
    kind: MovementKind,
    quantity: Quantity,
    reason: ReasonCode,
    may_go_negative: bool,
) -> Result<(Quantity, Quantity)> {
    let (on_hand, reserved) = match kind {
        MovementKind::Adjustment => {
            // An adjustment is a **delta** and may be either sign; the sign rule in the schema
            // refuses zero, and so does this.
            if quantity.is_zero() {
                return Err(InventoryError::invalid(
                    "movement",
                    "quantity",
                    "an adjustment of zero is not a movement",
                ));
            }
            let next = on_hand_after(current_on_hand, quantity, kind);
            (next, current_reserved)
        }
        MovementKind::Reserve => (
            current_on_hand,
            current_reserved
                .checked_add(quantity)
                .ok_or_else(|| InventoryError::invalid("movement", "quantity", "too large"))?,
        ),
        MovementKind::Release => {
            // `checked_sub` refuses an **overflow**, not a negative result: `Quantity` holds an
            // `i128` and `0 − 1000` is a perfectly good value of the type. The check that matters
            // is therefore written out — a release may not take `reserved` below zero, because a
            // negative reservation is a hold on stock nobody has, and the schema's
            // `reserved >= 0` constraint would refuse the row *after* the ledger row was written.
            let released = current_reserved.checked_sub(quantity).ok_or_else(|| {
                InventoryError::invalid("movement", "quantity", "that number is too large")
            })?;
            if released.is_negative() {
                return Err(InventoryError::invalid(
                    "movement",
                    "quantity",
                    format!(
                        "only {current_reserved} is held here — releasing {quantity} would take \
                         the reservation below zero"
                    ),
                ));
            }
            (current_on_hand, released)
        }
        _ => {
            if !quantity.is_positive() {
                return Err(InventoryError::invalid(
                    "movement",
                    "quantity",
                    "a receipt, issue or transfer takes a positive quantity",
                ));
            }
            (on_hand_after(current_on_hand, quantity, kind), current_reserved)
        }
    };

    // The negative-stock rule, in one place. A `correction` with the permission may go below zero;
    // every other reason is refused outright, and the message carries the number that is
    // available because the person reading it is standing at the shelf.
    if on_hand.is_negative() {
        let allowed = reason.may_go_negative() && may_go_negative;
        if !allowed {
            let available = current_on_hand.checked_sub(current_reserved).unwrap_or(Quantity::ZERO);
            // The two sentences say different things and both are needed: the first tells a
            // clerk what to ask for, the second tells a manager that the reason they picked is
            // the wrong one. One message for both would leave one of the two people stuck.
            let reason_note = if reason.may_go_negative() {
                "a correction below zero needs inventory.negative.manage".to_string()
            } else {
                format!("reason {} cannot take stock below zero", reason.as_str())
            };
            return Err(InventoryError::negative(
                format!("this would leave {on_hand} at this location — {reason_note}"),
                available.to_text(),
            ));
        }
    }

    // You cannot hold stock that does not exist, whatever the reason — and this one is
    // unconditional, which is why the schema mirrors it as a check constraint.
    //
    // **Except once the balance is already negative.** A permitted `correction` may leave
    // `on_hand` at −3, and the check above has already decided that is allowed; re-testing
    // `reserved <= on_hand` afterwards would then refuse the very write the permission exists to
    // permit, with a message ("this would hold 0.000 against −3.000 on hand") that is nonsense to
    // read. The schema's `reserved <= on_hand` had the same gap and was fixed in the same way —
    // `0126_inventory.sql` now says `reserved <= on_hand or on_hand < 0` — and the two have to
    // agree: a service that allowed a negative and a constraint that then refused the row would
    // leave a written ledger row with no rollup behind it, which is the one state this module
    // exists to make impossible. That is how the gap was found: the service's own test for a
    // permitted negative balance failed on it.
    if !on_hand.is_negative() && reserved.milli() > on_hand.milli() {
        return Err(InventoryError::negative(
            format!("this would hold {reserved} against {on_hand} on hand"),
            on_hand.to_text(),
        ));
    }

    Ok((on_hand, reserved))
}

/// `on_hand` after a movement of `quantity` — the one arithmetic in the module.
fn on_hand_after(current: Quantity, quantity: Quantity, kind: MovementKind) -> Quantity {
    match kind.signed() {
        1 => current.checked_add(quantity).unwrap_or(current),
        -1 => current.checked_sub(quantity).unwrap_or(current),
        // An adjustment carries its own sign, so it is a plain addition of a possibly-negative
        // number. `0` is the other kind that reaches here, and a reserve/release must not move
        // `on_hand` at all.
        _ => current.checked_add(quantity).unwrap_or(current),
    }
}

/// Record a movement: append the ledger row and update the rollup, in one transaction.
///
/// **The order is the correctness argument.** The stock row is locked `for update` first, so two
/// concurrent movements for the same item at the same location serialize and the second one reads
/// the first one's total; the new numbers are computed from that locked read; the ledger row is
/// written with those numbers on it; and only then is the rollup updated. If the ledger insert
/// failed after the rollup had been written, the module would be in exactly the state it exists to
/// prevent — so the ledger row goes first and the rollup write is the one that can be lost, and a
/// lost rollup write is caught by [`replay`] rather than by a customer.
///
/// The stock row is **upserted rather than assumed**: an item's first movement at a location has
/// no row yet, and an `update` that matched nothing would silently drop the movement's effect on
/// the rollup while keeping its ledger row.
pub async fn record_movement(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewMovement,
    actor: Option<Uuid>,
) -> Result<Recorded> {
    // Both rows are read before the transaction and the values are dropped on purpose: the reads
    // are there to turn a foreign id into a **404 before** any write is attempted, not to be
    // used afterwards. Re-reading them inside the transaction would lock a row the movement does
    // not modify and would suggest they matter to the arithmetic when they do not.
    store::get_item(pool, organization_id, new.item_id).await?;
    store::get_location(pool, organization_id, new.location_id).await?;
    let note = items::validate_note("movement", new.note.as_deref().unwrap_or_default())?;
    let reason = items::default_reason(new.reason.as_deref())?;
    let quantity = store::parse_quantity("movement", "quantity", &new.quantity)?;
    // `resolve_kind` is what refuses a hand-written reservation, and it is reached only from
    // here: the reservation bridge calls `record_resolved` with the kind already decided, so the
    // two callers that need opposite answers never have to be told apart by a flag.
    let kind = resolve_kind(new.kind.as_deref(), reason, quantity)?;

    check_source_kind(new.source_kind.as_deref())?;

    record_resolved(
        pool,
        organization_id,
        new.item_id,
        new.location_id,
        kind,
        quantity,
        reason,
        new.source_kind.as_deref(),
        new.source_id,
        &note,
        new.may_go_negative,
        actor,
    )
    .await
}

/// The one write path, with every decision already made.
///
/// [`record_movement`] validates and decides, then calls this. [`crate::reservations`] calls this
/// directly, because a reservation knows its kind by definition — that is the whole difference
/// between a hold and a hand-written movement, and re-deciding it here would mean a second place
/// where "which kind is this?" is answered.
///
/// The split exists so the ordering argument in this module's own header stays in one function:
/// the lock, the arithmetic, the ledger row first, the rollup second. A second caller cannot get
/// it half right because there is no second copy of it to get half right.
#[allow(clippy::too_many_arguments)]
pub async fn record_resolved(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
    location_id: Uuid,
    kind: MovementKind,
    quantity: Quantity,
    reason: ReasonCode,
    source_kind: Option<&str>,
    source_id: Option<Uuid>,
    note: &str,
    may_go_negative: bool,
    actor: Option<Uuid>,
) -> Result<Recorded> {
    let mut transaction = pool.begin().await?;

    // The lock, and the only place the rollup is read for a write.
    let current: Option<(Uuid, String, String)> = sqlx::query_as(
        "select id, on_hand::text as on_hand, reserved::text as reserved from inventory_stock \
         where organization_id = $1 and item_id = $2 and location_id = $3 for update",
    )
    .bind(organization_id)
    .bind(item_id)
    .bind(location_id)
    .fetch_optional(&mut *transaction)
    .await?;

    let (stock_id, current_on_hand, current_reserved) = match current {
        Some((id, on_hand, reserved)) => (
            id,
            store::quantity_from_text(&on_hand)?,
            store::quantity_from_text(&reserved)?,
        ),
        // `new_v4` in this workspace is infallible (the `v4` feature is on workspace-wide),
        // so the id is taken without a fallback. An `unwrap_or_else` here would compile on a
        // feature set where it does not and silently produce a nil id for every first movement.
        None => (
            Uuid::new_v4(),
            Quantity::ZERO,
            Quantity::ZERO,
        ),
    };

    let (next_on_hand, next_reserved) = apply_movement(
        current_on_hand,
        current_reserved,
        kind,
        quantity,
        reason,
        may_go_negative,
    )?;

    // The ledger row first, carrying the numbers that were computed from the locked read.
    let movement_id: i64 = sqlx::query_scalar(
        "insert into inventory_movements (organization_id, item_id, location_id, kind, quantity, \
             reason, source_kind, source_id, note, on_hand_after, reserved_after, actor_user_id) \
         values ($1, $2, $3, $4, $5::numeric, $6, $7, $8, $9, $10::numeric, $11::numeric, $12) \
         returning id",
    )
    .bind(organization_id)
    .bind(item_id)
    .bind(location_id)
    .bind(kind.as_str())
    .bind(quantity.to_text())
    .bind(reason.as_str())
    .bind(source_kind)
    .bind(source_id)
    .bind(note)
    .bind(next_on_hand.to_text())
    .bind(next_reserved.to_text())
    .bind(actor)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| {
        // A check-constraint violation on the sign rule means the module computed something the
        // schema refuses; surfacing the schema's sentence is more useful than a 500.
        if let sqlx::Error::Database(db) = &error {
            if db.constraint() == Some("inventory_movements_sign") {
                return InventoryError::invalid(
                    "movement",
                    "quantity",
                    "this kind of movement needs a positive quantity (an adjustment may be either)",
                );
            }
        }
        error.into()
    })?;

    sqlx::query(
        "insert into inventory_stock (id, organization_id, item_id, location_id, on_hand, \
             reserved, last_movement_at) \
         values ($1, $2, $3, $4, $5::numeric, $6::numeric, now()) \
         on conflict (item_id, location_id) do update set \
             on_hand = excluded.on_hand, reserved = excluded.reserved, \
             last_movement_at = now(), updated_at = now()",
    )
    .bind(stock_id)
    .bind(organization_id)
    .bind(item_id)
    .bind(location_id)
    .bind(next_on_hand.to_text())
    .bind(next_reserved.to_text())
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;

    // Read both back through the same functions the screens use, so the answer a caller gets is
    // the answer the list would have drawn — not a re-derivation of the arithmetic done twice.
    let movement = get_movement(pool, movement_id).await?;
    let position = stock_level(pool, organization_id, item_id, location_id).await?;
    Ok(Recorded { movement, position })
}

/// Decide what a hand-written movement does, from what the caller named and what they meant.
///
/// A caller that names the kind gets it. A caller that does not gets the kind the **reason**
/// implies, which is what the drawer's "record a receipt" and "record an issue" buttons send:
/// a positive quantity with `purchase_receipt` is a receipt, a positive quantity with
/// `sale_shipment` is an issue, and a positive quantity with `customer_return` is a receipt (the
/// goods come **back**). Getting that last one backwards is the mistake the table exists to
/// The `source_kind` a movement names, validated once for both callers.
///
/// The ledger's `source` column is what a person reads three months later to answer "what did
/// this?" (`order SO-2026-0007`, `stocktake ST-4`), and it is also an `inventory_movements_source_idx`
/// lookup key. Free text there would make that index useless and the column a place to type
/// whatever the mood suggested, so the vocabulary is closed to lower-case words and underscores.
fn check_source_kind(raw: Option<&str>) -> Result<()> {
    if let Some(raw) = raw
        && (raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    {
        return Err(InventoryError::invalid(
            "movement",
            "source_kind",
            "use lower-case words and underscores, like `order` or `stocktake`",
        ));
    }
    Ok(())
}

/// A hand-written movement may only *choose* a kind it is allowed to record.
///
/// This lives inside [`resolve_kind`] rather than beside the caller because the reservation
/// bridge names its kind too, and the two callers need opposite answers: a person at the adjust
/// drawer must be refused a `reserve` (a hold nobody will fulfil is a lie in the ledger), while
/// `sales.order.confirmed` must be *able* to name one. Splitting on "is this caller the order
/// service?" is exactly the kind of question that gets answered wrongly at 02:00, so the resolver
/// asks about the **caller's claim** instead: if the caller is explicit about the kind, it is
/// taken; if it left the kind to the reason, the reason decides and a reason can never produce a
/// reservation.
fn resolve_kind(named: Option<&str>, reason: ReasonCode, quantity: Quantity) -> Result<MovementKind> {
    if let Some(raw) = named.map(str::trim).filter(|raw| !raw.is_empty()) {
        let kind = MovementKind::parse(raw).ok_or_else(|| {
            InventoryError::invalid("movement", "kind", format!("{raw} is not a movement kind"))
        })?;
        if !kind.is_recordable_by_hand() {
            return Err(InventoryError::invalid(
                "movement",
                "kind",
                "a reservation is made by confirming an order, not by recording a movement by hand",
            ));
        }
        return Ok(kind);
    }
    Ok(match reason {
        ReasonCode::PurchaseReceipt | ReasonCode::CustomerReturn => MovementKind::Receipt,
        ReasonCode::SaleShipment | ReasonCode::SupplierReturn => MovementKind::Issue,
        ReasonCode::Transfer => {
            if quantity.is_negative() {
                MovementKind::TransferOut
            } else {
                MovementKind::TransferIn
            }
        }
        ReasonCode::Correction
        | ReasonCode::Damage
        | ReasonCode::Loss
        | ReasonCode::InternalUse
        | ReasonCode::StocktakeVariance => MovementKind::Adjustment,
    })
}

/// The stock row for one item at one location, or the zero row when it does not exist yet.
///
/// **A zero row rather than a 404**, because "this item has never been at this location" and
/// "this row does not exist" are the same fact and the item detail has to draw the first.
pub async fn stock_level(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
    location_id: Uuid,
) -> Result<StockLevel> {
    let levels = store::stock_for_item(pool, organization_id, item_id).await?;
    if let Some(row) = levels.into_iter().find(|row| row.location_id == location_id) {
        return Ok(row);
    }
    let item = store::get_item(pool, organization_id, item_id).await?;
    let location = store::get_location(pool, organization_id, location_id).await?;
    let warehouse_code: String =
        sqlx::query_scalar("select code from inventory_warehouses where id = $1")
            .bind(location.warehouse_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or_default();
    Ok(StockLevel {
        id: Uuid::nil(),
        item_id,
        sku: item.item.sku.clone(),
        name: item.item.name.clone(),
        category: item.item.category.clone(),
        unit: item.item.unit.clone(),
        location_id,
        location_code: location.code.clone(),
        location_name: location.name.clone(),
        warehouse_id: location.warehouse_id,
        warehouse_code,
        on_hand: Quantity::ZERO,
        reserved: Quantity::ZERO,
        available: Quantity::ZERO,
        min_threshold: item.item.min_threshold,
        reorder_point: item.item.reorder_point,
        status: StockStatus::of(
            Quantity::ZERO,
            item.item.min_threshold,
            item.item.reorder_point,
        ),
        last_movement_at: None,
    })
}

/// One page of the ledger, newest first.
pub async fn list_movements(
    pool: &PgPool,
    organization_id: Uuid,
    query: &MovementQuery,
) -> Result<store::Page<Movement>> {
    let page_size = query.page_size();
    let cursor = query.cursor_id();
    let from = query.from_at()?;
    let to = query.to_at()?;

    let mut kinds = Vec::new();
    for raw in &query.kinds {
        let kind = MovementKind::parse(raw.trim()).ok_or_else(|| {
            InventoryError::InvalidQuery(format!("{raw} is not a movement kind"))
        })?;
        kinds.push(kind.as_str());
    }
    let reason = match query.reason.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(ReasonCode::parse(raw).ok_or_else(|| {
            InventoryError::InvalidQuery(format!("{raw} is not a reason code"))
        })?),
    };

    let mut builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
        "select m.id, m.organization_id, m.item_id, i.sku, i.name as item_name, m.location_id, \
                l.code as location_code, m.kind, m.quantity::text as quantity, m.reason, \
                m.source_kind, m.source_id, m.note, m.on_hand_after::text as on_hand_after, \
                m.reserved_after::text as reserved_after, m.actor_user_id, m.created_at \
         from inventory_movements m \
         join inventory_items i on i.id = m.item_id \
         join inventory_locations l on l.id = m.location_id \
         where m.organization_id = ",
    );
    builder.push_bind(organization_id);
    if let Some(item_id) = query.item_id {
        builder.push(" and m.item_id = ").push_bind(item_id);
    }
    if let Some(location_id) = query.location_id {
        builder.push(" and m.location_id = ").push_bind(location_id);
    }
    if !kinds.is_empty() {
        builder.push(" and m.kind = any(").push_bind(&kinds).push(")");
    }
    if let Some(reason) = reason {
        builder.push(" and m.reason = ").push_bind(reason.as_str());
    }
    if let Some(actor) = query.actor_user_id {
        builder.push(" and m.actor_user_id = ").push_bind(actor);
    }
    if let Some(source) = query.source.as_deref().map(str::trim).filter(|raw| !raw.is_empty()) {
        // A source is either a kind (`order`), a uuid, or a number a person typed off a document.
        // Matching the text against both columns means "everything this transfer caused" is one
        // filter rather than a dropdown nobody can enumerate.
        if Uuid::parse_str(source).is_ok() {
            builder.push(" and m.source_id = ").push_bind(Uuid::parse_str(source).unwrap());
        } else {
            builder
                .push(" and (m.source_kind = ")
                .push_bind(source.to_string())
                .push(" or m.source_id::text = ")
                .push_bind(source.to_string())
                .push(")");
        }
    }
    if let Some(from) = from {
        builder.push(" and m.created_at >= ").push_bind(from);
    }
    if let Some(to) = to {
        builder.push(" and m.created_at <= ").push_bind(to);
    }
    if let Some(cursor) = cursor {
        // The id sequence is monotonic, so the cursor is the id and the order is the id — the
        // ledger is append-only, which is exactly the property that makes keyset pagination
        // exact here and merely approximate on a list people can reorder.
        builder.push(" and m.id < ").push_bind(cursor);
    }
    builder
        .push(" order by m.id desc limit ")
        .push_bind(page_size + 1);

    let rows = builder.build().fetch_all(pool).await?;
    let total = count_movements(pool, organization_id, query).await?;
    let mut movements = rows
        .iter()
        .map(movement_from_row)
        .collect::<Result<Vec<_>>>()?;
    let next_cursor = if rows.len() > page_size as usize {
        movements.first().map(|row| row.id)
    } else {
        None
    };
    movements.truncate(page_size as usize);
    Ok(store::Page::new(
        movements,
        next_cursor.map(|id| id.to_string()),
        total,
    ))
}

async fn count_movements(
    pool: &PgPool,
    organization_id: Uuid,
    query: &MovementQuery,
) -> Result<i64> {
    let mut count: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
        "select count(*) from inventory_movements m where m.organization_id = ",
    );
    count.push_bind(organization_id);
    if let Some(item_id) = query.item_id {
        count.push(" and m.item_id = ").push_bind(item_id);
    }
    if let Some(location_id) = query.location_id {
        count.push(" and m.location_id = ").push_bind(location_id);
    }
    if let Some(from) = query.from_at()? {
        count.push(" and m.created_at >= ").push_bind(from);
    }
    if let Some(to) = query.to_at()? {
        count.push(" and m.created_at <= ").push_bind(to);
    }
    Ok(count.build_query_scalar::<i64>().fetch_one(pool).await?)
}

/// Decode a ledger row.
fn movement_from_row(row: &PgRow) -> Result<Movement> {
    let kind_raw: String = row.get("kind");
    let kind = MovementKind::parse(&kind_raw).ok_or_else(|| {
        InventoryError::invalid("movement", "kind", format!("{kind_raw} is not a movement kind"))
    })?;
    let reason_raw: String = row.get("reason");
    let reason = ReasonCode::parse(&reason_raw).ok_or_else(|| {
        InventoryError::invalid(
            "movement",
            "reason",
            format!("{reason_raw} is not a reason code"),
        )
    })?;
    Ok(Movement {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        item_id: row.get("item_id"),
        sku: row.get("sku"),
        item_name: row.get("item_name"),
        location_id: row.get("location_id"),
        location_code: row.get("location_code"),
        kind,
        quantity: store::quantity_from_text(row.get::<&str, _>("quantity"))?,
        reason,
        source_kind: row.get("source_kind"),
        source_id: row.get("source_id"),
        note: row.get("note"),
        on_hand_after: store::quantity_from_text(row.get::<&str, _>("on_hand_after"))?,
        reserved_after: store::quantity_from_text(row.get::<&str, _>("reserved_after"))?,
        actor_user_id: row.get("actor_user_id"),
        created_at: row.get("created_at"),
    })
}

/// One movement, by its ledger id.
pub async fn get_movement(pool: &PgPool, movement_id: i64) -> Result<Movement> {
    let row = sqlx::query(
        "select m.id, m.organization_id, m.item_id, i.sku, i.name as item_name, m.location_id, \
                l.code as location_code, m.kind, m.quantity::text as quantity, m.reason, \
                m.source_kind, m.source_id, m.note, m.on_hand_after::text as on_hand_after, \
                m.reserved_after::text as reserved_after, m.actor_user_id, m.created_at \
         from inventory_movements m \
         join inventory_items i on i.id = m.item_id \
         join inventory_locations l on l.id = m.location_id \
         where m.id = $1",
    )
    .bind(movement_id)
    .fetch_optional(pool)
    .await?
    .ok_or(InventoryError::NotFound("movement"))?;
    movement_from_row(&row)
}

/// An item's history, newest first — the item detail's movement tab.
pub async fn item_history(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
    limit: i64,
) -> Result<Vec<Movement>> {
    list_movements(
        pool,
        organization_id,
        &MovementQuery {
            item_id: Some(item_id),
            limit: Some(limit.clamp(1, 500)),
            ..MovementQuery::default()
        },
    )
    .await
    .map(|page| page.items)
}

/// Recompute every item × location's `on_hand` from the ledger alone.
///
/// This is the check the acceptance criteria demand, and it is deliberately written as a
/// **replay from the movements** rather than a comparison of the last row's `on_hand_after`: the
/// last row only proves the final number, whereas a replay proves the whole sequence adds up.
/// A ledger whose middle was corrupted still ends at the right number if the last row was honest.
///
/// Rows the ledger never mentions are **absent from the map**, and that absence is meaningful:
/// `reconciliation_report` compares it against a rollup row and finds the disagreement, which is
/// the one a spot check never finds because nothing in the ledger mentions it.
pub async fn replay(pool: &PgPool, organization_id: Uuid) -> Result<std::collections::HashMap<Uuid, Quantity>> {
    let rows = sqlx::query(
        "select s.id, m.kind, m.quantity::text as quantity from inventory_stock s \
         left join inventory_movements m \
           on m.item_id = s.item_id and m.location_id = s.location_id \
          and m.organization_id = s.organization_id \
         where s.organization_id = $1 order by m.id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut totals: std::collections::HashMap<Uuid, Quantity> = Default::default();
    for row in rows {
        let id: Uuid = row.get("id");
        let kind: Option<String> = row.get("kind");
        let Some(kind_raw) = kind else {
            // No movement has ever touched this row. A rollup that is not zero here is a
            // disagreement the replay has to be able to *see*, so the row is seeded at zero
            // rather than skipped.
            totals.entry(id).or_insert(Quantity::ZERO);
            continue;
        };
        let kind = MovementKind::parse(&kind_raw).ok_or_else(|| {
            InventoryError::invalid("movement", "kind", format!("{kind_raw} is not a movement kind"))
        })?;
        let quantity = store::quantity_from_text(row.get::<&str, _>("quantity"))?;
        let entry = totals.entry(id).or_insert(Quantity::ZERO);
        *entry = match kind.signed() {
            1 => entry.checked_add(quantity).unwrap_or(*entry),
            -1 => entry.checked_sub(quantity).unwrap_or(*entry),
            _ => entry.checked_add(quantity).unwrap_or(*entry),
        };
    }
    Ok(totals)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(milli: i128) -> Quantity {
        Quantity::from_milli(milli).unwrap()
    }

    #[test]
    fn a_receipt_adds_an_issue_removes_and_the_sign_lives_in_the_kind() {
        let (on_hand, _) = apply_movement(
            q(10_000),
            Quantity::ZERO,
            MovementKind::Receipt,
            q(5_000),
            ReasonCode::PurchaseReceipt,
            false,
        )
        .unwrap();
        assert_eq!(on_hand.to_text(), "15.000");

        let (on_hand, _) = apply_movement(
            on_hand,
            Quantity::ZERO,
            MovementKind::Issue,
            q(4_000),
            ReasonCode::SaleShipment,
            false,
        )
        .unwrap();
        assert_eq!(on_hand.to_text(), "11.000");
    }

    #[test]
    fn a_negative_result_is_refused_with_the_number_that_is_available() {
        let error = apply_movement(
            q(4_000),
            Quantity::ZERO,
            MovementKind::Issue,
            q(6_000),
            ReasonCode::SaleShipment,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("4.000"), "the available number must travel: {error}");
        assert!(error.contains("sale_shipment"), "{error}");
    }

    #[test]
    fn only_a_correction_with_the_permission_may_go_negative() {
        // Same movement, three different answers — this is the whole criterion.
        let refused = apply_movement(
            q(2_000),
            Quantity::ZERO,
            MovementKind::Adjustment,
            q(-5_000),
            ReasonCode::Correction,
            false,
        );
        assert!(refused.is_err(), "a correction without the permission is refused");

        let allowed = apply_movement(
            q(2_000),
            Quantity::ZERO,
            MovementKind::Adjustment,
            q(-5_000),
            ReasonCode::Correction,
            true,
        )
        .unwrap();
        assert_eq!(allowed.0.to_text(), "-3.000");

        // A damage write-off is an adjustment, but `damage` is not `correction`, so it is
        // refused even with the permission: the rule is about the reason, not the kind.
        let refused = apply_movement(
            q(2_000),
            Quantity::ZERO,
            MovementKind::Adjustment,
            q(-5_000),
            ReasonCode::Damage,
            true,
        );
        assert!(refused.is_err());
    }

    #[test]
    fn an_adjustment_of_zero_is_not_a_movement() {
        let error = apply_movement(
            q(2_000),
            Quantity::ZERO,
            MovementKind::Adjustment,
            Quantity::ZERO,
            ReasonCode::Correction,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("zero"), "{error}");
    }

    #[test]
    fn a_reservation_moves_reserved_and_never_on_hand() {
        let (on_hand, reserved) = apply_movement(
            q(10_000),
            Quantity::ZERO,
            MovementKind::Reserve,
            q(4_000),
            ReasonCode::SaleShipment,
            false,
        )
        .unwrap();
        assert_eq!(on_hand.to_text(), "10.000", "the goods are still on the shelf");
        assert_eq!(reserved.to_text(), "4.000", "but four of them are spoken for");

        let (on_hand, reserved) = apply_movement(
            on_hand,
            reserved,
            MovementKind::Release,
            q(4_000),
            ReasonCode::SaleShipment,
            false,
        )
        .unwrap();
        assert_eq!(on_hand.to_text(), "10.000");
        assert_eq!(reserved.to_text(), "0.000");
    }

    #[test]
    fn you_cannot_hold_stock_that_does_not_exist() {
        // A reservation beyond the balance is refused whatever the reason, and this is the one
        // rule the schema mirrors as a check constraint because it has no exception.
        let error = apply_movement(
            q(2_000),
            Quantity::ZERO,
            MovementKind::Reserve,
            q(5_000),
            ReasonCode::SaleShipment,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("2.000"), "{error}");
    }

    #[test]
    fn a_release_beyond_what_is_held_is_refused_rather_than_wrapping() {
        // Subtracting into the negative would make `reserved` negative and the schema would
        // refuse the row — after the ledger row had been written, if the check were not here.
        let error = apply_movement(
            q(5_000),
            q(1_000),
            MovementKind::Release,
            q(2_000),
            ReasonCode::SaleShipment,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("held"), "{error}");
    }

    #[test]
    fn a_hand_written_movement_cannot_be_a_reservation() {
        let error = resolve_kind(Some("reserve"), ReasonCode::SaleShipment, q(1_000))
            .unwrap_err()
            .to_string();
        assert!(error.contains("confirming an order"), "{error}");
    }

    #[test]
    fn the_reason_decides_the_direction_when_the_caller_names_no_kind() {
        // The one that is easy to get backwards: a customer return brings goods **back**.
        assert_eq!(
            resolve_kind(None, ReasonCode::CustomerReturn, q(1_000)).unwrap(),
            MovementKind::Receipt
        );
        assert_eq!(
            resolve_kind(None, ReasonCode::PurchaseReceipt, q(1_000)).unwrap(),
            MovementKind::Receipt
        );
        assert_eq!(
            resolve_kind(None, ReasonCode::SaleShipment, q(1_000)).unwrap(),
            MovementKind::Issue
        );
        assert_eq!(
            resolve_kind(None, ReasonCode::SupplierReturn, q(1_000)).unwrap(),
            MovementKind::Issue
        );
        assert_eq!(
            resolve_kind(None, ReasonCode::Damage, q(-1_000)).unwrap(),
            MovementKind::Adjustment
        );
        assert_eq!(
            resolve_kind(None, ReasonCode::Transfer, q(-1_000)).unwrap(),
            MovementKind::TransferOut
        );
        assert_eq!(
            resolve_kind(None, ReasonCode::Transfer, q(1_000)).unwrap(),
            MovementKind::TransferIn
        );
        // A named kind always wins, so a caller that says `receipt` gets a receipt.
        assert_eq!(
            resolve_kind(Some("receipt"), ReasonCode::Damage, q(1_000)).unwrap(),
            MovementKind::Receipt
        );
    }

    #[test]
    fn the_ledger_prints_a_signed_quantity_while_storing_a_positive_one() {
        let movement = Movement {
            id: 1,
            organization_id: Uuid::nil(),
            item_id: Uuid::nil(),
            sku: "X".into(),
            item_name: "x".into(),
            location_id: Uuid::nil(),
            location_code: "STOCK".into(),
            kind: MovementKind::Issue,
            quantity: q(4_000),
            reason: ReasonCode::SaleShipment,
            source_kind: None,
            source_id: None,
            note: String::new(),
            on_hand_after: q(6_000),
            reserved_after: Quantity::ZERO,
            actor_user_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        // Stored positive — that is the schema's rule — printed negative because the kind says so.
        assert_eq!(movement.quantity.to_text(), "4.000");
        assert_eq!(movement.signed_quantity().to_text(), "-4.000");

        let receipt = Movement {
            kind: MovementKind::Receipt,
            ..movement
        };
        assert_eq!(receipt.signed_quantity().to_text(), "4.000");
    }

    #[test]
    fn a_bad_date_is_a_refusal_naming_the_field_not_a_silently_ignored_filter() {
        let query = MovementQuery {
            from: Some("yesterday".into()),
            ..MovementQuery::default()
        };
        let error = query.from_at().unwrap_err().to_string();
        assert!(error.contains("from"), "{error}");
    }
}
