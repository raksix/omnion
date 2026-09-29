//! The catalog of things: items, warehouses, locations and the stock rollup itself.
//!
//! Four rules the HTTP layer must not have to remember, because a screen, a CSV import and a
//! scanner box are all going to call these:
//!
//! * **Every read and write is organization-scoped in SQL, and a row of another organization is
//!   `404`.** Not `403`: a `403` confirms the row exists, and one organization's stock is the
//!   one thing this module exists to keep apart.
//! * **Quantities cross the boundary as text and are bound back with `::numeric`.** `numeric(14,3)`
//!   has no Rust type in this workspace, so a value is read as `quantity::text`, validated by
//!   [`crate::money::Quantity`], and written as a normalised string with an explicit cast.
//! * **Nothing is deleted.** `archived_*` sets `archived_at`, because a past movement still names
//!   the item it moved.
//! * **The rollup is never written directly by a screen.** A caller adjusts stock through the
//!   ledger ([`crate::ledger`]); the only functions here that touch `inventory_stock` are the
//!   lock/upsert that the ledger uses and [`reconciliation_report`], which reads it to compare it
//!   against the replay.

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{InventoryError, Result};
use crate::items::{self, Item};
use crate::model::{LocationKind, StockStatus};
use crate::money::{Amount, Quantity};

/// Rows a page holds when the caller names no size.
pub const DEFAULT_PER_PAGE: i64 = 50;
/// Hard cap on a page.
pub const MAX_PER_PAGE: i64 = 200;
/// Longest a search term may be.
pub const MAX_SEARCH_LENGTH: usize = 120;

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// One page of a list, with the cursor the next one starts from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    /// The rows of this page.
    pub items: Vec<T>,
    /// The cursor to pass as `?cursor=` for the next page, or `None` at the end.
    pub next_cursor: Option<String>,
    /// How many rows the filter matched, when the count is cheap enough to run.
    pub total_estimate: i64,
}

impl<T> Page<T> {
    /// A page assembled from a query.
    #[must_use]
    pub fn new(items: Vec<T>, next_cursor: Option<String>, total_estimate: i64) -> Self {
        Self {
            items,
            next_cursor,
            total_estimate,
        }
    }
}

/// An item as the screens see it: the module's [`Item`] plus the row's bookkeeping and the
/// organization's currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The module's own view of the item, so a caller gets the validated shapes.
    #[serde(flatten)]
    pub item: Item,
    /// When it was archived, if it was.
    #[serde(with = "crate::dates::instant::option")]
    pub archived_at: Option<OffsetDateTime>,
    /// When the row was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
    /// When it last changed.
    #[serde(with = "crate::dates::instant")]
    pub updated_at: OffsetDateTime,
}

impl ItemView {
    /// The compact reference an audit row and an event payload carry.
    ///
    /// Deliberately small: an event travels to every webhook subscriber, so it carries the id,
    /// the SKU and the name — not the notes, and not the cost.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "item_id": self.id,
            "sku": self.item.sku,
            "name": self.item.name,
        })
    }
}

/// A warehouse as the tree editor sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarehouseView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The short code.
    pub code: String,
    /// The name shown in the tree.
    pub name: String,
    /// Whether new stock may be booked here.
    pub active: bool,
    /// The locations under it, so the tree is one request.
    pub locations: Vec<LocationView>,
    /// How many live items the organization has — the node's "item count" in the tree.
    pub item_count: i64,
    /// The summed `on_hand` across the warehouse's locations, as a text number.
    pub on_hand_total: String,
    /// When it was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl WarehouseView {
    /// The compact reference an audit row and an event payload carry.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "warehouse_id": self.id,
            "code": self.code,
            "name": self.name,
        })
    }
}

/// A location as the screens see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The warehouse it belongs to.
    pub warehouse_id: Uuid,
    /// The short code.
    pub code: String,
    /// The name shown in the tree.
    pub name: String,
    /// What the location is for.
    pub kind: LocationKind,
    /// Whether new stock may be booked here.
    pub active: bool,
    /// The summed `on_hand` at this location, as a text number.
    pub on_hand_total: String,
    /// When it was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl LocationView {
    /// The compact reference an audit row and an event payload carry.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "location_id": self.id,
            "code": self.code,
            "name": self.name,
            "kind": self.kind.as_str(),
        })
    }
}

/// One row of the stock list: an item, a location, and the two numbers plus their difference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockLevel {
    /// The stock row's id.
    pub id: Uuid,
    /// The item.
    pub item_id: Uuid,
    /// The item's SKU, for the scanner column and the label.
    pub sku: String,
    /// The item's name.
    pub name: String,
    /// The item's category, when set.
    pub category: Option<String>,
    /// The item's unit, printed beside every quantity.
    pub unit: String,
    /// The location.
    pub location_id: Uuid,
    /// The location's code.
    pub location_code: String,
    /// The location's name.
    pub location_name: String,
    /// The warehouse the location belongs to.
    pub warehouse_id: Uuid,
    /// The warehouse's code.
    pub warehouse_code: String,
    /// How much is physically there.
    pub on_hand: Quantity,
    /// How much of it is held for orders.
    pub reserved: Quantity,
    /// `on_hand - reserved`: what a person may actually draw.
    pub available: Quantity,
    /// The item's critical threshold.
    pub min_threshold: Quantity,
    /// The item's reorder point.
    pub reorder_point: Quantity,
    /// The badge the row wears — one definition, shared with the item list and the sweep.
    pub status: StockStatus,
    /// When this row last moved, for the "idle" filter and the column.
    #[serde(with = "crate::dates::instant::option")]
    pub last_movement_at: Option<OffsetDateTime>,
}

/// An item with its stock, totals and the thresholds the badges are derived from.
///
/// The item detail reads this rather than joining in the browser: a screen that computed
/// `available` itself would be a second implementation of a rule the module already owns, and
/// would be the one that drifts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockPosition {
    /// The item.
    pub item: ItemView,
    /// One row per location that holds it.
    pub locations: Vec<StockLevel>,
    /// The sum of `on_hand` across every location.
    pub on_hand: Quantity,
    /// The sum of `reserved`.
    pub reserved: Quantity,
    /// `on_hand - reserved`, the number the item list prints.
    pub available: Quantity,
    /// The badge for the organization-wide position, not for any one row.
    pub status: StockStatus,
    /// The last movement of any kind, for the detail header.
    #[serde(with = "crate::dates::instant::option")]
    pub last_movement_at: Option<OffsetDateTime>,
}

/// What a reconciliation run found: every item × location where the rollup and the replayed
/// ledger disagree.
///
/// The report is a **list of disagreements, not a count** — a count tells somebody the module is
/// wrong and not where, and the entire purpose of [`crate::ledger::replay`] is that somebody can
/// go and look at the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StocktakeSnapshot {
    /// How many item × location rows were compared.
    pub checked: i64,
    /// How many disagreed.
    pub mismatches: Vec<StockMismatch>,
}

/// One disagreement between the rollup and the replayed ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockMismatch {
    /// The item.
    pub item_id: Uuid,
    /// The location.
    pub location_id: Uuid,
    /// The `on_hand` the rollup holds.
    pub rollup_on_hand: String,
    /// The `on_hand` the ledger replays to.
    pub replayed_on_hand: String,
    /// The `reserved` the rollup holds.
    pub rollup_reserved: String,
    /// The `reserved` the ledger replays to.
    pub replayed_reserved: String,
}

// ---------------------------------------------------------------------------------------------
// The writes the screens send
// ---------------------------------------------------------------------------------------------

