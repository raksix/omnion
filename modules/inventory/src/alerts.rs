//! Low-stock alerts: one per crossing, cleared by a restock, re-armed by the next crossing.
//!
//! Slice 1 shipped the **edge**: `inventory.stock.low` is emitted on the downward crossing only,
//! so a busy warehouse does not flood the automation log. What it did not ship is the *record*.
//! An event is not an alert — it says something happened, and nothing about whether anybody acted
//! on it, whether the shelf was refilled, or whether the item went back under the threshold an
//! hour later. This file is that record.
//!
//! ## Why the table is not derived from the event log
//!
//! The cheap design is "the alert inbox is a filtered view of `inventory.stock.low` events". It
//! fails in the two ways that matter to a person looking at it:
//!
//! * **It cannot be cleared.** A shelf that was refilled still shows a crossing three days ago,
//!   and there is no way to say "this is handled" except by annotating the event.
//! * **It cannot tell a crossing from a restock.** The whole question the inbox answers — "is
//!   this still true?" — needs the *state*, not the history. History is what the item detail
//!   shows; the inbox is what someone works through.
//!
//! ## The partial unique index is the rule
//!
//! "Exactly one alert per crossing" is a **database** fact, not a promise in a handler: the
//! unique index is partial on `cleared_at is null`, so a closed episode does not block the next
//! one. A full `(item, location, kind)` unique index would make the second crossing *fail*, which
//! is a different bug with the same words; a service-level `select` before `insert` would be a
//! race, and the race is won by the wrong writer exactly when two movements land together.
//!
//! A restock **clears**; it does not revive. The next crossing raises a **new** row, so the
//! history reads as a series of episodes rather than one row whose meaning depends on two
//! timestamps — the same reason the adjustment request holds the whole write instead of being a
//! `pending` flag on a movement.
//!
//! The threshold is stored **on the row**. An alert is a statement about one moment; a row that
//! re-read the item's threshold today would make yesterday's alert claim something that never
//! happened.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{InventoryError, Result};
use crate::model::StockStatus;
use crate::money::Quantity;

/// What an alert is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// The balance fell to or below the item's reorder point.
    LowStock,
    /// The balance is below zero — a permitted `correction` left it there.
    NegativeStock,
}

impl AlertKind {
    /// The value stored in `inventory_alerts.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LowStock => "low_stock",
            Self::NegativeStock => "negative_stock",
        }
    }

    /// Read a stored kind.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "low_stock" => Self::LowStock,
            "negative_stock" => Self::NegativeStock,
            _ => return None,
        })
    }

    /// The kind a [`StockStatus`] raises, or `None` for a healthy row.
    ///
    /// **The mapping lives here and not in the sweep**, so a status added to the model later
    /// cannot quietly stop raising anything: this is the one function that has to be updated,
    /// and the sweep asks it rather than repeating the match.
    #[must_use]
    pub const fn for_status(status: StockStatus) -> Option<Self> {
        match status {
            StockStatus::Negative => Some(Self::NegativeStock),
            StockStatus::Critical | StockStatus::Low => Some(Self::LowStock),
            StockStatus::Ok => None,
        }
    }
}

/// An alert, as the inbox renders it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The item.
    pub item_id: Uuid,
    /// The item's SKU, so a row reads like the shelf label.
    pub sku: String,
    /// The item's name.
    pub item_name: String,
    /// The location it is about, when the alert is per location.
    pub location_id: Option<Uuid>,
    /// That location's code.
    pub location_code: Option<String>,
    /// What kind of crossing it is.
    pub kind: AlertKind,
    /// The threshold that was crossed, **as it stood when it was crossed**.
    pub threshold: Quantity,
    /// The balance at the moment of the crossing.
    pub observed: Quantity,
    /// When.
    #[serde(with = "crate::dates::instant")]
    pub raised_at: OffsetDateTime,
    /// When the notification went out, if it did.
    #[serde(default, with = "crate::dates::instant::option")]
    pub notified_at: Option<OffsetDateTime>,
    /// When the balance came back, if it did.
    #[serde(default, with = "crate::dates::instant::option")]
    pub cleared_at: Option<OffsetDateTime>,
}

impl AlertView {
    /// True while the alert is still unanswered.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.cleared_at.is_none()
    }
}

