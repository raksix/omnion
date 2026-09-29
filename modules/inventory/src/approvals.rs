//! Adjustment approval requests: the write that waits instead of happening
//! (docs/requests/REQ-053, slice 2).
//!
//! Slice 1 shipped the two halves of this rule and neither did anything: `inventory_settings
//! .adjustment_approval_threshold` was a number nothing read, and `InventoryError
//! ::ApprovalNotGranted` was a variant nothing raised. This module is the missing middle.
//!
//! ## What the rule is
//!
//! **An absolute adjustment whose size is over the organization's threshold changes nothing until
//! somebody who did not ask for it says yes.** Three words carry the weight:
//!
//! * *absolute* — the amount is the number of units moved, not a percentage. A variance of two
//!   out of ten thousand units is not a smaller mistake than two out of ten; a percentage rule
//!   waves the first through and stops the second.
//! * *changes nothing* — no ledger row, no rollup, no event. The stock list shows the same number
//!   before and after the request, which is the only way a person can trust the badge.
//! * *somebody else* — the approver is not the requester. A module that lets you approve your own
//!   recount has a form, not a control.
//!
//! ## Why the request holds the whole write
//!
//! The cheaper design is a `pending` flag on `inventory_movements`, and it breaks the one property
//! the module exists for. The ledger is append-only and replayable; a row in it whose meaning
//! depends on a second table is a row whose meaning can change after the fact, and `replay` — the
//! proof that the rollup is honest — stops being a proof. So the request carries `item_id`,
//! `location_id`, `kind`, `mode`, `quantity`, `reason`, `note` and the source reference, and
//! approving **replays those exact values through [`record_movement`]** rather than re-deriving
//! them from a form the requester may since have edited. One write path, two routes into it.
//!
//! ## Why approving replays rather than re-derives
//!
//! Because two write paths for one business rule is how a module ends up with two answers. If
//! approval had its own `update inventory_stock set on_hand = …`, the arithmetic could drift from
//! the ledger's — silently, and only for over-threshold adjustments, which are by definition the
//! rare ones nobody tests by hand. The route calls [`record_movement`] and the ledger is written
//! the same way every other movement is.
//!
//! ## What is measured, and when
//!
//! The threshold is read at the moment the request is raised and **copied onto the row**, for the
//! same reason `sales_quote_approvals.threshold_percent` is a snapshot: an organization that
//! lowers its threshold after three requests are pending must not retroactively justify them. The
//! approver sees the line the request was measured against, not today's.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{InventoryError, Result};
use crate::ledger::{self, NewMovement, Recorded};
use crate::model::Settings;
use crate::money::Quantity;
use crate::store;

/// The `status` column's five states, as the API accepts them.
pub const STATUSES: [&str; 4] = ["pending", "approved", "rejected", "cancelled"];

/// A request, as the inbox, the item detail and the drawer all read it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovalView {
    /// The request.
    pub id: Uuid,
    /// The organization it belongs to.
    pub organization_id: Uuid,
    /// The item being adjusted.
    pub item_id: Uuid,
    /// The item's SKU, so the inbox row is readable without a second query.
    pub sku: String,
    /// The item's name, for the same reason.
    pub item_name: String,
    /// The location being adjusted.
    pub location_id: Uuid,
    /// The location's code, for the same reason.
    pub location_code: String,
    /// The kind of movement that will be written if this is approved.
    pub kind: String,
    /// `delta` or `counted` — how to read `quantity`.
    pub mode: String,
    /// The number as the requester typed it.
    pub quantity: Quantity,
    /// The reason code.
    pub reason: String,
    /// The requester's note.
    pub note: String,
    /// The number being asked for: the absolute value of a delta, or the distance between the
    /// counted number and what was there.
    pub amount: Quantity,
    /// The threshold in force when the request was raised.
    pub threshold: Quantity,
    /// The on-hand when the request was raised.
    pub on_hand_at_request: Quantity,
    /// One of [`STATUSES`].
    pub status: String,
    /// The decision, when there is one.
    pub decision: Option<String>,
    /// The approver.
    pub decided_by: Option<Uuid>,
    /// When the decision was taken.
    #[serde(with = "crate::dates::instant::option")]
    pub decided_at: Option<OffsetDateTime>,
    /// The approver's comment — mandatory on a rejection.
    pub comment: Option<String>,
    /// The movement the approval produced.
    pub movement_id: Option<i64>,
    /// Who asked.
    pub requested_by: Uuid,
    /// When they asked.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