/// A new item.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewItem {
    /// The organization's SKU.
    pub sku: String,
    /// The name.
    pub name: String,
    /// The category, when set.
    #[serde(default)]
    pub category: Option<String>,
    /// The unit, when set (the organization's default otherwise).
    #[serde(default)]
    pub unit: Option<String>,
    /// The barcode, when set.
    #[serde(default)]
    pub barcode: Option<String>,
    /// The critical threshold, as text.
    #[serde(default)]
    pub min_threshold: String,
    /// The reorder point, as text.
    #[serde(default)]
    pub reorder_point: String,
    /// How much a reorder brings, as text.
    #[serde(default)]
    pub reorder_qty: String,
    /// The unit cost, as text, when the organization tracks one.
    #[serde(default)]
    pub cost: Option<String>,
    /// The REQ-052 product this item mirrors.
    #[serde(default)]
    pub product_id: Option<Uuid>,
    /// Free notes.
    #[serde(default)]
    pub notes: String,
}

/// A patch of an item. `None` means "not mentioned"; an empty string means "cleared".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemPatch {
    /// The name.
    #[serde(default)]
    pub name: Option<String>,
    /// The category.
    #[serde(default)]
    pub category: Option<String>,
    /// The unit.
    #[serde(default)]
    pub unit: Option<String>,
    /// The barcode — `Some("")` clears it.
    #[serde(default)]
    pub barcode: Option<String>,
    /// The critical threshold, as text.
    #[serde(default)]
    pub min_threshold: Option<String>,
    /// The reorder point, as text.
    #[serde(default)]
    pub reorder_point: Option<String>,
    /// How much a reorder brings, as text.
    #[serde(default)]
    pub reorder_qty: Option<String>,
    /// The unit cost — `None` leaves it, `Some("")` clears it.
    #[serde(default)]
    pub cost: Option<String>,
    /// The catalog product link — `Some(null)` clears it.
    #[serde(default)]
    pub product_id: Option<Option<Uuid>>,
    /// The notes.
    #[serde(default)]
    pub notes: Option<String>,
    /// Whether new movements may name it.
    #[serde(default)]
    pub active: Option<bool>,
}

/// A new warehouse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewWarehouse {
    /// The short code.
    pub code: String,
    /// The name.
    pub name: String,
}

/// A new location under a warehouse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewLocation {
    /// The warehouse it belongs to.
    pub warehouse_id: Uuid,
    /// The short code.
    pub code: String,
    /// The name.
    pub name: String,
    /// What it is for; `internal` when the caller names none.
    #[serde(default)]
    pub kind: Option<String>,
}

/// A patch of a warehouse or a location — the rename and the deactivate the tree offers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WarehousePatch {
    /// The new name.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether the node is usable.
    #[serde(default)]
    pub active: Option<bool>,
}

/// A patch of a location, which additionally moves its kind.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocationPatch {
    /// The new name.
    #[serde(default)]
    pub name: Option<String>,
    /// What it is for.
    #[serde(default)]
    pub kind: Option<String>,
    /// Whether the node is usable.
    #[serde(default)]
    pub active: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

/// The item list's query.
#[derive(Debug, Clone, Default)]
pub struct ItemQuery {
    /// Free text over SKU, name and barcode.
    pub search: Option<String>,
    /// One category the item must carry.
    pub category: Option<String>,
    /// `true` for live only, `false` for inactive only, absent for both.
    pub active: Option<bool>,
    /// Include the archived items.
    pub include_archived: Option<bool>,
    /// `true` for items that mirror a catalog product, `false` for those that do not.
    pub linked_to_catalog: Option<bool>,
    /// Sort key.
    pub sort: Option<String>,
    /// `asc` or `desc`.
    pub direction: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Cursor of the previous page.
    pub cursor: Option<String>,
}

impl ItemQuery {
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
        let raw = self.cursor.as_deref()?;
        Uuid::parse_str(raw).ok()
    }

    /// Whether the archived rows are in scope.
    pub fn shows_archived(&self) -> bool {
        self.include_archived.unwrap_or(false)
    }

    /// The sort column, validated against the list the screen offers.
    pub fn item_sort(&self) -> Result<(&'static str, bool)> {
        let column = match self.sort.as_deref() {
            None | Some("") | Some("name") => "p.name",
            Some("sku") => "lower(p.sku)",
            Some("category") => "coalesce(p.category, '')",
            Some("unit") => "p.unit",
            Some("updated_at") => "p.updated_at",
            Some(other) => {
                return Err(InventoryError::InvalidQuery(format!(
                    "cannot sort items by {other}"
                )));
            }
        };
        let descending = matches!(self.direction.as_deref(), Some("desc"));
        Ok((column, descending))
    }
}

/// The stock list's query.
#[derive(Debug, Clone, Default)]
pub struct StockQuery {
    /// Free text over SKU and name.
    pub search: Option<String>,
    /// One warehouse the row must belong to.
    pub warehouse_id: Option<Uuid>,
    /// One location the row must belong to.
    pub location_id: Option<Uuid>,
    /// One item the row must belong to.
    pub item_id: Option<Uuid>,
    /// One category.
    pub category: Option<String>,
    /// The badge the row must wear. `below_threshold` is `low` or `critical`.
    pub status: Option<String>,
    /// Rows with no movement for this many days.
    pub idle_days: Option<i32>,
    /// Page size.
    pub limit: Option<i64>,
    /// Cursor of the previous page.
    pub cursor: Option<String>,
}

impl StockQuery {
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
        self.cursor.as_deref().and_then(|raw| Uuid::parse_str(raw).ok())
    }
}

// ---------------------------------------------------------------------------------------------
// Decoding helpers
// ---------------------------------------------------------------------------------------------

/// Read a `numeric` column that arrived as text into a [`Quantity`].
pub fn quantity_from_text(raw: &str) -> Result<Quantity> {
    Quantity::parse(raw)
        .map_err(|source| InventoryError::number("stock", "quantity", source))
}

/// Read an optional `numeric(14,2)` into an [`Amount`].
pub fn amount_from_text(raw: &str) -> Result<Amount> {
    Amount::parse(raw).map_err(|source| InventoryError::number("item", "cost", source))
}

/// Turn a 23505 unique-violation into the message the form shows.
///
/// PostgreSQL's own sentence names an index and a key, which is true and useless to a person
/// standing at a form; this is the reason the unique index is not the only thing standing between
/// two items and one SKU.
pub fn map_unique_violation(error: &sqlx::Error, entity: &'static str) -> Option<InventoryError> {
    let sqlx::Error::Database(db) = error else {
        return None;
    };
    if db.code().as_deref() != Some("23505") {
        return None;
    }
    let constraint = db.constraint().unwrap_or_default();
    if constraint.contains("barcode") {
        Some(InventoryError::code_taken("barcode", "this barcode"))
    } else if constraint.contains("sku") {
        Some(InventoryError::code_taken("item", "this SKU"))
    } else {
        Some(InventoryError::code_taken(entity, constraint))
    }
}

// ---------------------------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------------------------