/// What the sweep did — the number the screen's toast prints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swept {
    /// Rows examined.
    pub examined: i64,
    /// Alerts raised by this run.
    pub raised: i64,
    /// Alerts closed by this run.
    pub cleared: i64,
    /// Alerts still open after the run.
    pub open: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct AlertRow {
    id: Uuid,
    organization_id: Uuid,
    item_id: Uuid,
    sku: String,
    item_name: String,
    location_id: Option<Uuid>,
    location_code: Option<String>,
    kind: String,
    threshold: String,
    observed: String,
    raised_at: OffsetDateTime,
    notified_at: Option<OffsetDateTime>,
    cleared_at: Option<OffsetDateTime>,
}

impl AlertRow {
    fn into_view(self) -> Result<AlertView> {
        let kind = AlertKind::parse(&self.kind).ok_or_else(|| {
            InventoryError::invalid(
                "alert",
                "kind",
                format!("stored alert kind {} is not one this module knows", self.kind),
            )
        })?;
        Ok(AlertView {
            id: self.id,
            organization_id: self.organization_id,
            item_id: self.item_id,
            sku: self.sku,
            item_name: self.item_name,
            location_id: self.location_id,
            location_code: self.location_code,
            kind,
            threshold: crate::store::quantity_from_text(&self.threshold)?,
            observed: crate::store::quantity_from_text(&self.observed)?,
            raised_at: self.raised_at,
            notified_at: self.notified_at,
            cleared_at: self.cleared_at,
        })
    }
}

/// One stock row the sweep has to decide about.
///
/// A **named struct, not a positional tuple**: this query joins two tables and a five-element
/// `Option<(Uuid, Uuid, String, String, bool)>` would decode by position, and the day two
/// adjacent text columns swap in the SELECT the sweep would compare a threshold against an
/// on-hand — silently raising no alerts and clearing every open one.
#[derive(Debug, sqlx::FromRow)]
struct CandidateRow {
    item_id: Uuid,
    location_id: Uuid,
    /// `on_hand − reserved` — the number the badge and the alert both ask about.
    available: String,
    min_threshold: String,
    reorder_point: String,
    item_active: bool,
}

/// The alert inbox's query.
#[derive(Debug, Clone, Default)]
pub struct AlertQuery {
    /// Free text over the SKU, the name and the location code.
    pub search: Option<String>,
    /// One kind — repeated for several.
    pub kinds: Vec<String>,
    /// Only the unanswered ones.
    pub open_only: bool,
    /// Page size.
    pub limit: Option<i64>,
    /// Cursor — the id of the last row of the previous page.
    pub cursor: Option<Uuid>,
}

impl AlertQuery {
    /// How many rows a page holds, or the default.
    pub fn page_size(&self) -> i64 {
        match self.limit {
            None => crate::store::DEFAULT_PER_PAGE,
            Some(limit) if limit < 1 => crate::store::DEFAULT_PER_PAGE,
            Some(limit) if limit > crate::store::MAX_PER_PAGE => crate::store::MAX_PER_PAGE,
            Some(limit) => limit,
        }
    }

    /// The id the cursor points at, if there is one.
    pub fn cursor_id(&self) -> Option<Uuid> {
        self.cursor.filter(|id| !id.is_nil())
    }

    /// The kinds the caller asked for, parsed — an unknown kind is refused, not ignored.
    pub fn parsed_kinds(&self) -> Result<Vec<AlertKind>> {
        self.kinds
            .iter()
            .filter(|raw| !raw.trim().is_empty())
            .map(|raw| {
                AlertKind::parse(raw.trim()).ok_or_else(|| {
                    InventoryError::invalid("alert", "kind", format!("{raw} is not an alert kind"))
                })
            })
            .collect()
    }
}