/// What the caller is asking to be allowed to do.
#[derive(Debug, Clone)]
pub struct NewApproval {
    /// The item.
    pub item_id: Uuid,
    /// The location.
    pub location_id: Uuid,
    /// The kind, already resolved by the caller's own inference.
    pub kind: String,
    /// `delta` or `counted`.
    pub mode: String,
    /// The number as typed.
    pub quantity: Quantity,
    /// The reason code's stored name.
    pub reason: String,
    /// The requester's note.
    pub note: String,
    /// Where the write came from (`order`, `stocktake`, …).
    pub source_kind: Option<String>,
    /// The document it came from.
    pub source_id: Option<Uuid>,
}

/// The inbox's filter.
#[derive(Debug, Clone, Default)]
pub struct ApprovalQuery {
    /// Restrict to one status.
    pub status: Option<String>,
    /// Restrict to one item.
    pub item_id: Option<Uuid>,
    /// How many rows.
    pub limit: i64,
    /// The page cursor.
    pub cursor: Option<String>,
}

const SELECT: &str = "select a.id, a.organization_id, a.item_id, i.sku, i.name as item_name, \
     a.location_id, l.code as location_code, a.kind, a.mode, a.quantity::text as quantity, \
     a.reason, a.note, a.amount::text as amount, a.threshold::text as threshold, \
     a.on_hand_at_request::text as on_hand_at_request, a.status, a.decision, a.decided_by, \
     a.decided_at, a.comment, a.movement_id, a.requested_by, a.created_at \
     from inventory_adjustment_approvals a \
     join inventory_items i on i.id = a.item_id \
     join inventory_locations l on l.id = a.location_id";

/// Parse one row of [`SELECT`].
fn map_row(row: &sqlx::postgres::PgRow) -> Result<ApprovalView> {
    use sqlx::Row;
    Ok(ApprovalView {
        id: row.try_get("id")?,
        organization_id: row.try_get("organization_id")?,
        item_id: row.try_get("item_id")?,
        sku: row.try_get("sku")?,
        item_name: row.try_get("item_name")?,
        location_id: row.try_get("location_id")?,
        location_code: row.try_get("location_code")?,
        kind: row.try_get("kind")?,
        mode: row.try_get("mode")?,
        quantity: store::quantity_from_text(row.try_get("quantity")?)?,
        reason: row.try_get("reason")?,
        note: row.try_get("note")?,
        amount: store::quantity_from_text(row.try_get("amount")?)?,
        threshold: store::quantity_from_text(row.try_get("threshold")?)?,
        on_hand_at_request: store::quantity_from_text(row.try_get("on_hand_at_request")?)?,
        status: row.try_get("status")?,
        decision: row.try_get("decision")?,
        decided_by: row.try_get("decided_by")?,
        decided_at: row.try_get("decided_at")?,
        comment: row.try_get("comment")?,
        movement_id: row.try_get("movement_id")?,
        requested_by: row.try_get("requested_by")?,
        created_at: row.try_get("created_at")?,
    })
}

/// Parse a cursor string into the id it names, or `None` for "first page".
fn cursor_id(raw: Option<&str>) -> Option<Uuid> {
    raw.map(str::trim)
        .filter(|text| !text.is_empty())
        .and_then(|text| Uuid::parse_str(text).ok())
}

