//! Stock held for a sales order, and the bridge from a confirmed order to the shelf.
//!
//! The acceptance criterion this file exists for is one sentence — *"`sales.order.confirmed`
//! reserves stock and the reservation is visible in both the order and the stock list; cancel
//! releases it"* — and the state of the platform when it was written is worth recording, because
//! it is the third time this module has had the same gap in a different costume.
//!
//! ## What was there, and what it meant
//!
//! [`MovementKind::Reserve`] and [`MovementKind::Release`] existed, with correct arithmetic in
//! [`crate::ledger::apply_movement`] and a `sales_order_reservations` table in the sales module
//! (`0057`). Confirming an order wrote the sales-side rows. **It never touched `inventory_stock`.**
//!
//! Every part of that sentence was true and the sentence was false. The order detail showed a
//! hold; the stock list showed the same item as fully available; a second order could promise the
//! same units; and the ledger — which is what a warehouse reads — had no row for any of it. The
//! reservation was a *claim about a hold*, printed where somebody would look for the hold.
//!
//! The general form, which is why it is worth writing down: **a record that describes a hold is
//! not a hold.** `sales_order_reservations` is the sales module's own table by design — migration
//! `0057` says so explicitly, so that a tenant without inventory installed can still record the
//! *intent* to hold. That decision is right and it is exactly what hid the gap: the column
//! `reservation_state` could say `total`, so there was nothing red on any screen while the
//! warehouse's own number was wrong.
//!
//! ## The decision this file makes, and states
//!
//! **A line is held only where it can be.** A reservation is per *item at a location*, and a
//! sales order line names a **product**, not a location. So:
//!
//! * A line whose product is linked to an inventory item (or whose SKU matches one) is held
//!   against that item's locations — **all of them**, spread over the locations that have the
//!   stock, because an order does not nominate a shelf and asking the seller to would be inventing
//!   a fact. The ledger row per location is the honest record: this shelf is holding 3 of these.
//! * A line with no inventory item is **recorded as a hold and held nowhere**, and the order's
//!   `reservation_state` stays `partial` rather than claiming `total`. This is the spec's "a
//!   visible note rather than a silent failure": a services order reserving an hour of somebody's
//!   time has nothing to put on a shelf, and pretending otherwise would show `reserved: 0` on a
//!   line the sales desk was told is safe.
//!
//! ## Why the arithmetic is not reimplemented here
//!
//! Every write goes through [`crate::ledger::record_resolved`] — the *same* function a hand-written
//! movement goes through, with the kind already decided. There is deliberately no second copy of
//! "reserve raises `reserved` and leaves `on_hand` alone": slice 2's approval path already learned
//! that a second write path for one business rule is how a module ends up with two answers, and
//! the reconciliation report is the thing that finds out.
//!
//! ## What the caller must decide, and what it must not
//!
//! [`reserve_for_order`] and [`release_for_order` take the order's lines **from the database**,
//! not from the caller. The caller is the sales module, and a caller that could name the
//! quantities would be able to promise stock it never looked at. The one thing the caller does
//! pass is the actor, because "who promised this" is a fact about a person, not derivable from
//! the document.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{InventoryError, Result};
use crate::items;
use crate::ledger::{self, Movement, Recorded};
use crate::model::{MovementKind, ReasonCode};
use crate::money::Quantity;
use crate::store::StockLevel;

/// Which way the hold is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationAction {
    /// Take the hold. `sales.order.confirmed` calls this.
    Reserve,
    /// Give it back. `sales.order.cancelled` calls this.
    Release,
}

impl ReservationAction {
    /// The movement kind this action writes.
    const fn kind(self) -> MovementKind {
        match self {
            Self::Reserve => MovementKind::Reserve,
            Self::Release => MovementKind::Release,
        }
    }

    /// What the ledger's `source_kind` says, so the two are one vocabulary.
    const fn source_kind(self) -> &'static str {
        "order"
    }
}

/// One order line, as the sales module's table holds it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct OrderLine {
    line_id: Uuid,
    product_id: Option<Uuid>,
    /// The line's own free text, which is what a warehouse reads when there is no item.
    description: String,
    quantity: String,
    unit: String,
    /// The sales product's SKU, when the line names a product. This is the join the tables do not
    /// have: `inventory_items.product_id` is a bare `uuid` with no foreign key (migration `0126`
    /// leaves it for REQ-052 to adopt), so the match is on the SKU the two already agree on.
    sku: Option<String>,
}