/// How many alerts are still open, for the badge.
pub async fn open_count(pool: &PgPool, organization_id: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from inventory_alerts where organization_id = $1 and cleared_at is null",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// The alert inbox.
pub async fn list_alerts(
    pool: &PgPool,
    organization_id: Uuid,
    query: &AlertQuery,
) -> Result<crate::store::Page<AlertView>> {
    let kinds = query.parsed_kinds()?;
    let needle = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(|raw| format!("%{}%", raw.to_lowercase()));

    let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "select a.id, a.organization_id, a.item_id, i.sku, i.name as item_name, a.location_id, \
                l.code as location_code, a.kind, a.threshold::text as threshold, \
                a.observed::text as observed, a.raised_at, a.notified_at, a.cleared_at \
         from inventory_alerts a \
         join inventory_items i on i.id = a.item_id \
         left join inventory_locations l on l.id = a.location_id \
         where a.organization_id = ",
    );
    builder.push_bind(organization_id);
    if let Some(needle) = needle {
        builder
            .push(" and (lower(i.sku) like ")
            .push_bind(needle.clone())
            .push(" or lower(i.name) like ")
            .push_bind(needle.clone())
            .push(" or lower(coalesce(l.code, '')) like ")
            .push_bind(needle)
            .push(")");
    }
    if !kinds.is_empty() {
        builder.push(" and a.kind = any(").push_bind(
            kinds.iter().map(|kind| kind.as_str().to_string()).collect::<Vec<_>>(),
        );
        builder.push(")");
    } else if query.open_only {
        builder.push(" and a.cleared_at is null");
    }
    builder.push(" order by a.raised_at desc, a.id desc limit ").push_bind(query.page_size() + 1);

    let rows: Vec<AlertRow> = builder.build_query_as().fetch_all(pool).await?;
    let has_more = rows.len() as i64 > query.page_size();
    let items: Result<Vec<AlertView>> = rows
        .into_iter()
        .take(query.page_size() as usize)
        .map(AlertRow::into_view)
        .collect();
    let items = items?;
    let next_cursor = has_more
        .then(|| items.last().map(|alert| alert.id.to_string()))
        .flatten();

    Ok(crate::store::Page { items, next_cursor, total_estimate: 0 })
}