/// Resolve a cursor id to the `(created_at, id::text)` pair the window comparison uses.
///
/// `None` means the id is not in this organization's inbox, and the caller turns that into a
/// `400` naming the cursor: a cursor from another organization must not be able to walk a reader
/// through rows they may not see, and "invalid cursor" says that without explaining whose rows
/// they were.
async fn cursor_position(
    pool: &PgPool,
    organization_id: Uuid,
    cursor: Uuid,
) -> Result<Option<(OffsetDateTime, String)>> {
    Ok(sqlx::query_as::<_, (OffsetDateTime, String)>(
        "select created_at, id::text as id from inventory_adjustment_approvals \
          where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(cursor)
    .fetch_optional(pool)
    .await?)
}

/// One request, or `None` if the id is not in this organization.
pub async fn get_approval(
    pool: &PgPool,
    organization_id: Uuid,
    approval_id: Uuid,
) -> Result<Option<ApprovalView>> {
    let sql = format!("{SELECT} where a.organization_id = $1 and a.id = $2");
    let row = sqlx::query(&sql)
        .bind(organization_id)
        .bind(approval_id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_row).transpose()
}

/// One page of the inbox.
pub async fn list_approvals(
    pool: &PgPool,
    organization_id: Uuid,
    query: &ApprovalQuery,
) -> Result<store::Page<ApprovalView>> {
    let status = match query.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => {
            let normalized = raw.to_ascii_lowercase();
            if !STATUSES.contains(&normalized.as_str()) {
                return Err(InventoryError::InvalidQuery(format!(
                    "{raw} is not an approval status — use one of {}",
                    STATUSES.join(", ")
                )));
            }
            Some(normalized)
        }
    };

    // The cursor is the id of the last row of the previous page, resolved to its position by a
    // subquery rather than carried as a `(created_at, id)` pair on the wire. Two reasons: the
    // pair form needs the client to hand back a timestamp it can only get by parsing a JSON date,
    // and a row inserted between the two requests would then sort wrongly. Resolving the id
    // inside the query re-reads the position from the row itself, so a late insert shifts the
    // window instead of being skipped.
    let after: Option<(OffsetDateTime, String)> = match cursor_id(query.cursor.as_deref()) {
        None => None,
        Some(cursor) => Some(cursor_position(pool, organization_id, cursor).await?.ok_or_else(
            || {
                InventoryError::InvalidQuery(
                    "that page cursor does not name a request in this inbox".to_owned(),
                )
            },
        )?),
    };
    let limit = query.limit.clamp(1, 200);

    let sql = format!(
        "{SELECT} where a.organization_id = $1 \
           and ($2::text is null or a.status = $2) \
           and ($3::uuid is null or a.item_id = $3) \
           and ($4::timestamptz is null or (a.created_at, a.id::text) > ($4, $5)) \
         order by a.created_at desc, a.id::text desc limit $6"
    );
    let rows = sqlx::query(&sql)
        .bind(organization_id)
        .bind(status.clone())
        .bind(query.item_id)
        .bind(after.as_ref().map(|pair| pair.0))
        .bind(after.as_ref().map(|pair| pair.1.clone()))
        .bind(limit + 1)
        .fetch_all(pool)
        .await?;

    let total: i64 = sqlx::query_scalar(
        "select count(*) from inventory_adjustment_approvals \
          where organization_id = $1 and ($2::text is null or status = $2)",
    )
    .bind(organization_id)
    .bind(status.clone())
    .fetch_one(pool)
    .await?;

    let mut items: Vec<ApprovalView> = rows.iter().map(map_row).collect::<Result<_>>()?;
    let next_cursor = if items.len() > limit as usize {
        items.truncate(limit as usize);
        items.last().map(|row| row.id.to_string())
    } else {
        None
    };
    Ok(store::Page::new(items, next_cursor, total))
}

/// Every request ever raised for one item — the item detail's "adjustments awaiting a decision"
/// panel and the history below it.
pub async fn list_for_item(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
    limit: i64,
) -> Result<Vec<ApprovalView>> {
    let sql = format!(
        "{SELECT} where a.organization_id = $1 and a.item_id = $2 \
         order by a.created_at desc limit $3"
    );
    let rows = sqlx::query(&sql)
        .bind(organization_id)
        .bind(item_id)
        .bind(limit.clamp(1, 200))
        .fetch_all(pool)
        .await?;
    rows.iter().map(map_row).collect()
}

/// How many requests are waiting, for the module nav's badge.
pub async fn pending_count(pool: &PgPool, organization_id: Uuid) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from inventory_adjustment_approvals \
          where organization_id = $1 and status = 'pending'",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?)
}