/// The item row as it comes back from a query.
#[derive(Debug, Clone, FromRow)]
struct ItemRow {
    id: Uuid,
    organization_id: Uuid,
    sku: String,
    name: String,
    category: Option<String>,
    unit: String,
    barcode: Option<String>,
    min_threshold: String,
    reorder_point: String,
    reorder_qty: String,
    cost: Option<String>,
    currency: String,
    product_id: Option<Uuid>,
    notes: String,
    active: bool,
    archived_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl ItemRow {
    /// The module's own view, validated.
    fn into_view(self) -> Result<ItemView> {
        let cost = match self.cost.as_deref() {
            None => None,
            Some(raw) => Some(amount_from_text(raw)?),
        };
        Ok(ItemView {
            id: self.id,
            organization_id: self.organization_id,
            item: Item {
                id: self.id,
                sku: self.sku,
                name: self.name,
                category: self.category,
                unit: self.unit,
                barcode: self.barcode,
                min_threshold: quantity_from_text(&self.min_threshold)?,
                reorder_point: quantity_from_text(&self.reorder_point)?,
                reorder_qty: quantity_from_text(&self.reorder_qty)?,
                cost,
                currency: self.currency,
                product_id: self.product_id,
                notes: self.notes,
                active: self.active,
            },
            archived_at: self.archived_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

const ITEM_COLUMNS: &str = "id, organization_id, sku, name, category, unit, barcode, \
     min_threshold::text as min_threshold, reorder_point::text as reorder_point, \
     reorder_qty::text as reorder_qty, cost::text as cost, currency, product_id, notes, \
     active, archived_at, created_at, updated_at";

/// One page of items.
pub async fn list_items(
    pool: &PgPool,
    organization_id: Uuid,
    query: &ItemQuery,
) -> Result<Page<ItemView>> {
    let page_size = query.page_size();
    let cursor = query.cursor_id();
    let (sort_column, descending) = query.item_sort()?;
    let term = match query.search.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(term) => {
            if term.chars().count() > MAX_SEARCH_LENGTH {
                return Err(InventoryError::InvalidQuery(format!(
                    "the search term is longer than {MAX_SEARCH_LENGTH} characters"
                )));
            }
            Some(term.to_string())
        }
    };

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "select {ITEM_COLUMNS} from inventory_items p where p.organization_id = "
    ));
    builder.push_bind(organization_id);
    if !query.shows_archived() {
        builder.push(" and p.archived_at is null");
    }
    if let Some(active) = query.active {
        builder.push(" and p.active = ").push_bind(active);
    }
    if let Some(category) = query.category.as_deref() {
        builder.push(" and p.category = ").push_bind(category);
    }
    match query.linked_to_catalog {
        Some(true) => {
            builder.push(" and p.product_id is not null");
        }
        Some(false) => {
            builder.push(" and p.product_id is null");
        }
        None => {}
    }
    if let Some(term) = term {
        builder
            .push(" and (p.name ilike ")
            .push_bind(format!("%{term}%"))
            .push(" or p.sku ilike ")
            .push_bind(format!("%{term}%"))
            .push(" or coalesce(p.barcode, '') ilike ")
            .push_bind(format!("%{term}%"))
            .push(")");
    }
    // Keyset pagination on the id, which is stable across writes; `offset` would skip a row
    // whenever somebody records a movement while somebody else reads the second page.
    if let Some(cursor) = cursor {
        builder.push(" and p.id > ").push_bind(cursor);
    }
    builder
        .push(" order by ")
        .push(sort_column)
        .push(if descending { " desc" } else { " asc" })
        .push(", p.id asc limit ")
        .push_bind(page_size + 1);

    let rows: Vec<ItemRow> = builder.build_query_as().fetch_all(pool).await?;
    let total = count_items(pool, organization_id, query).await?;

    // The `+ 1` on the query is the look-ahead that says "there is another page", so the cursor is
    // the last id **of the page that is actually returned** — reading it off the last row of the
    // over-fetched set would hand the caller a cursor that skips a row.
    let has_more = rows.len() > page_size as usize;
    let mut items = Vec::with_capacity(rows.len().min(page_size as usize));
    for row in rows.into_iter().take(page_size as usize) {
        items.push(row.into_view()?);
    }
    let next_cursor = has_more
        .then(|| items.last().map(|view| view.id.to_string()))
        .flatten();
    Ok(Page::new(items, next_cursor, total))
}

async fn count_items(pool: &PgPool, organization_id: Uuid, query: &ItemQuery) -> Result<i64> {
    let mut count: QueryBuilder<Postgres> =
        QueryBuilder::new("select count(*) from inventory_items p where p.organization_id = ");
    count.push_bind(organization_id);
    if !query.shows_archived() {
        count.push(" and p.archived_at is null");
    }
    if let Some(active) = query.active {
        count.push(" and p.active = ").push_bind(active);
    }
    if let Some(category) = query.category.as_deref() {
        count.push(" and p.category = ").push_bind(category);
    }
    Ok(count.build_query_scalar::<i64>().fetch_one(pool).await?)
}

/// One item.
pub async fn get_item(pool: &PgPool, organization_id: Uuid, item_id: Uuid) -> Result<ItemView> {
    let row: ItemRow = sqlx::query_as(&format!(
        "select {ITEM_COLUMNS} from inventory_items p \
         where p.organization_id = $1 and p.id = $2"
    ))
    .bind(organization_id)
    .bind(item_id)
    .fetch_optional(pool)
    .await?
    .ok_or(InventoryError::NotFound("item"))?;
    row.into_view()
}

/// Create an item.
///
/// The thresholds are validated **before** the insert rather than left to the check constraint,
/// so the form gets a message naming the field instead of PostgreSQL's sentence naming a
/// constraint.
pub async fn create_item(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewItem,
) -> Result<ItemView> {
    let sku = items::validate_sku(&new.sku)?;
    let name = items::validate_name("item", &new.name)?;
    let barcode = items::normalize_barcode(new.barcode.as_deref())?;
    let min_threshold = quantity_or_zero("item", "min_threshold", new.min_threshold.as_str())?;
    let reorder_point = quantity_or_zero("item", "reorder_point", new.reorder_point.as_str())?;
    let reorder_qty = quantity_or_zero("item", "reorder_qty", new.reorder_qty.as_str())?;
    let (min_threshold, reorder_point) = items::validate_thresholds(min_threshold, reorder_point)?;
    let cost = match new.cost.as_deref() {
        None | Some("") => None,
        Some(raw) => Some(
            Amount::parse(raw)
                .map_err(|source| InventoryError::number("item", "cost", source))?,
        ),
    };
    if let Some(notes) = new.notes.chars().count().checked_sub(items::MAX_NOTES_LENGTH) {
        if notes > 0 {
            return Err(InventoryError::invalid(
                "item",
                "notes",
                format!("keep the notes under {} characters", items::MAX_NOTES_LENGTH),
            ));
        }
    }
    let category = items::normalize_category(new.category.as_deref());

    // The unit falls back to the organization's setting, so a form that leaves it alone still
    // produces a row a person can read rather than an empty string.
    let unit = match new.unit.as_deref() {
        Some(raw) => items::validate_unit(raw)?,
        None => default_unit(pool, organization_id).await?,
    };

    let sql = "insert into inventory_items (organization_id, sku, name, category, unit, barcode, \
               min_threshold, reorder_point, reorder_qty, cost, product_id, notes) \
               values ($1, $2, $3, $4, $5, $6, $7::numeric, $8::numeric, $9::numeric, \
               $10::numeric, $11, $12) returning id";
    let inserted: std::result::Result<Uuid, sqlx::Error> = sqlx::query_scalar(sql)
        .bind(organization_id)
        .bind(&sku)
        .bind(&name)
        .bind(&category)
        .bind(&unit)
        .bind(&barcode)
        .bind(min_threshold.to_text())
        .bind(reorder_point.to_text())
        .bind(reorder_qty.to_text())
        .bind(cost.map(|value| value.to_text()))
        .bind(new.product_id)
        .bind(&new.notes)
        .fetch_one(pool)
        .await;

    let id = match inserted {
        Ok(id) => id,
        Err(error) => {
            return Err(map_unique_violation(&error, "item").unwrap_or(error.into()));
        }
    };
    get_item(pool, organization_id, id).await
}

/// Update an item. Only the fields the caller named are written.
///
/// `None` means "not mentioned" and an empty string means "cleared" for the nullable columns —
/// the difference is the whole reason this is a `PATCH` built from a `QueryBuilder` rather than a
/// `select` of the row and an `update` of all of it.
pub async fn patch_item(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
    patch: &ItemPatch,
) -> Result<ItemView> {
    // The row has to exist **in this organization** before anything is written, or the update
    // below would answer "no rows" for a row that exists elsewhere and the caller would read a
    // 404 as a no-op rather than as a refusal.
    get_item(pool, organization_id, item_id).await?;

    let mut builder: QueryBuilder<Postgres> =
        QueryBuilder::new("update inventory_items set updated_at = now()");
    if let Some(name) = patch.name.as_deref() {
        builder
            .push(", name = ")
            .push_bind(items::validate_name("item", name)?);
    }
    if let Some(category) = patch.category.as_deref() {
        builder
            .push(", category = ")
            .push_bind(items::normalize_category(Some(category)));
    }
    if let Some(unit) = patch.unit.as_deref() {
        builder
            .push(", unit = ")
            .push_bind(items::validate_unit(unit)?);
    }
    if let Some(barcode) = patch.barcode.as_deref() {
        builder
            .push(", barcode = ")
            .push_bind(items::normalize_barcode(Some(barcode))?);
    }
    if patch.min_threshold.is_some() || patch.reorder_point.is_some() {
        let current = get_item(pool, organization_id, item_id).await?;
        let min_threshold = match patch.min_threshold.as_deref() {
            Some(raw) => parse_quantity("item", "min_threshold", raw)?,
            None => current.item.min_threshold,
        };
        let reorder_point = match patch.reorder_point.as_deref() {
            Some(raw) => parse_quantity("item", "reorder_point", raw)?,
            None => current.item.reorder_point,
        };
        let (min_threshold, reorder_point) = items::validate_thresholds(min_threshold, reorder_point)?;
        if patch.min_threshold.is_some() {
            builder
                .push(", min_threshold = ")
                .push_bind(min_threshold.to_text())
                .push("::numeric");
        }
        if patch.reorder_point.is_some() {
            builder
                .push(", reorder_point = ")
                .push_bind(reorder_point.to_text())
                .push("::numeric");
        }
    }
    if let Some(raw) = patch.reorder_qty.as_deref() {
        builder
            .push(", reorder_qty = ")
            .push_bind(quantity_or_zero("item", "reorder_qty", raw)?.to_text())
            .push("::numeric");
    }
    if let Some(cost) = patch.cost.as_deref() {
        let value = if cost.trim().is_empty() {
            None
        } else {
            Some(Amount::parse(cost).map_err(|source| InventoryError::number("item", "cost", source))?)
        };
        builder
            .push(", cost = ")
            .push_bind(value.map(|amount| amount.to_text()))
            .push("::numeric");
    }
    if let Some(product_id) = patch.product_id.as_ref() {
        builder.push(", product_id = ").push_bind(*product_id);
    }
    if let Some(notes) = patch.notes.as_deref() {
        if notes.chars().count() > items::MAX_NOTES_LENGTH {
            return Err(InventoryError::invalid(
                "item",
                "notes",
                format!("keep the notes under {} characters", items::MAX_NOTES_LENGTH),
            ));
        }
        builder.push(", notes = ").push_bind(notes);
    }
    if let Some(active) = patch.active {
        builder.push(", active = ").push_bind(active);
    }
    builder
        .push(" where organization_id = ")
        .push_bind(organization_id)
        .push(" and id = ")
        .push_bind(item_id);

    let outcome = builder.build().execute(pool).await;
    if let Err(error) = outcome {
        return Err(map_unique_violation(&error, "item").unwrap_or(error.into()));
    }
    get_item(pool, organization_id, item_id).await
}

/// Archive an item — `DELETE` on the route, `archived_at` in the row.
///
/// A refusal rather than a silent no-op when the item still holds stock: an archived item with a
/// balance is not a mistake the person made, it is a question ("archive it anyway?") and
/// answering it for them is how a warehouse ends up with stock nobody can draw.
pub async fn archive_item(pool: &PgPool, organization_id: Uuid, item_id: Uuid) -> Result<ItemView> {
    let balances: i64 = sqlx::query_scalar(
        "select count(*) from inventory_stock s \
         join inventory_locations l on l.id = s.location_id \
         where s.organization_id = $1 and s.item_id = $2 and s.on_hand <> 0",
    )
    .bind(organization_id)
    .bind(item_id)
    .fetch_one(pool)
    .await?;
    if balances > 0 {
        return Err(InventoryError::InvalidStatusChange(format!(
            "this item still holds stock at {balances} location(s) — move or write it off first"
        )));
    }
    sqlx::query(
        "update inventory_items set archived_at = now(), active = false, updated_at = now() \
         where organization_id = $1 and id = $2 and archived_at is null",
    )
    .bind(organization_id)
    .bind(item_id)
    .execute(pool)
    .await?;
    get_item(pool, organization_id, item_id).await
}

/// Resolve an item by barcode or SKU — the scanner box's one call.
///
/// **Barcode first, SKU second**, and the order matters: a scanner emits digits and a SKU may be
/// digits too, so asking "is this a SKU?" first would find `1234` when the label is a barcode
/// belonging to a different item. The barcode is the unambiguous key, so it is asked first.
pub async fn lookup_item(
    pool: &PgPool,
    organization_id: Uuid,
    raw: &str,
) -> Result<Option<ItemView>> {
    let normalized = items::normalize_barcode(Some(raw))?;
    if let Some(barcode) = normalized {
        let found: Option<ItemRow> = sqlx::query_as(&format!(
            "select {ITEM_COLUMNS} from inventory_items p \
             where p.organization_id = $1 and p.barcode = $2 and p.archived_at is null"
        ))
        .bind(organization_id)
        .bind(&barcode)
        .fetch_optional(pool)
        .await?;
        if let Some(row) = found {
            return Ok(Some(row.into_view()?));
        }
    }
    let sku = raw.trim();
    let found: Option<ItemRow> = sqlx::query_as(&format!(
        "select {ITEM_COLUMNS} from inventory_items p \
         where p.organization_id = $1 and lower(p.sku) = lower($2) and p.archived_at is null"
    ))
    .bind(organization_id)
    .bind(sku)
    .fetch_optional(pool)
    .await?;
    found.map(ItemRow::into_view).transpose()
}

/// Every category the organization has written, for the filter and the stocktake scope.
pub async fn list_categories(pool: &PgPool, organization_id: Uuid) -> Result<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "select distinct category from inventory_items \
         where organization_id = $1 and category is not null and archived_at is null \
         order by category",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Every unit the organization has written, for the item form's combobox.
pub async fn list_units(pool: &PgPool, organization_id: Uuid) -> Result<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "select distinct unit from inventory_items \
         where organization_id = $1 and archived_at is null order by unit",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// Warehouses and locations
// ---------------------------------------------------------------------------------------------

/// The tree: every warehouse with its locations, item count and on-hand total.
///
/// One request rather than N+1, because the tree editor draws a warehouse with its children and a
/// screen that fetches them per node is a screen that flickers and a load that is N times the
/// obvious one.
pub async fn list_warehouses(pool: &PgPool, organization_id: Uuid) -> Result<Vec<WarehouseView>> {
    let mut warehouses: Vec<WarehouseView> = sqlx::query_as::<_, WarehouseRow>(
        "select id, organization_id, code, name, active, created_at from inventory_warehouses \
         where organization_id = $1 order by code",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(WarehouseRow::into_view)
    .collect::<Result<Vec<_>>>()?;

    let locations = list_locations(pool, organization_id).await?;
    let item_count: i64 = sqlx::query_scalar(
        "select count(*) from inventory_items \
         where organization_id = $1 and archived_at is null",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    let mut per_location: std::collections::HashMap<Uuid, (i64, i128)> = Default::default();
    let rows = sqlx::query(
        "select location_id, sum(on_hand)::text as total from inventory_stock \
         where organization_id = $1 group by location_id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    for row in rows {
        let location_id: Uuid = row.get("location_id");
        let total: i128 = quantity_from_text(row.get::<&str, _>("total"))?.milli();
        per_location.insert(location_id, (1, total));
    }

    for warehouse in &mut warehouses {
        // The total is stamped onto a **copy** before it is pushed: the shared `locations` list is
        // still borrowed by the iterator, and writing through it would mutate every view of the
        // tree at once — which is invisible until a second read of the same list shows a number
        // belonging to a different warehouse.
        for location in locations.iter().filter(|l| l.warehouse_id == warehouse.id) {
            let total = per_location
                .get(&location.id)
                .map(|(_, total)| *total)
                .unwrap_or(0);
            let mut row = location.clone();
            row.on_hand_total = Quantity::from_milli(total)
                .unwrap_or(Quantity::ZERO)
                .to_text();
            warehouse.locations.push(row);
        }
        warehouse.item_count = item_count;
    }
    Ok(warehouses)
}

#[derive(Debug, Clone, FromRow)]
struct WarehouseRow {
    id: Uuid,
    organization_id: Uuid,
    code: String,
    name: String,
    active: bool,
    created_at: OffsetDateTime,
}

impl WarehouseRow {
    fn into_view(self) -> Result<WarehouseView> {
        Ok(WarehouseView {
            id: self.id,
            organization_id: self.organization_id,
            code: self.code,
            name: self.name,
            active: self.active,
            locations: Vec::new(),
            item_count: 0,
            on_hand_total: Quantity::ZERO.to_text(),
            created_at: self.created_at,
        })
    }
}

#[derive(Debug, Clone, FromRow)]
struct LocationRow {
    id: Uuid,
    organization_id: Uuid,
    warehouse_id: Uuid,
    code: String,
    name: String,
    kind: String,
    active: bool,
    created_at: OffsetDateTime,
}

impl LocationRow {
    fn into_view(self) -> Result<LocationView> {
        let kind = LocationKind::parse(&self.kind).ok_or_else(|| {
            InventoryError::invalid("location", "kind", format!("{} is not a location kind", self.kind))
        })?;
        Ok(LocationView {
            id: self.id,
            organization_id: self.organization_id,
            warehouse_id: self.warehouse_id,
            code: self.code,
            name: self.name,
            kind,
            active: self.active,
            on_hand_total: Quantity::ZERO.to_text(),
            created_at: self.created_at,
        })
    }
}

/// Every location of the organization, ordered by warehouse then code.
pub async fn list_locations(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<LocationView>> {
    let rows: Vec<LocationRow> = sqlx::query_as(
        "select l.id, l.organization_id, l.warehouse_id, l.code, l.name, l.kind, l.active, \
                l.created_at \
         from inventory_locations l \
         join inventory_warehouses w on w.id = l.warehouse_id \
         where l.organization_id = $1 order by w.code, l.code",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(LocationRow::into_view).collect()
}

/// One location.
pub async fn get_location(
    pool: &PgPool,
    organization_id: Uuid,
    location_id: Uuid,
) -> Result<LocationView> {
    let row: Option<LocationRow> = sqlx::query_as(
        "select id, organization_id, warehouse_id, code, name, kind, active, created_at \
         from inventory_locations where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(location_id)
    .fetch_optional(pool)
    .await?;
    row.ok_or(InventoryError::NotFound("location"))?
        .into_view()
}

/// Create a warehouse.
pub async fn create_warehouse(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewWarehouse,
) -> Result<WarehouseView> {
    let code = items::validate_code("warehouse", &new.code)?;
    let name = items::validate_name("warehouse", &new.name)?;
    let inserted: std::result::Result<Uuid, sqlx::Error> = sqlx::query_scalar(
        "insert into inventory_warehouses (organization_id, code, name) values ($1, $2, $3) \
         returning id",
    )
    .bind(organization_id)
    .bind(&code)
    .bind(&name)
    .fetch_one(pool)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(error) => {
            return Err(map_unique_violation(&error, "warehouse").unwrap_or(error.into()));
        }
    };
    get_warehouse(pool, organization_id, id).await
}

/// One warehouse, with its locations — the detail the editor opens.
pub async fn get_warehouse(
    pool: &PgPool,
    organization_id: Uuid,
    warehouse_id: Uuid,
) -> Result<WarehouseView> {
    list_warehouses(pool, organization_id)
        .await?
        .into_iter()
        .find(|warehouse| warehouse.id == warehouse_id)
        .ok_or(InventoryError::NotFound("warehouse"))
}

/// Rename or deactivate a warehouse.
pub async fn patch_warehouse(
    pool: &PgPool,
    organization_id: Uuid,
    warehouse_id: Uuid,
    patch: &WarehousePatch,
) -> Result<WarehouseView> {
    get_warehouse(pool, organization_id, warehouse_id).await?;
    let mut builder: QueryBuilder<Postgres> =
        QueryBuilder::new("update inventory_warehouses set updated_at = now()");
    if let Some(name) = patch.name.as_deref() {
        builder
            .push(", name = ")
            .push_bind(items::validate_name("warehouse", name)?);
    }
    if let Some(active) = patch.active {
        // Deactivating a warehouse that still holds stock is refused **by name**: the operator is
        // told where the stock is, because "cannot deactivate" without the location is a question
        // they have to go and answer by hand.
        if !active {
            let holders: Vec<(String, String)> = sqlx::query_as(
                "select w.code, l.code from inventory_stock s \
                 join inventory_locations l on l.id = s.location_id \
                 join inventory_warehouses w on w.id = l.warehouse_id \
                 where s.organization_id = $1 and l.warehouse_id = $2 and s.on_hand <> 0 \
                 group by w.code, l.code order by l.code limit 3",
            )
            .bind(organization_id)
            .bind(warehouse_id)
            .fetch_all(pool)
            .await?;
            if !holders.is_empty() {
                let where_ = holders
                    .iter()
                    .map(|(warehouse, location)| format!("{warehouse}/{location}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(InventoryError::InvalidStatusChange(format!(
                    "this warehouse still holds stock at {where_} — move or write it off first"
                )));
            }
        }
        builder.push(", active = ").push_bind(active);
    }
    builder
        .push(" where organization_id = ")
        .push_bind(organization_id)
        .push(" and id = ")
        .push_bind(warehouse_id);
    builder.build().execute(pool).await?;
    get_warehouse(pool, organization_id, warehouse_id).await
}

/// Create a location under a warehouse.
///
/// The warehouse has to be **in this organization**, checked here rather than left to the foreign
/// key: a warehouse id from another tenant would otherwise produce a constraint violation (a 500)
/// on a write that should have been a 404.
pub async fn create_location(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewLocation,
) -> Result<LocationView> {
    get_warehouse(pool, organization_id, new.warehouse_id).await?;
    let code = items::validate_code("location", &new.code)?;
    let name = items::validate_name("location", &new.name)?;
    let kind = match new.kind.as_deref() {
        None | Some("") => LocationKind::Internal,
        Some(raw) => LocationKind::parse(raw.trim()).ok_or_else(|| {
            InventoryError::invalid("location", "kind", format!("{raw} is not a location kind"))
        })?,
    };
    let inserted: std::result::Result<Uuid, sqlx::Error> = sqlx::query_scalar(
        "insert into inventory_locations (organization_id, warehouse_id, code, name, kind) \
         values ($1, $2, $3, $4, $5) returning id",
    )
    .bind(organization_id)
    .bind(new.warehouse_id)
    .bind(&code)
    .bind(&name)
    .bind(kind.as_str())
    .fetch_one(pool)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(error) => {
            return Err(map_unique_violation(&error, "location").unwrap_or(error.into()));
        }
    };
    get_location(pool, organization_id, id).await
}

/// Rename, re-kind or deactivate a location.
pub async fn patch_location(
    pool: &PgPool,
    organization_id: Uuid,
    location_id: Uuid,
    patch: &LocationPatch,
) -> Result<LocationView> {
    get_location(pool, organization_id, location_id).await?;
    let mut builder: QueryBuilder<Postgres> =
        QueryBuilder::new("update inventory_locations set updated_at = now()");
    if let Some(name) = patch.name.as_deref() {
        builder
            .push(", name = ")
            .push_bind(items::validate_name("location", name)?);
    }
    if let Some(kind) = patch.kind.as_deref() {
        let kind = LocationKind::parse(kind.trim()).ok_or_else(|| {
            InventoryError::invalid("location", "kind", format!("{kind} is not a location kind"))
        })?;
        builder.push(", kind = ").push_bind(kind.as_str());
    }
    if let Some(active) = patch.active {
        if !active {
            let on_hand: String = sqlx::query_scalar(
                "select coalesce(sum(on_hand), 0)::text from inventory_stock \
                 where organization_id = $1 and location_id = $2",
            )
            .bind(organization_id)
            .bind(location_id)
            .fetch_one(pool)
            .await?;
            let on_hand = quantity_from_text(&on_hand)?;
            if !on_hand.is_zero() {
                return Err(InventoryError::InvalidStatusChange(format!(
                    "this location still holds {on_hand} — move or write it off first"
                )));
            }
        }
        builder.push(", active = ").push_bind(active);
    }
    builder
        .push(" where organization_id = ")
        .push_bind(organization_id)
        .push(" and id = ")
        .push_bind(location_id);
    builder.build().execute(pool).await?;
    get_location(pool, organization_id, location_id).await
}

// ---------------------------------------------------------------------------------------------
// Stock
// ---------------------------------------------------------------------------------------------

const STOCK_COLUMNS: &str = "s.id, s.item_id, i.sku, i.name, i.category, i.unit, s.location_id, \
     l.code as location_code, l.name as location_name, l.warehouse_id, w.code as warehouse_code, \
     s.on_hand::text as on_hand, s.reserved::text as reserved, \
     i.min_threshold::text as min_threshold, i.reorder_point::text as reorder_point, \
     s.last_movement_at";

/// One row of the stock list, decoded.
fn stock_level_from_row(row: &PgRow) -> Result<StockLevel> {
    let on_hand = quantity_from_text(row.get::<&str, _>("on_hand"))?;
    let reserved = quantity_from_text(row.get::<&str, _>("reserved"))?;
    let min_threshold = quantity_from_text(row.get::<&str, _>("min_threshold"))?;
    let reorder_point = quantity_from_text(row.get::<&str, _>("reorder_point"))?;
    let available = on_hand.checked_sub(reserved).unwrap_or(Quantity::ZERO);
    Ok(StockLevel {
        id: row.get("id"),
        item_id: row.get("item_id"),
        sku: row.get("sku"),
        name: row.get("name"),
        category: row.get("category"),
        unit: row.get("unit"),
        location_id: row.get("location_id"),
        location_code: row.get("location_code"),
        location_name: row.get("location_name"),
        warehouse_id: row.get("warehouse_id"),
        warehouse_code: row.get("warehouse_code"),
        on_hand,
        reserved,
        available,
        min_threshold,
        reorder_point,
        status: StockStatus::of(available, min_threshold, reorder_point),
        last_movement_at: row.get("last_movement_at"),
    })
}

/// Every stock row of an item, in the order the detail draws them.
pub async fn stock_for_item(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
) -> Result<Vec<StockLevel>> {
    let rows = sqlx::query(&format!(
        "select {STOCK_COLUMNS} from inventory_stock s \
         join inventory_items i on i.id = s.item_id \
         join inventory_locations l on l.id = s.location_id \
         join inventory_warehouses w on w.id = l.warehouse_id \
         where s.organization_id = $1 and s.item_id = $2 \
         order by w.code, l.code"
    ))
    .bind(organization_id)
    .bind(item_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(stock_level_from_row).collect()
}

/// The item detail's position: the item, its per-location rows and the organization-wide totals.
///
/// The totals are **summed from the rows this call returned**, not from a second query, so the
/// number the header prints is the number the table under it adds up to. A separate `sum()`
/// would be a second answer to the same question and the two would drift on the day a location is
/// deactivated.
pub async fn item_position(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
) -> Result<StockPosition> {
    let item = get_item(pool, organization_id, item_id).await?;
    let locations = stock_for_item(pool, organization_id, item_id).await?;

    let mut on_hand = Quantity::ZERO;
    let mut reserved = Quantity::ZERO;
    for row in &locations {
        on_hand = on_hand.checked_add(row.on_hand).unwrap_or(on_hand);
        reserved = reserved.checked_add(row.reserved).unwrap_or(reserved);
    }
    let available = on_hand.checked_sub(reserved).unwrap_or(Quantity::ZERO);
    let last_movement_at = locations.iter().filter_map(|row| row.last_movement_at).max();

    Ok(StockPosition {
        status: StockStatus::of(available, item.item.min_threshold, item.item.reorder_point),
        item,
        locations,
        on_hand,
        reserved,
        available,
        last_movement_at,
    })
}

/// One page of the stock list.
pub async fn list_stock(
    pool: &PgPool,
    organization_id: Uuid,
    query: &StockQuery,
) -> Result<Page<StockLevel>> {
    let page_size = query.page_size();
    let cursor = query.cursor_id();
    let term = match query.search.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(term) => {
            if term.chars().count() > MAX_SEARCH_LENGTH {
                return Err(InventoryError::InvalidQuery(format!(
                    "the search term is longer than {MAX_SEARCH_LENGTH} characters"
                )));
            }
            Some(term.to_string())
        }
    };

    // `None` means "no filter", and the five tokens are validated against the enum rather than
    // passed through: a filter that silently matched nothing makes an operator conclude the
    // warehouse is empty, which is the most expensive possible wrong answer on this screen.
    let status: Option<String> = match query.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some("below_threshold") => Some("below_threshold".to_string()),
        Some(other) => {
            let parsed = StockStatus::parse(other).ok_or_else(|| {
                InventoryError::InvalidQuery(format!(
                    "{other} is not a stock status — use one of {}",
                    StockStatus::ALL_FILTERS.join(", ")
                ))
            })?;
            Some(parsed.as_str().to_string())
        }
    };

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "select {STOCK_COLUMNS} from inventory_stock s \
         join inventory_items i on i.id = s.item_id \
         join inventory_locations l on l.id = s.location_id \
         join inventory_warehouses w on w.id = l.warehouse_id \
         where s.organization_id = "
    ));
    builder.push_bind(organization_id);
    if let Some(item_id) = query.item_id {
        builder.push(" and s.item_id = ").push_bind(item_id);
    }
    if let Some(location_id) = query.location_id {
        builder.push(" and s.location_id = ").push_bind(location_id);
    }
    if let Some(warehouse_id) = query.warehouse_id {
        builder.push(" and l.warehouse_id = ").push_bind(warehouse_id);
    }
    if let Some(category) = query.category.as_deref() {
        builder.push(" and i.category = ").push_bind(category);
    }
    if let Some(term) = term {
        builder
            .push(" and (i.name ilike ")
            .push_bind(format!("%{term}%"))
            .push(" or i.sku ilike ")
            .push_bind(format!("%{term}%"))
            .push(")");
    }
    if let Some(status) = status.as_deref() {
        // The status is computed from the same expression the badge uses
        // (`on_hand - reserved` against the two thresholds), written here once as SQL so the
        // filter and the badge cannot disagree. `below_threshold` is the union of the two amber
        // states rather than a fourth badge, because "what needs reordering?" does not care which
        // of the two it is.
        // The rank is written here once, in the same order as `StockStatus::of`: 1 negative,
        // 2 critical, 3 low, 4 ok. The badge and the filter read the same four cases, so a row
        // cannot be badged one thing and hidden by a filter that thinks another.
        builder
            .push(" and case when s.on_hand - s.reserved < 0 then 1 ")
            .push("when s.on_hand - s.reserved <= i.min_threshold then 2 ")
            .push("when s.on_hand - s.reserved <= i.reorder_point then 3 else 4 end ")
            .push(match status {
                "negative" => "= 1",
                "critical" => "= 2",
                "low" => "= 3",
                "ok" => "= 4",
                _ => "in (2, 3)",
            });
    }
    if let Some(days) = query.idle_days {
        if !(1..=3650).contains(&days) {
            return Err(InventoryError::InvalidQuery(
                "the idle filter is a number of days between 1 and 3650".into(),
            ));
        }
        // "Idle" means **no movement at all**, which is `last_movement_at is null` as well as old:
        // a stock row created by importing an opening balance has a timestamp, and a row an
        // item has never occupied has none, and both are equally idle to a person asking the
        // question.
        builder
            .push(" and (s.last_movement_at is null or s.last_movement_at < now() - make_interval(days => ")
            .push_bind(days as i32)
            .push("))");
    }
    if let Some(cursor) = cursor {
        builder.push(" and s.id > ").push_bind(cursor);
    }
    builder
        .push(" order by s.id asc limit ")
        .push_bind(page_size + 1);

    let rows = builder.build().fetch_all(pool).await?;
    let total = count_stock(pool, organization_id, query).await?;
    let levels = rows.iter().map(stock_level_from_row).collect::<Result<Vec<_>>>()?;
    let next_cursor = if rows.len() > page_size as usize {
        levels.last().map(|row| row.id.to_string())
    } else {
        None
    };
    let mut levels = levels;
    levels.truncate(page_size as usize);
    Ok(Page::new(levels, next_cursor, total))
}

async fn count_stock(pool: &PgPool, organization_id: Uuid, query: &StockQuery) -> Result<i64> {
    let mut count: QueryBuilder<Postgres> = QueryBuilder::new(
        "select count(*) from inventory_stock s \
         join inventory_items i on i.id = s.item_id \
         join inventory_locations l on l.id = s.location_id \
         where s.organization_id = ",
    );
    count.push_bind(organization_id);
    if let Some(item_id) = query.item_id {
        count.push(" and s.item_id = ").push_bind(item_id);
    }
    if let Some(location_id) = query.location_id {
        count.push(" and s.location_id = ").push_bind(location_id);
    }
    if let Some(warehouse_id) = query.warehouse_id {
        count.push(" and l.warehouse_id = ").push_bind(warehouse_id);
    }
    if let Some(days) = query.idle_days {
        count
            .push(" and (s.last_movement_at is null or s.last_movement_at < now() - make_interval(days => ")
            .push_bind(days as i32)
            .push("))");
    }
    Ok(count.build_query_scalar::<i64>().fetch_one(pool).await?)
}

/// The overview's numbers: items, rows below their threshold, negative rows, idle rows, the summed
/// on-hand and the movements recorded today.
///
/// One query per figure rather than one big `select`, because each is a different table and a
/// single `cross join` of aggregates is a query nobody can read the plan of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overview {
    /// How many live items the organization has.
    pub item_count: i64,
    /// How many warehouses and locations.
    pub warehouse_count: i64,
    pub location_count: i64,
    /// Rows at or below their reorder point (low or critical).
    pub below_threshold: i64,
    /// Rows whose `available` is below zero.
    pub negative: i64,
    /// Rows with no movement in 30 days.
    pub idle_30: i64,
    /// The summed `on_hand` across every row, as text.
    pub on_hand_total: String,
    /// How many movements were recorded today.
    pub movements_today: i64,
}