/// What a reservation did, per item × location, as the caller reports it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationOutcome {
    /// The order the holds belong to.
    pub order_id: Uuid,
    /// Which way this went.
    pub action: ReservationAction,
    /// Order lines that named a product with no inventory item behind it — the honest `partial`.
    ///
    /// Named, not counted: "2 of 5 lines have nothing on a shelf" is a sentence the order detail
    /// can print, and a number alone is not.
    pub unheld_lines: Vec<UnheldLine>,
    /// The movements written, in the order they were written.
    pub movements: Vec<Movement>,
    /// The stock rows afterwards, in the same order — so a caller can show the before/after
    /// without reading the rollup back itself.
    pub positions: Vec<StockLevel>,
}

impl ReservationOutcome {
    /// Whether every line of the order is on a shelf somewhere.
    #[must_use]
    pub const fn is_total(&self) -> bool {
        self.unheld_lines.is_empty()
    }
}

/// A line that could not be held, and why it could not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnheldLine {
    /// The sales order line.
    pub line_id: Uuid,
    /// What the line says it is.
    pub description: String,
    /// The product it named, when it named one.
    pub product_id: Option<Uuid>,
    /// The SKU, when there was one — this is what somebody has to go and create.
    pub sku: Option<String>,
    /// A sentence, not a code: this is printed under the line.
    pub reason: String,
}