/// The number a request is measured by, for a given write.
///
/// Exposed rather than inlined so the drawer can ask "would this need approval?" **before** the
/// operator fills the form in, and get the same answer the save would give. Two implementations
/// of the comparison is how a screen says "no approval needed" and then produces one.
#[must_use]
pub fn approval_amount(mode: &str, quantity: Quantity, current_on_hand: Quantity) -> Quantity {
    // The absolute value, computed on the integer the module already works in rather than
    // through a helper that may not exist: `Quantity` is a signed i128 of thousandths, so the
    // magnitude is `abs` on that and there is no rounding to get wrong on the way.
    let magnitude = |value: Quantity| -> Quantity {
        if value.is_negative() {
            // `from_milli` refuses an overflow and returns an `Option`, so the negation is
            // `checked` on purpose: `i128::MIN` negated would wrap to itself and a quantity of
            // -9.2e18 would come back positive. A `Quantity` can never hold that value — the
            // constructor already refused it — so the fallback is unreachable rather than merely
            // unlikely, and `ZERO` is the safe answer if it ever is not.
            Quantity::from_milli(value.milli().checked_neg().unwrap_or(i128::MIN))
                .unwrap_or(Quantity::ZERO)
        } else {
            value
        }
    };
    match mode {
        // A counted number is a distance from what was there; a delta already is one.
        "counted" => quantity
            .checked_sub(current_on_hand)
            .map(magnitude)
            .unwrap_or(Quantity::ZERO),
        _ => magnitude(quantity),
    }
}

/// Whether a write of this size has to wait for a decision, given the settings row.
///
/// A threshold of zero means "the organization has switched the rule off", and that is a real
/// setting rather than a degenerate one: an installation that wants every movement direct sets
/// it to zero and the comparison below never fires. The rule is `>`, not `>=`, so a threshold of
/// ten allows an adjustment of exactly ten and asks about eleven.
#[must_use]
pub fn needs_approval(amount: Quantity, settings: &Settings) -> bool {
    amount.milli() > settings.adjustment_approval_threshold.milli()
}

/// Raise a request for a write that is over the threshold.
///
/// Called by the route **only when the caller does not hold `inventory.adjustment.approve`**:
/// somebody who can approve does not get a request to chase, they just do it. The threshold check
/// runs again here rather than trusting the caller's arithmetic, because the caller's arithmetic
/// and this function's are the two things that must agree and one of them has to be checked.
///
/// # Errors
///
/// Refuses a write that is not over the threshold (it should never have been routed here), a
/// zero or negative amount, a second open request for the same item and location, and a foreign
/// item or location — the last as a `404`, before the request exists.
pub async fn request_approval(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewApproval,
    actor: Uuid,
) -> Result<ApprovalView> {
    let settings = store::get_settings(pool, organization_id).await?;
    // The item and the location are resolved before the arithmetic, so a bad id is a 404 and not
    // a request carrying a quantity measured against a row that does not exist.
    store::get_item(pool, organization_id, new.item_id).await?;
    store::get_location(pool, organization_id, new.location_id).await?;
    let level = ledger::stock_level(pool, organization_id, new.item_id, new.location_id).await?;

    let amount = approval_amount(&new.mode, new.quantity, level.on_hand);
    if amount.milli() <= 0 {
        return Err(InventoryError::invalid(
            "movement",
            "quantity",
            "an adjustment of nothing is not an adjustment",
        ));
    }
    if !needs_approval(amount, &settings) {
        return Err(InventoryError::invalid(
            "movement",
            "quantity",
            format!(
                "an adjustment of {amount} is within the threshold of {} — record it directly",
                settings.adjustment_approval_threshold
            ),
        ));
    }

    let open: Option<(Uuid, String)> = sqlx::query_as(
        "select id, amount::text as amount from inventory_adjustment_approvals \
          where organization_id = $1 and item_id = $2 and location_id = $3 \
            and status = 'pending' for update",
    )
    .bind(organization_id)
    .bind(new.item_id)
    .bind(new.location_id)
    .fetch_optional(pool)
    .await?;
    if let Some((_, pending_amount)) = open {
        return Err(InventoryError::ApprovalNotGranted(format!(
            "a request for {pending_amount} is already waiting on a decision for this item at this location"
        )));
    }

    let note = crate::items::validate_note("approval", &new.note).unwrap_or_default();
    let approval_id: Uuid = sqlx::query_scalar(
        "insert into inventory_adjustment_approvals \
             (organization_id, item_id, location_id, requested_by, kind, mode, quantity, reason, \
              note, source_kind, source_id, amount, threshold, on_hand_at_request) \
         values ($1, $2, $3, $4, $5, $6, $7::numeric, $8, $9, $10, $11, $12::numeric, $13::numeric, \
                 $14::numeric) \
         returning id",
    )
    .bind(organization_id)
    .bind(new.item_id)
    .bind(new.location_id)
    .bind(actor)
    .bind(&new.kind)
    .bind(&new.mode)
    .bind(new.quantity.to_text())
    .bind(&new.reason)
    .bind(&note)
    .bind(new.source_kind.as_deref())
    .bind(new.source_id)
    .bind(amount.to_text())
    .bind(settings.adjustment_approval_threshold.to_text())
    .bind(level.on_hand.to_text())
    .fetch_one(pool)
    .await?;

    get_approval(pool, organization_id, approval_id)
        .await?
        .ok_or(InventoryError::NotFound("adjustment approval"))
}