/// The overview's figures.
pub async fn overview(pool: &PgPool, organization_id: Uuid) -> Result<Overview> {
    let item_count: i64 = sqlx::query_scalar(
        "select count(*) from inventory_items where organization_id = $1 and archived_at is null",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    let warehouse_count: i64 =
        sqlx::query_scalar("select count(*) from inventory_warehouses where organization_id = $1")
            .bind(organization_id)
            .fetch_one(pool)
            .await?;
    let location_count: i64 =
        sqlx::query_scalar("select count(*) from inventory_locations where organization_id = $1")
            .bind(organization_id)
            .fetch_one(pool)
            .await?;
    let below_threshold: i64 = sqlx::query_scalar(
        "select count(*) from inventory_stock s join inventory_items i on i.id = s.item_id \
         where s.organization_id = $1 and s.on_hand - s.reserved between 0 and i.reorder_point",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    let negative: i64 = sqlx::query_scalar(
        "select count(*) from inventory_stock s \
         where s.organization_id = $1 and s.on_hand - s.reserved < 0",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    let idle_30: i64 = sqlx::query_scalar(
        "select count(*) from inventory_stock s join inventory_items i on i.id = s.item_id \
         where s.organization_id = $1 and i.archived_at is null \
         and (s.last_movement_at is null or s.last_movement_at < now() - interval '30 days')",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    let on_hand_total: String = sqlx::query_scalar(
        "select coalesce(sum(on_hand), 0)::text from inventory_stock where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    let movements_today: i64 = sqlx::query_scalar(
        "select count(*) from inventory_movements \
         where organization_id = $1 and created_at >= date_trunc('day', now())",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    Ok(Overview {
        item_count,
        warehouse_count,
        location_count,
        below_threshold,
        negative,
        idle_30,
        on_hand_total,
        movements_today,
    })
}

/// Compare the rollup against the replayed ledger for every item × location.
///
/// **The report is a list, not a count.** A count tells somebody the module is wrong and not
/// where; this tells them where, which is the only reason anyone runs it.
pub async fn reconciliation_report(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<StocktakeSnapshot> {
    let replayed = crate::ledger::replay(pool, organization_id).await?;
    let rows = sqlx::query(
        "select s.id, s.item_id, s.location_id, s.on_hand::text as on_hand, \
                s.reserved::text as reserved \
         from inventory_stock s where s.organization_id = $1",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut mismatches = Vec::new();
    for row in &rows {
        let id: Uuid = row.get("id");
        let rollup_on_hand = quantity_from_text(row.get::<&str, _>("on_hand"))?;
        let rollup_reserved = quantity_from_text(row.get::<&str, _>("reserved"))?;
        // A row that exists only in the rollup is a disagreement too, and the one a spot check
        // never finds: nothing in the ledger mentions it, so "sum the movements" says nothing is
        // wrong.
        let expected = replayed.get(&id).copied().unwrap_or_default();
        // **Both** numbers, and the conjunction is the point. This used to compare `on_hand` only
        // and then report `replay`ed `reserved` as the rollup's own figure — a column that agreed
        // with itself by construction and therefore could never fail. That was harmless while no
        // write moved `reserved`; slice 5 made holds real, and a bridge that corrupted the
        // reservation column would have been reported as clean. A check that cannot fail is worse
        // than no check, because the report is what everybody trusts.
        if expected.on_hand.milli() != rollup_on_hand.milli()
            || expected.reserved.milli() != rollup_reserved.milli()
        {
            mismatches.push(StockMismatch {
                item_id: row.get("item_id"),
                location_id: row.get("location_id"),
                rollup_on_hand: rollup_on_hand.to_text(),
                replayed_on_hand: expected.on_hand.to_text(),
                rollup_reserved: rollup_reserved.to_text(),
                replayed_reserved: expected.reserved.to_text(),
            });
        }
    }
    Ok(StocktakeSnapshot {
        checked: rows.len() as i64,
        mismatches,
    })
}

/// The organization's settings, with the defaults filled in for a row that was never written.
///
/// A missing row answers with the defaults rather than erroring: an installation that has never
/// opened the settings screen must still be able to record a movement, and REQ-051's board taught
/// this module what a "the world was seeded on day one" assumption costs when it is wrong.
pub async fn get_settings(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<crate::model::Settings> {
    let row: Option<(String, String, bool, String)> = sqlx::query_as(
        "select adjustment_approval_threshold::text, default_adjustment_reason, alerts_on_read, \
                default_unit from inventory_settings where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    let Some((threshold, reason, alerts_on_read, default_unit)) = row else {
        return Ok(crate::model::Settings::defaults());
    };
    let reason = crate::model::ReasonCode::parse(&reason)
        .unwrap_or(crate::model::ReasonCode::Correction);
    Ok(crate::model::Settings {
        adjustment_approval_threshold: quantity_from_text(&threshold)?,
        default_adjustment_reason: reason,
        alerts_on_read,
        default_unit,
    })
}

/// The organization's default unit, falling back to the module's own default.
pub async fn default_unit(pool: &PgPool, organization_id: Uuid) -> Result<String> {
    Ok(get_settings(pool, organization_id).await?.default_unit)
}

/// A patch of the settings row.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SettingsPatch {
    /// Above this many units, one adjustment needs a decision.
    #[serde(default)]
    pub adjustment_approval_threshold: Option<String>,
    /// The reason a new adjustment proposes.
    #[serde(default)]
    pub default_adjustment_reason: Option<String>,
    /// Whether the low-stock sweep runs when a screen is read.
    #[serde(default)]
    pub alerts_on_read: Option<bool>,
    /// The default unit for a new item.
    #[serde(default)]
    pub default_unit: Option<String>,
}

/// Write the settings row, creating it when it does not exist.
///
/// `on conflict do update` rather than "insert, and if it exists update": the row is created by
/// the organization trigger and a caller that deleted it should not get a unique violation for
/// having used the module correctly.
pub async fn update_settings(
    pool: &PgPool,
    organization_id: Uuid,
    patch: &SettingsPatch,
) -> Result<crate::model::Settings> {
    let current = get_settings(pool, organization_id).await?;
    let threshold = match patch.adjustment_approval_threshold.as_deref() {
        Some(raw) => {
            let value = parse_quantity("settings", "adjustment_approval_threshold", raw)?;
            if value.is_negative() {
                return Err(InventoryError::invalid(
                    "settings",
                    "adjustment_approval_threshold",
                    "the threshold is a quantity and cannot be negative",
                ));
            }
            value
        }
        None => current.adjustment_approval_threshold,
    };
    let reason = match patch.default_adjustment_reason.as_deref() {
        Some(raw) => crate::model::ReasonCode::parse(raw.trim()).ok_or_else(|| {
            InventoryError::invalid(
                "settings",
                "default_adjustment_reason",
                format!("{raw} is not a reason code"),
            )
        })?,
        None => current.default_adjustment_reason,
    };
    let alerts_on_read = patch.alerts_on_read.unwrap_or(current.alerts_on_read);
    let default_unit = match patch.default_unit.as_deref() {
        Some(raw) => items::validate_unit(raw)?,
        None => current.default_unit,
    };

    sqlx::query(
        "insert into inventory_settings (organization_id, adjustment_approval_threshold, \
             default_adjustment_reason, alerts_on_read, default_unit) \
         values ($1, $2::numeric, $3, $4, $5) \
         on conflict (organization_id) do update set \
             adjustment_approval_threshold = excluded.adjustment_approval_threshold, \
             default_adjustment_reason = excluded.default_adjustment_reason, \
             alerts_on_read = excluded.alerts_on_read, \
             default_unit = excluded.default_unit, \
             updated_at = now()",
    )
    .bind(organization_id)
    .bind(threshold.to_text())
    .bind(reason.as_str())
    .bind(alerts_on_read)
    .bind(&default_unit)
    .execute(pool)
    .await?;
    get_settings(pool, organization_id).await
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

/// Parse a quantity a caller sent, naming the field on failure.
pub fn parse_quantity(entity: &'static str, field: &'static str, raw: &str) -> Result<Quantity> {
    Quantity::parse(raw).map_err(|source| InventoryError::number(entity, field, source))
}

/// An empty or absent number is zero, which is what "the operator did not set a threshold" means.
fn quantity_or_zero(entity: &'static str, field: &'static str, raw: &str) -> Result<Quantity> {
    if raw.trim().is_empty() {
        Ok(Quantity::ZERO)
    } else {
        parse_quantity(entity, field, raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_defaults_to_fifty_rows_and_caps_at_two_hundred() {
        let query = ItemQuery::default();
        assert_eq!(query.page_size(), DEFAULT_PER_PAGE);
        assert_eq!(
            ItemQuery {
                limit: Some(1_000),
                ..ItemQuery::default()
            }
            .page_size(),
            MAX_PER_PAGE
        );
        assert_eq!(
            ItemQuery {
                limit: Some(0),
                ..ItemQuery::default()
            }
            .page_size(),
            DEFAULT_PER_PAGE,
            "a page size of zero would return nothing and look like an empty list"
        );
    }

    #[test]
    fn an_unknown_sort_column_is_refused_by_name() {
        let query = ItemQuery {
            sort: Some("secret".into()),
            ..ItemQuery::default()
        };
        assert!(query.item_sort().unwrap_err().to_string().contains("secret"));
    }

    #[test]
    fn an_unknown_status_is_refused_with_the_list_of_the_ones_that_work() {
        // A filter that silently matched nothing is the worst answer: the list looks empty and
        // the operator concludes the warehouse is empty.
        let status = StockStatus::parse("lowish");
        assert!(status.is_none());
        let rendered = StockStatus::ALL_FILTERS.join(", ");
        assert!(rendered.contains("below_threshold"));
        assert!(rendered.contains("negative"));
    }

    #[test]
    fn a_cursor_that_is_not_a_uuid_is_simply_no_cursor() {
        // Not an error: a stale bookmark in the browser should show the first page, not a 400.
        let query = StockQuery {
            cursor: Some("not-a-uuid".into()),
            ..StockQuery::default()
        };
        assert!(query.cursor_id().is_none());
    }

    #[test]
    fn an_empty_threshold_is_zero_rather_than_a_refusal() {
        // The item form leaves a threshold blank when the operator does not reorder it, and a
        // refusal on an untouched optional field is a form that cannot be submitted.
        assert!(quantity_or_zero("item", "min_threshold", "  ").unwrap().is_zero());
        assert!(quantity_or_zero("item", "min_threshold", "4").unwrap().to_text() == "4.000");
    }
}