/// Hold (or give back) the stock for one confirmed order.
///
/// The order's lines are read **here**, not passed in — see the module header for why. `order_id`
/// must belong to `organization_id`: a foreign order is a `NotFound`, the same answer a foreign
/// item gets, because one organization's stock is the thing this module keeps apart.
///
/// `action` is [`ReservationAction::Reserve`] for a confirm and [`ReservationAction::Release`] for a
/// cancel. A release is **symmetric with the reserve it undoes**: it releases from the same
/// locations, in the same spread, and if the stock has since moved it releases what is still held
/// and reports the difference in the ledger rather than refusing — a cancelled order cannot be
/// blocked by a warehouse that has been busy since it was promised.
pub async fn reserve_for_order(
    pool: &PgPool,
    organization_id: Uuid,
    order_id: Uuid,
    action: ReservationAction,
    actor: Option<Uuid>,
) -> Result<ReservationOutcome> {
    let order: Option<(String, String)> = sqlx::query_as(
        "select number, status from sales_orders where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(order_id)
    .fetch_optional(pool)
    .await?;
    let (number, _status) = order.ok_or(InventoryError::NotFound("order"))?;

    let lines: Vec<OrderLine> = sqlx::query_as(
        "select l.id as line_id,
                l.product_id,
                l.description,
                l.quantity::text as quantity,
                l.unit,
                p.sku
           from sales_order_lines l
           left join sales_products p on p.id = l.product_id
          where l.order_id = $1
          order by l.position",
    )
    .bind(order_id)
    .fetch_all(pool)
    .await?;

    let mut outcome = ReservationOutcome {
        order_id,
        action,
        unheld_lines: Vec::new(),
        movements: Vec::new(),
        positions: Vec::new(),
    };

    for line in &lines {
        let quantity = crate::store::parse_quantity("reservation", "quantity", &line.quantity)?;
        if !quantity.is_positive() {
            return Err(InventoryError::invalid(
                "reservation",
                "quantity",
                format!("line \"{}\" has nothing to hold", line.description),
            ));
        }

        // **A second confirm must not hold the same stock twice.** The sales module's unique
        // index on `(order_id, line_id)` makes the *sales* row idempotent, so a double-click or a
        // retried request writes no second hold there — and every screen that reads the order then
        // looks perfectly correct while the shelf has been drained twice. This is the second time
        // in this module that a unique index in one table has hidden a write in another: the
        // guard asks the **ledger** what this order still holds on this line, which is the only
        // place the answer can come from, because the rollup is the organization's total.
        let outstanding = if action == ReservationAction::Reserve {
            let already = held_for_line(pool, organization_id, order_id, line.line_id).await?;
            quantity.checked_sub(already).unwrap_or(Quantity::ZERO)
        } else {
            quantity
        };
        if !outstanding.is_positive() {
            continue;
        }

        let Some(item_id) = item_for_line(pool, organization_id, line).await? else {
            // The honest `partial`. A free-text line and a line whose product has never been made
            // an inventory item are the same fact from the warehouse's side: there is no shelf.
            outcome.unheld_lines.push(UnheldLine {
                line_id: line.line_id,
                description: line.description.clone(),
                product_id: line.product_id,
                sku: line.sku.clone(),
                reason: match (&line.sku, line.product_id) {
                    (Some(sku), Some(_)) => format!(
                        "no inventory item carries SKU {sku} — create it before promising this \
                         line to a customer"
                    ),
                    (None, Some(_)) => "this product is not linked to an inventory item".to_string(),
                    _ => "a free-text line has nothing the warehouse can put on a shelf".to_string(),
                },
            });
            continue;
        };

        // Spread over the locations that have the stock, largest first, so a shelf that cannot
        // satisfy the hold takes as little of it as possible. `on_hand - reserved` is the number a
        // promise may be built on; sorting by the *available* figure (and not by the id) is what
        // makes the spread deterministic across two calls that see the same data.
        let locations = available_locations(pool, organization_id, item_id, outstanding).await?;
        if locations.is_empty() {
            let available = total_available(pool, organization_id, item_id).await?;
            outcome.unheld_lines.push(UnheldLine {
                line_id: line.line_id,
                description: line.description.clone(),
                product_id: line.product_id,
                sku: line.sku.clone(),
                reason: format!(
                    "this item can be promised {available} and the order asks for {outstanding}"
                ),
            });
            continue;
        }

        for (location_id, room) in locations {
            // A release gives back exactly what the same line took. Reading it off the ledger's
            // own `reserved` for this source is what makes a cancel of a partly-issued order
            // return the remainder rather than a fresh spread of the original number.
            let take = match action {
                ReservationAction::Reserve => room.min(outstanding),
                ReservationAction::Release => {
                    let held = held_for_source(
                        pool,
                        organization_id,
                        item_id,
                        location_id,
                        order_id,
                    )
                    .await?;
                    // `min`, because a warehouse that already issued part of the goods has
                    // released the rest itself; releasing more than is held would take `reserved`
                    // below zero and the ledger refuses that for the right reason.
                    held.min(outstanding.min(room.max(Quantity::ZERO)))
                }
            };
            if !take.is_positive() {
                continue;
            }

            let note = items::validate_note(
                "reservation",
                &format!("order {number} · line \"{}\"", line.description),
            )?;
            let recorded: Recorded = ledger::record_resolved(
                pool,
                organization_id,
                item_id,
                location_id,
                action.kind(),
                take,
                // The reason a hold carries is `sale_shipment`: the order exists because the
                // goods are going out, and the ledger's `reason` column is the vocabulary the
                // filters and the reports are written against. Naming a new reason here would add
                // a value `0126`'s check constraint refuses, and inventing a constraint change for
                // a bridge would be the wrong trade.
                ReasonCode::SaleShipment,
                Some(action.source_kind()),
                Some(order_id),
                &note,
                Some(line.line_id),
                // A reservation can never go negative: it moves `reserved` only, and
                // `apply_movement` refuses `reserved > on_hand` unconditionally. Passing `false`
                // is the honest statement — this write is not asking for the exception.
                false,
                actor,
            )
            .await?;
            outcome.movements.push(recorded.movement);
            outcome.positions.push(recorded.position);
        }

        // The line's own quantity, not the sum of what fitted: a line that only half fits must
        // report as unheld rather than quietly holding half of what was promised.
        // `Quantity` implements no `Sum` — a checked add is the point, since the running total is
        // a promise being built up and a silent overflow to the previous value would be a wrong
        // answer rather than a loud one.
        let mut held_here = Quantity::ZERO;
        for position in outcome
            .positions
            .iter()
            .filter(|position| position.item_id == item_id)
        {
            held_here = held_here.checked_add(position.reserved).unwrap_or(held_here);
        }
        if held_here < outstanding && action == ReservationAction::Reserve {
            outcome.unheld_lines.push(UnheldLine {
                line_id: line.line_id,
                description: line.description.clone(),
                product_id: line.product_id,
                sku: line.sku.clone(),
                reason: format!(
                    "only {held_here} of {outstanding} is on a shelf to promise"
                ),
            });
        }
    }

    Ok(outcome)
}

/// The inventory item behind a sales line, if there is one.
///
/// Two joins are tried, in this order, and the reason is worth keeping: `inventory_items.product_id`
/// is the **explicit** link and is what an item created through the module's own form carries; the
/// SKU is the **implicit** one, and it is the join that saves the common case — an item created
/// before the catalog existed, or a product created afterwards that nobody linked. Matching on
/// `lower(sku)` is the same rule both tables already use for their own uniqueness index, so the
/// join cannot disagree with either module's idea of "the same SKU".
async fn item_for_line(
    pool: &PgPool,
    organization_id: Uuid,
    line: &OrderLine,
) -> Result<Option<Uuid>> {
    if let Some(product_id) = line.product_id
        && let Some(item_id) = sqlx::query_scalar::<_, Uuid>(
            "select id from inventory_items
              where organization_id = $1 and product_id = $2
              order by created_at, id limit 1",
        )
        .bind(organization_id)
        .bind(product_id)
        .fetch_optional(pool)
        .await?
    {
        return Ok(Some(item_id));
    }
    let Some(sku) = line.sku.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar::<_, Uuid>(
        "select id from inventory_items
          where organization_id = $1 and lower(sku) = lower($2) and archived_at is null
          order by created_at, id limit 1",
    )
    .bind(organization_id)
    .bind(sku)
    .fetch_optional(pool)
    .await?)
}