/// Raise or clear the alerts the current balances call for.
///
/// This is the **sweep**, and it is idempotent: running it twice over an unchanged shelf raises
/// nothing the second time. That property is not a nicety — the setting `alerts_on_read` turns
/// it on whenever a stock screen is read, so a busy warehouse would otherwise raise one alert per
/// page view and the inbox would be worthless by lunchtime.
///
/// The per-alert rule is **one open alert per item × location × kind**, and it is enforced by the
/// partial unique index rather than by a select-then-insert, because two movements landing at the
/// same moment is not a rare event in a warehouse and the select-then-insert version loses that
/// race. `on conflict do nothing` is the losing writer's answer, and it is a correct one: the
/// alert exists, which is all the sweep was trying to achieve.
pub async fn sweep(pool: &PgPool, organization_id: Uuid) -> Result<Swept> {
    // The candidate rows are the **current balances**, joined to their item's threshold, and the
    // status is computed by the model's own `StockStatus::of` equivalent in SQL. A sweep that
    // compared `on_hand < min_threshold` in its own expression would be a second definition of
    // "low", and the badge on the stock list would disagree with the inbox the first time the
    // two were written by different people.
    // **Availability, not `on_hand`, and both thresholds, not one folded number.** The first is
    // because `StockStatus::of` is defined over `available` and the stock list's badge already
    // says so; a sweep that asked about `on_hand` would raise an alert for a shelf whose every
    // unit is already promised. The second is because the two limits are two different
    // sentences — "you are out" and "time to reorder" — and `greatest(a, b)` erases the
    // difference, which is the one thing this inbox exists to say.
    let rows: Vec<CandidateRow> = sqlx::query_as(
        "select s.item_id, s.location_id, \
                (s.on_hand - s.reserved)::text as available, \
                i.min_threshold::text as min_threshold, \
                i.reorder_point::text as reorder_point, \
                i.active as item_active \
         from inventory_stock s \
         join inventory_items i on i.id = s.item_id \
         where s.organization_id = $1",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut swept = Swept { examined: rows.len() as i64, ..Swept::default() };

    for candidate in rows {
        let CandidateRow {
            item_id,
            location_id,
            available,
            min_threshold,
            reorder_point,
            item_active,
            ..
        } = candidate;
        let available = crate::store::quantity_from_text(&available)?;
        let min_threshold = crate::store::quantity_from_text(&min_threshold)?;
        let reorder_point = crate::store::quantity_from_text(&reorder_point)?;
        // **The model's own function, not a copy of it.** A sweep that decided "low" with its own
        // comparison would be a second definition, and the first time the two were written by
        // different people the inbox and the stock badge would disagree about the same shelf.
        let status = StockStatus::of(available, min_threshold, reorder_point);

        match (AlertKind::for_status(status), item_active) {
            (Some(kind), true) => {
                let raised = sqlx::query(
                    "insert into inventory_alerts (id, organization_id, item_id, location_id, \
                         kind, threshold, observed) \
                     values ($1, $2, $3, $4, $5, $6::numeric, $7::numeric) \
                     on conflict do nothing",
                )
                .bind(Uuid::new_v4())
                .bind(organization_id)
                .bind(item_id)
                .bind(location_id)
                .bind(kind.as_str())
                // The threshold on the row is the **reorder point**, because that is the line an
                // alert says was crossed, and the observed number is what the shelf actually
                // offered. Recording the other limit would make the row describe a different
                // moment than the one that raised it.
                .bind(reorder_point.to_text())
                .bind(available.to_text())
                .execute(pool)
                .await?;
                if raised.rows_affected() > 0 {
                    swept.raised += 1;
                }
            }
            _ => {
                // The row is healthy (or the item is retired): close whatever is open. The
                // `and cleared_at is null` is what makes this a no-op on a second run, which is
                // what lets the sweep be called on every read.
                let cleared = sqlx::query(
                    "update inventory_alerts set cleared_at = now() \
                     where organization_id = $1 and item_id = $2 and location_id = $3 \
                       and cleared_at is null",
                )
                .bind(organization_id)
                .bind(item_id)
                .bind(location_id)
                .execute(pool)
                .await?;
                if cleared.rows_affected() > 0 {
                    swept.cleared += cleared.rows_affected() as i64;
                }
            }
        }
    }

    swept.open = open_count(pool, organization_id).await?;
    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_row_raises_nothing() {
        assert_eq!(AlertKind::for_status(StockStatus::Ok), None);
    }

    #[test]
    fn the_two_bad_statuses_map_to_the_two_alert_kinds() {
        // The mapping is one function so a new `StockStatus` cannot quietly stop raising
        // anything: this is the place that has to be updated, and the sweep asks it.
        assert_eq!(
            AlertKind::for_status(StockStatus::Negative),
            Some(AlertKind::NegativeStock)
        );
        assert_eq!(
            AlertKind::for_status(StockStatus::Critical),
            Some(AlertKind::LowStock)
        );
        assert_eq!(
            AlertKind::for_status(StockStatus::Low),
            Some(AlertKind::LowStock)
        );
    }

    #[test]
    fn a_negative_balance_is_an_alert_of_its_own_and_not_merely_low() {
        // They are different conversations: "you are running out" and "your books say you owe
        // yourself stock". Folding the second into the first hides the one that needs a
        // correction rather than a delivery.
        let negative = AlertKind::for_status(StockStatus::Negative).expect("an alert");
        let low = AlertKind::for_status(StockStatus::Low).expect("an alert");
        assert_ne!(negative, low);
        assert_eq!(negative.as_str(), "negative_stock");
    }

    #[test]
    fn the_alert_reads_the_badge_the_stock_list_already_draws() {
        // This test exists because it **failed** when the sweep had its own comparison. The
        // first version folded the two thresholds with `greatest` and used a strict `<`, so a
        // shelf sitting exactly on its reorder point was `Ok` in the inbox and `Low` on the
        // stock list — the two screens disagreeing about the same shelf, which is the one thing
        // the shared `StockStatus::of` is there to prevent.
        let q = |n: &str| Quantity::parse(n).expect("quantity");
        let status = |available: &str, min: &str, reorder: &str| {
            StockStatus::of(q(available), q(min), q(reorder))
        };

        // At or below the reorder point is `Low`, and the comparison is **inclusive** — the
        // model's rule, which the sweep now calls rather than repeats.
        assert_eq!(status("5.000", "0.000", "5.000"), StockStatus::Low);
        assert_eq!(status("4.999", "0.000", "5.000"), StockStatus::Low);
        assert_eq!(status("5.001", "0.000", "5.000"), StockStatus::Ok);
        // At or below the minimum is `Critical` — the sharper of the two, and folding them
        // together with `greatest` would have hidden it.
        assert_eq!(status("2.000", "2.000", "5.000"), StockStatus::Critical);
        // Below zero is the same whatever the limits say.
        assert_eq!(status("-0.001", "0.000", "0.000"), StockStatus::Negative);
        assert_eq!(status("-3.000", "2.000", "5.000"), StockStatus::Negative);
    }

    #[test]
    fn a_kind_a_caller_made_up_is_refused_rather_than_ignored() {
        // The same rule as the transfer filter: a filter that quietly drops what it does not
        // understand answers "nothing is wrong", which is the one conclusion a typo must not be
        // able to produce.
        let query = AlertQuery { kinds: vec!["low_stock".into(), "on_fire".into()], ..AlertQuery::default() };
        let error = query.parsed_kinds().expect_err("a typo must not pass");
        assert!(error.to_string().contains("on_fire"), "{error}");
    }

    #[test]
    fn every_stored_kind_round_trips() {
        for kind in [AlertKind::LowStock, AlertKind::NegativeStock] {
            assert_eq!(AlertKind::parse(kind.as_str()), Some(kind), "{kind:?}");
        }
    }
}