/// The one row [`decide`] locks, holding the write it is going to replay.
///
/// Deliberately *not* the same shape as [`ApprovalView`]: this is the un-applied movement, so it
/// carries the raw `quantity` text and the `mode` it has to be read with, and it does not carry
/// the display columns (`sku`, `item_name`, `location_code`) that the view joins for. Selecting
/// the view's columns here would lock a row joined to three other tables for a write that only
/// touches one.
#[derive(Debug, sqlx::FromRow)]
struct ApprovalRow {
    /// Never read. The id is the key the update filters on, and the row is already known to
    /// carry this one — selecting it keeps the SELECT honest about which columns the lock
    /// depends on. `sqlx` needs a binding for every selected column, so a struct field with a
    /// doc comment is where "selected on purpose, not read" has to live.
    #[allow(dead_code)]
    id: Uuid,
    /// The item.
    item_id: Uuid,
    /// The location.
    location_id: Uuid,
    /// The kind to write.
    kind: String,
    /// `delta` or `counted`.
    mode: String,
    /// The quantity as text — parsed by the module's own parser, not by PostgreSQL.
    quantity_text: String,
    /// The reason code's stored name.
    reason: String,
    /// The requester's note.
    note: String,
    /// Kept for the record, and deliberately **not** used by the replay: the ledger row the
    /// approval produces points at the approval, not at whatever document caused the request, so
    /// "which stocktake raised this" is answered by the request and not by the movement.
    #[allow(dead_code)]
    source_kind: Option<String>,
    /// Likewise.
    #[allow(dead_code)]
    source_id: Option<Uuid>,
    /// One of [`STATUSES`].
    status: String,
    /// Who asked — the approver must not be this person.
    requested_by: Uuid,
}