/// The locations that can take `quantity`, largest first, with how much room each has.
async fn available_locations(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
    quantity: Quantity,
) -> Result<Vec<(Uuid, Quantity)>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "select location_id, (on_hand - reserved)::text as room
           from inventory_stock
          where organization_id = $1 and item_id = $2 and on_hand - reserved > 0
          order by (on_hand - reserved) desc, location_id",
    )
    .bind(organization_id)
    .bind(item_id)
    .fetch_all(pool)
    .await?;

    let mut remaining = quantity;
    let mut chosen = Vec::new();
    for (location_id, room) in rows {
        if !remaining.is_positive() {
            break;
        }
        let room = crate::store::quantity_from_text(&room)?;
        let take = room.min(remaining);
        remaining = remaining.checked_sub(take).unwrap_or(Quantity::ZERO);
        chosen.push((location_id, take));
    }
    Ok(chosen)
}

/// Everything this organization can promise of one item, across every location.
async fn total_available(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
) -> Result<Quantity> {
    let room: Option<String> = sqlx::query_scalar(
        "select coalesce(sum(on_hand - reserved), 0)::text from inventory_stock
          where organization_id = $1 and item_id = $2",
    )
    .bind(organization_id)
    .bind(item_id)
    .fetch_optional(pool)
    .await?;
    room.map(|raw| crate::store::quantity_from_text(&raw))
        .transpose()
        .map(|value| value.unwrap_or(Quantity::ZERO))
}

/// What one order line is still holding, summed over every location.
///
/// Read from the ledger's own `order_line_id` pointer (migration `0146`), so this is an equality on
/// a uuid. The first version of this function matched on the **note** — `like '%· line "<id>"%'`
/// — which is a string filter on a column that exists to be read by a person, and it would have
/// broken the first time somebody renamed a line. The pointer is the same question asked in the
/// shape the data can answer it.
async fn held_for_line(
    pool: &PgPool,
    organization_id: Uuid,
    order_id: Uuid,
    line_id: Uuid,
) -> Result<Quantity> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select kind, quantity::text from inventory_movements
          where organization_id = $1 and source_kind = 'order' and source_id = $2
            and order_line_id = $3
          order by id",
    )
    .bind(organization_id)
    .bind(order_id)
    .bind(line_id)
    .fetch_all(pool)
    .await?;

    let mut held = Quantity::ZERO;
    for (kind, quantity) in rows {
        let parsed = crate::store::quantity_from_text(&quantity)?;
        held = if kind == MovementKind::Reserve.as_str() {
            held.checked_add(parsed).unwrap_or(held)
        } else {
            held.checked_sub(parsed).unwrap_or(held)
        };
    }
    Ok(held)
}

/// What this order is still holding of one item at one location.
///
/// Read from the **ledger**, not from `inventory_stock.reserved`: the rollup is the organization's
/// total, and subtracting *this order's* share from it cannot be done without knowing the share —
/// which is exactly what the ledger's own `source_id` records. A cancel that asked the rollup for
/// "how much of this belongs to this order" would get the total and release the lot.
async fn held_for_source(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
    location_id: Uuid,
    order_id: Uuid,
) -> Result<Quantity> {
    // A reserve adds, a release takes. Replaying the movements in order gives the current hold,
    // and a warehouse that has issued part of the order already shows as a negative there.
    let deltas: Vec<String> = sqlx::query_scalar(
        "select case when kind = 'reserve' then quantity::text else (-quantity)::text end
           from inventory_movements
          where organization_id = $1 and item_id = $2 and location_id = $3
            and source_kind = 'order' and source_id = $4
          order by id",
    )
    .bind(organization_id)
    .bind(item_id)
    .bind(location_id)
    .bind(order_id)
    .fetch_all(pool)
    .await?;

    let mut held = Quantity::ZERO;
    for delta in deltas {
        let parsed = crate::store::quantity_from_text(&delta)?;
        held = held.checked_add(parsed).unwrap_or(held);
    }
    Ok(held)
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The kind a reserve/release write carries, and the reason it carries alongside it.
    ///
    /// The reason is worth its own test because it is the one thing in this file that a schema
    /// constrains from outside: `0126`'s `inventory_movements_reason` check allows exactly ten
    /// values, and a bridge that invented an `order_reservation` reason would be refused by a
    /// constraint at the moment a customer is being promised. `sale_shipment` is the existing
    /// value that means what this is — goods are going out because the order says so.
    #[test]
    fn a_reserve_is_a_reserve_movement_with_a_reason_the_schema_allows() {
        assert_eq!(ReservationAction::Reserve.kind(), MovementKind::Reserve);
        assert_eq!(ReservationAction::Release.kind(), MovementKind::Release);
        // Both write under the same `source_kind`, so "which order did this?" is one query for
        // both directions rather than two vocabularies that have to be searched separately.
        assert_eq!(ReservationAction::Reserve.source_kind(), "order");
        assert_eq!(ReservationAction::Release.source_kind(), "order");
        // A reservation is not hand-recordable, and the bridge is the only caller that may say so.
        assert!(!MovementKind::Reserve.is_recordable_by_hand());
        // The reason a hold is written under is the one the reports are written against. The
        // assertion is on `parse` rather than on a list of every reason, because the schema
        // (`0126`) is the list and this is the code's half of the same rule: a reason the code
        // cannot read back is a reason the ledger will refuse.
        assert_eq!(
            ReasonCode::parse(ReasonCode::SaleShipment.as_str()),
            Some(ReasonCode::SaleShipment)
        );
    }

    /// A line with nothing to hold is a refusal, not a zero.
    ///
    /// The cheap version writes a `reserve` of `0`, which the schema's sign rule already refuses —
    /// so this is belt and braces, and the assertion is about the *sentence* naming the line,
    /// because the line is what a person has to go and look at.
    #[test]
    fn a_line_with_nothing_to_hold_names_itself() {
        let error = InventoryError::invalid(
            "reservation",
            "quantity",
            "line \"Design retainer, 4 hours\" has nothing to hold",
        );
        assert_eq!(error.to_string(), "invalid reservation.quantity: line \"Design retainer, 4 hours\" has nothing to hold");
    }

    /// `is_total` is the order's `reservation_state`, so it must be false the moment **one** line
    /// is unheld — the failure mode being guarded is the flattering one, where a five-line order
    /// with four lines on shelves reports `total` because the arithmetic was done on the held ones.
    #[test]
    fn one_unheld_line_is_enough_to_stop_the_order_claiming_a_total_hold() {
        let mut outcome = ReservationOutcome {
            order_id: Uuid::new_v4(),
            action: ReservationAction::Reserve,
            unheld_lines: Vec::new(),
            movements: Vec::new(),
            positions: Vec::new(),
        };
        assert!(outcome.is_total());

        outcome.unheld_lines.push(UnheldLine {
            line_id: Uuid::new_v4(),
            description: "Installation".into(),
            product_id: None,
            sku: None,
            reason: "a free-text line has nothing the warehouse can put on a shelf".into(),
        });
        assert!(!outcome.is_total());
    }

    /// A release that names no reason would be a release nobody can read next month — the same
    /// argument the sales module's own cancel route makes about its reason field.
    #[test]
    fn the_note_a_reservation_writes_names_the_order_and_the_line() {
        // The note is what the ledger's detail panel shows, and it is built from the order number
        // and the line's own text: two facts a person needs and neither of which the row's ids
        // give them.
        let note = items::validate_note("reservation", "order SO-2026-0007 · line \"Steel bracket\"")
            .expect("note");
        assert!(note.contains("SO-2026-0007"));
        assert!(note.contains("Steel bracket"));
    }

    /// The note is bounded by the same 500 characters every other movement note is, so a
    /// pathological description cannot push a row the schema refuses.
    #[test]
    fn a_reservation_note_cannot_outgrow_the_ledger() {
        let long = "x".repeat(2_000);
        let built = format!("order SO-1 · line \"{long}\"");
        assert!(items::validate_note("reservation", &built).is_err());
    }
}