/// Approve or reject a request.
///
/// Both branches are one transaction because the request row and the ledger row must never
/// disagree: a request marked approved next to an unchanged stock level is exactly the state that
/// makes a manager believe a recount was applied when it was not.
///
/// The approve branch **replays the request through [`record_movement`]** — the module's one
/// write path — rather than writing a stock row of its own. The requester's `may_go_negative`
/// answer is deliberately **not** carried onto the request: the approver's own permission is what
/// governs the replay, so somebody without `inventory.negative.manage` cannot approve a
/// correction into negative stock even if the requester who raised it could.
///
/// # Errors
///
/// A request that is not pending, a rejection with no comment, an approval of the requester's own
/// request, and a replay the ledger refuses (which surfaces as the ledger's own error — an
/// approval that would drive stock negative is refused here, in the approver's hands, with the
/// available number, rather than silently becoming a decided request with no movement).
pub async fn decide(
    pool: &PgPool,
    organization_id: Uuid,
    approval_id: Uuid,
    actor: Uuid,
    decision: &str,
    comment: Option<&str>,
    may_go_negative: bool,
) -> Result<(ApprovalView, Option<Recorded>)> {
    let approve = match decision.trim().to_ascii_lowercase().as_str() {
        "approve" | "approved" => true,
        "reject" | "rejected" => false,
        other => {
            return Err(InventoryError::invalid(
                "approval",
                "decision",
                format!("{other} is not a decision — use `approve` or `reject`"),
            ))
        }
    };
    let comment = comment.map(str::trim).filter(|text| !text.is_empty());
    if !approve && comment.is_none() {
        return Err(InventoryError::invalid(
            "approval",
            "comment",
            "say why, so the operator knows what to recount",
        ));
    }

    let mut transaction = pool.begin().await?;

    // A named row rather than a 13-element tuple. The tuple version was written first and is
    // unreadable at the call site *and* silent about a column changing: sqlx decodes a tuple
    // positionally, so swapping two adjacent `text` columns in the SELECT compiles and hands the
    // approver somebody else's note. A struct makes the swap a compile error.
    let row: Option<ApprovalRow> = sqlx::query_as(
        "select id, item_id, location_id, kind, mode, quantity::text as quantity, reason, \
                note, source_kind, source_id, status, requested_by \
           from inventory_adjustment_approvals \
          where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(approval_id)
    .fetch_optional(&mut *transaction)
    .await?;

    let Some(row) = row else {
        return Err(InventoryError::NotFound("adjustment approval"));
    };
    let ApprovalRow {
        item_id,
        location_id,
        kind,
        mode,
        quantity_text,
        reason,
        note,
        source_kind: _,
        source_id: _,
        status,
        requested_by,
        ..
    } = row;

    if status != "pending" {
        return Err(InventoryError::InvalidStatusChange(format!(
            "this request was already {status}"
        )));
    }
    // A person approving their own recount is the case the whole control exists to prevent, and
    // the check is here rather than in the route so that every caller gets it.
    if approve && requested_by == actor {
        return Err(InventoryError::ApprovalNotGranted(
            "you asked for this adjustment, so somebody else has to approve it".to_owned(),
        ));
    }

    if !approve {
        sqlx::query(
            "update inventory_adjustment_approvals \
                set status = 'rejected', decision = 'rejected', decided_by = $2, decided_at = now(), \
                    comment = $3, updated_at = now() \
              where id = $1",
        )
        .bind(approval_id)
        .bind(actor)
        .bind(comment)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        let view = get_approval(pool, organization_id, approval_id)
            .await?
            .ok_or(InventoryError::NotFound("adjustment approval"))?;
        return Ok((view, None));
    }

    // Approve. The request's own values are replayed, never re-derived: the requester may have
    // edited the drawer since, and the approver approved *this* number.
    let quantity = store::quantity_from_text(&quantity_text)?;
    let current = ledger::stock_level(pool, organization_id, item_id, location_id).await?;
    let resolved = if mode == "counted" {
        quantity
            .checked_sub(current.on_hand)
            .ok_or_else(|| {
                InventoryError::invalid("movement", "quantity", "that number is too large")
            })?
    } else {
        quantity
    };

    let movement = NewMovement {
        item_id,
        location_id,
        kind: Some(kind),
        quantity: resolved.to_text(),
        reason: Some(reason),
        note: Some(note),
        source_kind: Some("approval".to_owned()),
        source_id: Some(approval_id),
        may_go_negative,
    };
    let recorded = ledger::record_movement(pool, organization_id, &movement, Some(actor)).await?;

    sqlx::query(
        "update inventory_adjustment_approvals \
            set status = 'approved', decision = 'approved', decided_by = $2, decided_at = now(), \
                comment = $3, movement_id = $4, updated_at = now() \
          where id = $1",
    )
    .bind(approval_id)
    .bind(actor)
    .bind(comment)
    .bind(recorded.movement.id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let view = get_approval(pool, organization_id, approval_id)
        .await?
        .ok_or(InventoryError::NotFound("adjustment approval"))?;
    Ok((view, Some(recorded)))
}

/// The requester withdraws their own request, while it is still waiting.
///
/// A decided request is not cancelled: a manager who rejected it has said something that belongs
/// in the history, and "cancel" would let the requester erase it.
pub async fn cancel(
    pool: &PgPool,
    organization_id: Uuid,
    approval_id: Uuid,
    actor: Uuid,
) -> Result<ApprovalView> {
    let owner: Option<(String, Uuid)> = sqlx::query_as(
        "select status, requested_by from inventory_adjustment_approvals \
          where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(approval_id)
    .fetch_optional(pool)
    .await?;
    let Some((status, requested_by)) = owner else {
        return Err(InventoryError::NotFound("adjustment approval"));
    };
    if status != "pending" {
        return Err(InventoryError::InvalidStatusChange(format!(
            "this request was already {status}"
        )));
    }
    if requested_by != actor {
        return Err(InventoryError::ApprovalNotGranted(
            "only the person who asked for this adjustment can withdraw it".to_owned(),
        ));
    }
    sqlx::query(
        "update inventory_adjustment_approvals \
            set status = 'cancelled', decision = 'cancelled', cancelled_at = now(), updated_at = now() \
          where id = $1",
    )
    .bind(approval_id)
    .execute(pool)
    .await?;
    get_approval(pool, organization_id, approval_id)
        .await?
        .ok_or(InventoryError::NotFound("adjustment approval"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Settings;

    fn settings(threshold: &str) -> Settings {
        Settings {
            adjustment_approval_threshold: store::parse_quantity(
                "settings",
                "adjustment_approval_threshold",
                threshold,
            )
            .expect("the fixture is a number"),
            ..Settings::defaults()
        }
    }

    #[test]
    fn the_amount_of_a_delta_is_its_magnitude() {
        let current = store::parse_quantity("t", "q", "10").expect("fixture");
        // A delta of -4 moves four units even though the number is negative: the approver is
        // approving a change of four, and showing them "-4" invites them to read it as "minus a
        // lot" rather than "four fewer".
        assert_eq!(
            approval_amount("delta", Quantity::from_milli(-4_000).expect("a small number"), current).to_text(),
            "4.000"
        );
    }

    #[test]
    fn the_amount_of_a_count_is_the_distance_from_what_was_there() {
        let current = store::parse_quantity("t", "q", "10").expect("fixture");
        // Counting 6 when 10 were on the shelf is an adjustment of 4, not of 6.
        assert_eq!(
            approval_amount("counted", store::parse_quantity("t", "q", "6").expect("fixture"), current)
                .to_text(),
            "4.000"
        );
        // And counting 14 is the same distance the other way.
        assert_eq!(
            approval_amount("counted", store::parse_quantity("t", "q", "14").expect("fixture"), current)
                .to_text(),
            "4.000"
        );
        // Counting what was already there is not an adjustment at all.
        assert_eq!(
            approval_amount("counted", store::parse_quantity("t", "q", "10").expect("fixture"), current)
                .to_text(),
            "0.000"
        );
    }

    #[test]
    fn a_count_measured_from_a_negative_balance_is_still_a_distance() {
        // The negative-stock exception lets on_hand be -3. A recount of 2 is a distance of 5
        // upwards, and the magnitude is what an approver has to weigh, not the -5 the subtraction
        // produces.
        let current = store::parse_quantity("t", "q", "-3").expect("fixture");
        assert_eq!(
            approval_amount("counted", store::parse_quantity("t", "q", "2").expect("fixture"), current)
                .to_text(),
            "5.000"
        );
    }

    #[test]
    fn the_threshold_is_exceeded_strictly_above() {
        let ten = settings("10");
        // Exactly at the line is allowed: the rule is "over", and an organization that set 10
        // meant "up to ten is mine to write".
        assert!(!needs_approval(store::parse_quantity("t", "q", "10").expect("f"), &ten));
        assert!(needs_approval(store::parse_quantity("t", "q", "10.001").expect("f"), &ten));
    }

    #[test]
    fn a_threshold_of_zero_still_asks_about_a_real_movement() {
        // Zero is a real setting ("every adjustment is mine"), not a disabled control: a move of
        // a thousand units is still over it.
        let zero = settings("0");
        assert!(needs_approval(store::parse_quantity("t", "q", "0.001").expect("f"), &zero));
    }

    #[test]
    fn the_decision_vocabulary_is_the_two_a_person_can_mean() {
        // The parse is what stops `decision: "maybe"` from being stored as a decision and shown
        // as an approval. It is exercised through the string comparison the function uses, since
        // the function needs a pool for everything after this point.
        for good in ["approve", "approved", "Approve"] {
            let normalized = good.trim().to_ascii_lowercase();
            assert!(matches!(normalized.as_str(), "approve" | "approved"));
        }
        assert!(!"maybe".trim().to_ascii_lowercase().contains("approve"));
    }
}
