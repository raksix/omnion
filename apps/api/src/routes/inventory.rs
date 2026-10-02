//! `/api/v1/inventory` — items, warehouses, locations, stock and the ledger
//! (docs/requests/REQ-053, slice 1).
//!
//! The shape every module on this platform follows, written once by REQ-051 and followed here
//! rather than reinvented:
//!
//! * the caller's organization is resolved by the CRM's rule, so a screen opened on `/inventory/*`
//!   with no query string shows **its** records rather than a prompt to pick a tenant;
//! * a record of another organization is a `404`, never a `403` — a `403` would confirm it
//!   exists, and one organization's stock is the one thing this module exists to keep apart;
//! * every mutation writes an audit row with the actor, the target and the before/after, and
//!   emits the documented `inventory.*` events.
//!
//! Two rules are particular to this module and live here rather than in the screens:
//!
//! * **`inventory.negative.manage` is asked here and passed down as a boolean.** The negative
//!   stock rule is a service rule — the schema cannot know who is calling — and the HTTP layer is
//!   the only place that can answer it, so [`NewMovement::may_go_negative`] is filled from the
//!   caller's keys on every write rather than assumed.
//! * **The ledger has no `PATCH` and no `DELETE` route.** Not a route that refuses: no route. The
//!   acceptance criterion asks for a `405`, and axum answers that for a path it does not
//!   implement — which is the only answer that cannot be changed by a later handler.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use axum::response::Response;
use omnion_module_inventory::ledger::{self, Movement, MovementQuery, NewMovement, RecordOutcome};
use omnion_module_inventory::store::{
    self, ItemPatch, ItemQuery, ItemView, LocationPatch, LocationView, NewItem, NewLocation,
    NewWarehouse, Overview, Page, SettingsPatch, StockLevel, StockPosition, StockQuery,
    WarehousePatch, WarehouseView,
};
use omnion_module_inventory::{InventoryError, money::Quantity};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::crm::organization_of;
use crate::routes::iam::record;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// The `?organization_id=` every list and detail accepts, for platform accounts.
#[derive(Debug, Default, Deserialize)]
pub struct OrganizationParam {
    /// Which organization to act on; a tenant account may not name another.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The item list's query string.
#[derive(Debug, Default, Deserialize)]
pub struct ItemListParams {
    /// Free text over SKU, name and barcode.
    #[serde(default)]
    pub search: Option<String>,
    /// One category.
    #[serde(default)]
    pub category: Option<String>,
    /// `true` for live only, `false` for inactive only.
    #[serde(default)]
    pub active: Option<bool>,
    /// Include archived items.
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// `true` for items that mirror a catalog product.
    #[serde(default)]
    pub linked_to_catalog: Option<bool>,
    /// Sort key.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc`.
    #[serde(default)]
    pub direction: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The previous page's cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl From<ItemListParams> for ItemQuery {
    fn from(params: ItemListParams) -> Self {
        Self {
            search: params.search,
            category: params.category,
            active: params.active,
            include_archived: params.include_archived,
            linked_to_catalog: params.linked_to_catalog,
            sort: params.sort,
            direction: params.direction,
            limit: params.limit,
            cursor: params.cursor,
        }
    }
}

/// The stock list's query string.
#[derive(Debug, Default, Deserialize)]
pub struct StockListParams {
    /// Free text over SKU and name.
    #[serde(default)]
    pub search: Option<String>,
    /// One warehouse.
    #[serde(default)]
    pub warehouse_id: Option<Uuid>,
    /// One location.
    #[serde(default)]
    pub location_id: Option<Uuid>,
    /// One item.
    #[serde(default)]
    pub item_id: Option<Uuid>,
    /// One category.
    #[serde(default)]
    pub category: Option<String>,
    /// `ok` / `low` / `critical` / `negative` / `below_threshold`.
    #[serde(default)]
    pub status: Option<String>,
    /// Rows with no movement for this many days.
    #[serde(default)]
    pub idle_days: Option<i32>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The previous page's cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl From<StockListParams> for StockQuery {
    fn from(params: StockListParams) -> Self {
        Self {
            search: params.search,
            warehouse_id: params.warehouse_id,
            location_id: params.location_id,
            item_id: params.item_id,
            category: params.category,
            status: params.status,
            idle_days: params.idle_days,
            limit: params.limit,
            cursor: params.cursor,
        }
    }
}

/// The ledger's query string.
#[derive(Debug, Default, Deserialize)]
pub struct MovementListParams {
    /// Free text over SKU, name and note.
    #[serde(default)]
    pub search: Option<String>,
    /// One item.
    #[serde(default)]
    pub item_id: Option<Uuid>,
    /// One location.
    #[serde(default)]
    pub location_id: Option<Uuid>,
    /// One or more kinds, repeated or comma-separated.
    #[serde(default)]
    pub kind: Option<String>,
    /// One reason.
    #[serde(default)]
    pub reason: Option<String>,
    /// Who recorded it.
    #[serde(default)]
    pub actor_user_id: Option<Uuid>,
    /// A document reference — a kind (`order`), a uuid, or a number typed off a document.
    #[serde(default)]
    pub source: Option<String>,
    /// The window's start, RFC 3339.
    #[serde(default)]
    pub from: Option<String>,
    /// The window's end, RFC 3339.
    #[serde(default)]
    pub to: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The previous page's cursor (a movement id).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl MovementListParams {
    /// The kinds, split on commas so `?kind=receipt,issue` works as well as a repeated param.
    fn kinds(&self) -> Vec<String> {
        self.kind
            .as_deref()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|piece| !piece.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The module's query, with the cursor parsed as the ledger's `bigint` id.
    fn into_query(self) -> Result<MovementQuery, InventoryError> {
        let cursor = self
            .cursor
            .as_deref()
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(str::parse::<i64>)
            .transpose()
            .map_err(|_| {
                InventoryError::invalid("movement", "cursor", "the cursor is a movement id")
            })?;
        // `kinds()` is read **before** the fields are moved, because `self.kinds()` borrows
        // `self.kind` and moving `self.search` first would make that borrow a move-after-partial.
        let kinds = self.kinds();
        Ok(MovementQuery {
            search: self.search,
            item_id: self.item_id,
            location_id: self.location_id,
            kinds,
            reason: self.reason,
            actor_user_id: self.actor_user_id,
            source: self.source,
            from: self.from,
            to: self.to,
            limit: self.limit,
            cursor,
        })
    }
}

/// The barcode/sku lookup the scanner box calls.
#[derive(Debug, Deserialize)]
pub struct LookupParams {
    /// The scanned code, separators and all.
    pub code: String,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The item detail's history length.
#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    /// How many movements; the module clamps it to 500.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of a movement the caller posts.
///
/// **The kind and the permission are the caller's business, not the form's**: `kind` is optional
/// and the module infers it from the reason, and `mode` is a drawer concept that the module
/// resolves into either a delta or an absolute count before the arithmetic happens.
#[derive(Debug, Deserialize)]
pub struct RecordMovementBody {
    /// The item that moves.
    pub item_id: Uuid,
    /// The location it moves at.
    pub location_id: Uuid,
    /// What it does, when the caller knows and does not want it inferred.
    #[serde(default)]
    pub kind: Option<String>,
    /// The quantity, as text: a delta, or — with `mode: "counted"` — the counted total.
    pub quantity: String,
    /// `delta` (the default) or `counted`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Why it happened.
    #[serde(default)]
    pub reason: Option<String>,
    /// A note.
    #[serde(default)]
    pub note: Option<String>,
    /// What caused it (`order`, `transfer`, `stocktake`, `manual`).
    #[serde(default)]
    pub source_kind: Option<String>,
    /// That document's id.
    #[serde(default)]
    pub source_id: Option<Uuid>,
    /// Organization to act on.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl RecordMovementBody {
    /// Turn the drawer's two modes into the module's one.
    ///
    /// **`counted` is turned into a delta here, not in the module**, and the reason is that the
    /// current on-hand is a property of the database, not of the request: a client that sent an
    /// absolute count would be asking the server to trust the number it is about to check the
    /// server's number against. Reading the row first is also what lets the refusal name the
    /// quantity that is actually available.
    async fn into_movement(
        self,
        pool: &sqlx::PgPool,
        organization_id: Uuid,
        may_go_negative: bool,
    ) -> Result<NewMovement, InventoryError> {
        let mode = self.mode.as_deref().unwrap_or("delta");
        let quantity = match mode {
            "delta" => self.quantity,
            "counted" => {
                let current = ledger::stock_level(pool, organization_id, self.item_id, self.location_id)
                    .await?;
                let counted = store::parse_quantity("movement", "quantity", &self.quantity)?;
                let delta = counted
                    .checked_sub(current.on_hand)
                    .ok_or_else(|| {
                        InventoryError::invalid("movement", "quantity", "that number is too large")
                    })?;
                if delta.is_zero() {
                    return Err(InventoryError::invalid(
                        "movement",
                        "quantity",
                        format!(
                            "the count is already {} — there is nothing to adjust",
                            current.on_hand
                        ),
                    ));
                }
                delta.to_text()
            }
            other => {
                return Err(InventoryError::invalid(
                    "movement",
                    "mode",
                    format!("{other} is not a mode — use `delta` or `counted`"),
                ));
            }
        };
        Ok(NewMovement {
            item_id: self.item_id,
            location_id: self.location_id,
            kind: self.kind,
            quantity,
            reason: self.reason,
            note: self.note,
            source_kind: self.source_kind,
            source_id: self.source_id,
            may_go_negative,
        })
    }
}

/// The body of the "what would this do?" preview the drawer calls before it commits.
#[derive(Debug, Deserialize)]
pub struct PreviewBody {
    /// The item.
    pub item_id: Uuid,
    /// The location.
    pub location_id: Uuid,
    /// The quantity, as text.
    pub quantity: String,
    /// The mode, as in [`RecordMovementBody`].
    #[serde(default)]
    pub mode: Option<String>,
    /// The reason, which decides the direction when no kind is named.
    #[serde(default)]
    pub reason: Option<String>,
    /// The kind, when the caller names one.
    #[serde(default)]
    pub kind: Option<String>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// What the preview answers.
#[derive(Debug, serde::Serialize)]
pub struct Preview {
    /// The quantity before.
    pub on_hand_before: String,
    /// The quantity the row would hold.
    pub on_hand_after: String,
    /// How much of it is held for orders, before and after.
    pub reserved_before: String,
    /// The reserved total after.
    pub reserved_after: String,
    /// What a person may draw after.
    pub available_after: String,
    /// The kind the module inferred, so the drawer can print it.
    pub kind: String,
    /// The reason that was applied.
    pub reason: String,
    /// The badge the row would wear.
    pub status: String,
}

// ---------------------------------------------------------------------------------------------
// Audit and events
// ---------------------------------------------------------------------------------------------

/// Record an event without letting a webhook problem fail the caller's request.
///
/// A stock movement that happened but whose event was lost is a stock movement that a REQ-003
/// automation never saw; refusing the *request* would be far worse, because the ledger row would
/// roll back with it and the person would retry a movement that had in fact not happened.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the inventory event could not be recorded");
    }
}

/// Which item fields a write actually changed.
///
/// Typed rather than "serialise both rows and diff the JSON": a diff over two serialised views
/// would also report the `updated_at` this same write moved, and an audit row that says a row
/// changed when nothing the person can see did is an audit row nobody trusts.
fn item_changes(before: &ItemView, after: &ItemView) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    let (before_item, after_item) = (&before.item, &after.item);
    if before_item.name != after_item.name {
        changed.push("name".to_owned());
    }
    if before_item.category != after_item.category {
        changed.push("category".to_owned());
    }
    if before_item.unit != after_item.unit {
        changed.push("unit".to_owned());
    }
    if before_item.barcode != after_item.barcode {
        changed.push("barcode".to_owned());
    }
    if before_item.min_threshold != after_item.min_threshold {
        changed.push("min_threshold".to_owned());
    }
    if before_item.reorder_point != after_item.reorder_point {
        changed.push("reorder_point".to_owned());
    }
    if before_item.reorder_qty != after_item.reorder_qty {
        changed.push("reorder_qty".to_owned());
    }
    if before_item.cost != after_item.cost {
        changed.push("cost".to_owned());
    }
    if before_item.product_id != after_item.product_id {
        changed.push("product_id".to_owned());
    }
    if before_item.notes != after_item.notes {
        changed.push("notes".to_owned());
    }
    if before_item.active != after_item.active {
        changed.push("active".to_owned());
    }
    changed
}

// ---------------------------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/inventory` — the overview's figures.
pub async fn overview(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<Overview>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        store::overview(state.db().pool(), organization_id).await?,
    ))
}

/// `GET /api/v1/inventory/vocabulary` — the words the item form and the filters offer.
///
/// Categories, units and the reason codes in one call, because the item form draws three selects
/// and a screen that fetches them one at a time is a form that appears empty three times.
#[derive(Debug, serde::Serialize)]
pub struct Vocabulary {
    /// The categories the organization has written.
    pub categories: Vec<String>,
    /// The units the organization has written.
    pub units: Vec<String>,
    /// The reason codes, with the two negatives flagged so the drawer can say which one needs a
    /// permission.
    pub reasons: Vec<Value>,
    /// The location kinds the location editor offers.
    pub location_kinds: Vec<String>,
}

/// `GET /api/v1/inventory/vocabulary` — categories, units, reasons and location kinds.
pub async fn vocabulary(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<Vocabulary>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let categories = store::list_categories(pool, organization_id).await?;
    let mut units = store::list_units(pool, organization_id).await?;
    // The organization's default is offered even before anybody has written an item, because the
    // first item of a new tenant is exactly the case where the list is empty.
    let default_unit = store::default_unit(pool, organization_id).await?;
    if !units.iter().any(|unit| unit == &default_unit) {
        units.insert(0, default_unit);
    }
    let reasons = omnion_module_inventory::model::ReasonCode::ALL
        .iter()
        .map(|reason| {
            json!({
                "value": reason.as_str(),
                "may_go_negative": reason.may_go_negative(),
            })
        })
        .collect();
    Ok(Json(Vocabulary {
        categories,
        units,
        reasons,
        location_kinds: omnion_module_inventory::model::LocationKind::ALL
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/inventory/items` — one page of items.
pub async fn list_items(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ItemListParams>,
) -> Result<Json<Page<ItemView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = ItemQuery::from(params);
    Ok(Json(
        store::list_items(state.db().pool(), organization_id, &query).await?,
    ))
}

/// `GET /api/v1/inventory/items/{id}` — one item, with its stock at every location.
///
/// The detail is **one call**, not "the item, then its positions": a screen that has to join two
/// responses can draw a header from one and a table from the other that disagree about the same
/// item, and the stock list is a rollup of the same numbers either way.
///
/// **`history_limit` is honoured, and it used to be ignored.** The client has always sent it
/// (`fetchItem(id, historyLimit = 25)`), and the route took no query parameter at all and asked
/// for 200 rows. So the detail screen always rendered the same 200 movements whatever it asked
/// for, and a caller asking for 25 to keep a phone's first paint small got 200. A parameter a
/// server ignores is worse than one it refuses: it is a claim the API is making that it is not
/// keeping. The bound is clamped rather than trusted — the ledger table is the one place in this
/// module where an unbounded read is a page that never finishes.
pub async fn get_item(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Query(history): Query<ItemDetailParams>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<ItemDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let position = store::item_position(pool, organization_id, item_id).await?;
    let history = ledger::item_history(pool, organization_id, item_id, history.limit()).await?;
    Ok(Json(ItemDetail { position, history }))
}

/// The item detail's own parameters. Separate from [`ItemListParams`] so `limit` can mean "how
/// many movements" here and "page size" there — one struct reused for both would answer a
/// question with the wrong number.
#[derive(Debug, serde::Deserialize)]
pub struct ItemDetailParams {
    /// How many movements to return.
    #[serde(default)]
    pub history_limit: Option<i64>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl ItemDetailParams {
    /// The clamped history limit: a missing value is the screen's own default, and anything
    /// outside the range is pulled back to it rather than refused, because a caller asking for
    /// ten thousand rows has not made a mistake worth a 422.
    const DEFAULT_LIMIT: i64 = 25;
    const MAX_LIMIT: i64 = 200;

    fn limit(&self) -> i64 {
        match self.history_limit {
            Some(asked) if asked > 0 => asked.min(Self::MAX_LIMIT),
            Some(_) => Self::DEFAULT_LIMIT,
            None => Self::DEFAULT_LIMIT,
        }
    }
}

/// The item detail: the position plus the movement history the screen's second tab draws.
///
/// **Not `#[serde(flatten)]`ed**, unlike the item inside the position. A flatten serializes as a
/// map, and every field of `StockPosition` would land at the top level beside `history` — which
/// reads well until two of them collide and one silently wins. A named `position` key is one line
/// longer in the JSON and cannot collide.
#[derive(Debug, serde::Serialize)]
pub struct ItemDetail {
    /// The item, its per-location rows and the totals.
    pub position: StockPosition,
    /// Its movements, newest first.
    pub history: Vec<Movement>,
}

/// `POST /api/v1/inventory/items` — create an item.
pub async fn create_item(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewItem>,
) -> Result<(StatusCode, Json<ItemView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let created = store::create_item(state.db().pool(), organization_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.item.created")
            .organization(organization_id)
            .target("inventory_item", created.id.to_string())
            .metadata(json!({ "request_id": created.id, "after": created.reference() }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.item.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(created.reference()),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /api/v1/inventory/items/{id}` — update an item.
pub async fn update_item(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(item_id): Path<Uuid>,
    body: Json<ItemPatch>,
) -> Result<Json<ItemView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_item(pool, organization_id, item_id).await?;
    let after = store::patch_item(pool, organization_id, item_id, &body.0).await?;

    let changed = item_changes(&before, &after);
    if !changed.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "inventory.item.updated")
                .organization(organization_id)
                .target("inventory_item", after.id.to_string())
                .metadata(json!({
                    "request_id": after.id,
                    "changed": changed,
                    "before": before.reference(),
                    "after": after.reference(),
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("inventory.item.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({ "item_id": after.id, "changed": changed })),
        )
        .await;
    }

    Ok(Json(after))
}

/// `DELETE /api/v1/inventory/items/{id}` — archive an item.
pub async fn archive_item(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<ItemView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_item(pool, organization_id, item_id).await?;
    let after = store::archive_item(pool, organization_id, item_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.item.archived")
            .organization(organization_id)
            .target("inventory_item", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "before": before.reference(),
                "after": after.reference(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.item.archived")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(after.reference()),
    )
    .await;

    Ok(Json(after))
}

/// `GET /api/v1/inventory/items/lookup?code=…` — the scanner box's one call.
///
/// A miss is a `404` and not an empty list: a scanner that resolves nothing has to say so, and a
/// `200` with zero rows is a response a scanner box renders as "found an item called nothing".
pub async fn lookup_item(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<LookupParams>,
) -> Result<Json<ItemView>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let found = store::lookup_item(state.db().pool(), organization_id, &params.code)
        .await?
        .ok_or(ApiError::new(
            StatusCode::NOT_FOUND,
            "inventory_item_not_found",
            "no item carries that barcode or SKU",
        ))?;
    Ok(Json(found))
}

// ---------------------------------------------------------------------------------------------
// Stock
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/inventory/stock` — one page of the stock list.
pub async fn list_stock(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<StockListParams>,
) -> Result<Json<Page<StockLevel>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = StockQuery::from(params);
    Ok(Json(
        store::list_stock(state.db().pool(), organization_id, &query).await?,
    ))
}

/// `GET /api/v1/inventory/reconciliation` — the rollup against a replay of the ledger.
///
/// **A list of disagreements, not a count.** The endpoint exists so somebody can go and look at a
/// row, and a number cannot be looked at.
pub async fn reconciliation(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<store::StocktakeSnapshot>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        store::reconciliation_report(state.db().pool(), organization_id).await?,
    ))
}

// ---------------------------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/inventory/movements` — one page of the ledger, newest first.
pub async fn list_movements(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<MovementListParams>,
) -> Result<Json<Page<Movement>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = params.into_query()?;
    Ok(Json(
        ledger::list_movements(state.db().pool(), organization_id, &query).await?,
    ))
}

/// `GET /api/v1/inventory/movements/{id}` — one movement, with the numbers it produced.
pub async fn get_movement(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(movement_id): Path<i64>,
) -> Result<Json<Movement>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let movement = ledger::get_movement(state.db().pool(), movement_id).await?;
    // A movement of another organization is a 404, checked here rather than in the module: the
    // ledger's id is a global sequence, so `get_movement` alone would happily return a row from a
    // competitor and the id is guessable by counting.
    if movement.organization_id != organization_id {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "inventory_movement_not_found",
            "no such movement in this organization",
        ));
    }
    Ok(Json(movement))
}

/// `POST /api/v1/inventory/movements` — record a movement.
///
/// The audit row carries the **before and after quantities**, not just the movement: the question
/// an auditor asks about a stock adjustment is "what was there before and what is there now",
/// and an audit row that only names the delta makes them reconstruct it from the ledger.
pub async fn record_movement(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<RecordMovementBody>,
) -> Result<(StatusCode, Json<RecordOutcome>), ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id.or(organization.organization_id)).await?;
    let pool = state.db().pool();

    // The negative-stock permission, asked once here and passed down as a boolean. This is the
    // only place in the stack that can answer it: the module has no session, and the schema
    // cannot ask.
    let may_go_negative = holds_permission(&state, &current, organization_id, "inventory.negative.manage")
        .await?;

    // **The threshold check, before the write and before anything is written about it.** The
    // comparison is the module's (`approvals::needs_approval`), not a second `>` in this file:
    // the drawer asks the same function through the preview, the save asks it here, and the two
    // answers have to be the same or a screen will say "no approval needed" and then refuse.
    //
    // Three outcomes, and only one of them writes:
    //
    // * under the threshold, or the caller may approve — write the movement (the old behaviour);
    // * over the threshold and the caller may not approve — **raise a request and return 202**
    //   with it, so the drawer shows "waiting on a decision" rather than a success;
    // * over the threshold and a request is already open for this item at this location — the
    //   module's own refusal, which carries the pending amount.
    let body = body.0;
    let settings = store::get_settings(pool, organization_id).await?;
    let may_approve = holds_permission(
        &state,
        &current,
        organization_id,
        "inventory.adjustment.approve",
    )
    .await?;

    // The amount is measured on the number as typed and against the current on-hand, which is
    // why a `counted` request is turned into its distance *before* the comparison. Measuring a
    // counted total against the threshold would ask for approval of a recount of 6 when the
    // shelf holds 10, which is a refusal of a correction the operator already made.
    let level = ledger::stock_level(pool, organization_id, body.item_id, body.location_id).await?;
    let mode = body.mode.clone().unwrap_or_else(|| "delta".to_owned());
    let typed = store::parse_quantity("movement", "quantity", &body.quantity)?;
    let amount = omnion_module_inventory::approvals::approval_amount(&mode, typed, level.on_hand);

    if omnion_module_inventory::approvals::needs_approval(amount, &settings) && !may_approve {
        let reason = omnion_module_inventory::items::default_reason(body.reason.as_deref())?;
        let kind = preview_kind(body.kind.as_deref(), reason, typed)?;
        let request = omnion_module_inventory::approvals::NewApproval {
            item_id: body.item_id,
            location_id: body.location_id,
            kind: kind.as_str().to_owned(),
            mode,
            quantity: typed,
            reason: reason.as_str().to_owned(),
            note: body.note.clone().unwrap_or_default(),
            source_kind: body.source_kind.clone(),
            source_id: body.source_id,
        };
        let view = omnion_module_inventory::approvals::request_approval(
            pool,
            organization_id,
            &request,
            current.user.id,
        )
        .await?;
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "inventory.adjustment.requested")
                .organization(organization_id)
                .target("inventory_adjustment_approval", view.id.to_string())
                .metadata(json!({
                    "approval_id": view.id,
                    "item_id": view.item_id,
                    "location_id": view.location_id,
                    "amount": view.amount.to_text(),
                    "threshold": view.threshold.to_text(),
                }))
                .ip_address(address.as_text()),
        )
        .await?;
        emit(
            &state,
            NewEvent::new("inventory.adjustment.requested")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "approval_id": view.id,
                    "sku": view.sku,
                    "amount": view.amount.to_text(),
                    "threshold": view.threshold.to_text(),
                })),
        )
        .await;
        // `202 Accepted`, not `201 Created`: the request was created, and the thing the caller
        // asked for — a movement — has not. A `201` here would tell the drawer it worked.
        return Ok((StatusCode::ACCEPTED, Json(RecordOutcome::AwaitingApproval { approval: view })));
    }

    let before = level;
    let new = body.into_movement(pool, organization_id, may_go_negative).await?;
    let recorded = ledger::record_movement(pool, organization_id, &new, Some(current.user.id)).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.movement.recorded")
            .organization(organization_id)
            .target("inventory_movement", recorded.movement.id.to_string())
            .metadata(json!({
                "movement_id": recorded.movement.id,
                "item_id": recorded.movement.item_id,
                "location_id": recorded.movement.location_id,
                "kind": recorded.movement.kind.as_str(),
                "reason": recorded.movement.reason.as_str(),
                "quantity": recorded.movement.quantity.to_text(),
                "on_hand_before": before.on_hand.to_text(),
                "on_hand_after": recorded.movement.on_hand_after.to_text(),
                "reserved_before": before.reserved.to_text(),
                "reserved_after": recorded.movement.reserved_after.to_text(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.movement.recorded")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(recorded.movement.reference()),
    )
    .await;

    // A crossing of the reorder point is its own event, because it is the one a REQ-003
    // automation subscribes to ("when stock runs low, raise a purchase request"). It is emitted
    // from the transition, not from the badge, so a movement that does not cross anything is
    // silent — the rule is edge-triggered, exactly as the spec demands, or a busy warehouse
    // floods the automation log.
    if crossed_downward(&before, &recorded.position) {
        emit(
            &state,
            NewEvent::new("inventory.stock.low")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "item_id": recorded.movement.item_id,
                    "sku": recorded.movement.sku,
                    "location_id": recorded.movement.location_id,
                    "available": recorded.position.available.to_text(),
                    "reorder_point": recorded.position.reorder_point.to_text(),
                })),
        )
        .await;
    }

    Ok((
        StatusCode::CREATED,
        Json(RecordOutcome::Recorded {
            movement: recorded.movement,
            position: recorded.position,
        }),
    ))
}

/// Whether this write took the row from above its reorder point to at or below it.
fn crossed_downward(before: &StockLevel, after: &StockLevel) -> bool {
    before.available.milli() > before.reorder_point.milli()
        && after.available.milli() <= after.reorder_point.milli()
}

/// `POST /api/v1/inventory/movements/preview` — what this adjustment would do, before it happens.
///
/// The drawer calls it on every keystroke so the operator sees the resulting quantity rather than
/// finding out after a round trip. It is a **read** of the current row plus the module's own
/// arithmetic, so the number previewed and the number written come from one implementation: a
/// second implementation in the browser is the one that drifts.
pub async fn preview_movement(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    body: Json<PreviewBody>,
) -> Result<Json<Preview>, ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id.or(organization.organization_id)).await?;
    let pool = state.db().pool();
    let current_level = ledger::stock_level(pool, organization_id, body.0.item_id, body.0.location_id).await?;
    let may_go_negative = holds_permission(&state, &current, organization_id, "inventory.negative.manage")
        .await?;

    let typed = store::parse_quantity("movement", "quantity", &body.0.quantity)?;
    let quantity = match body.0.mode.as_deref().unwrap_or("delta") {
        "delta" => typed,
        "counted" => typed
            .checked_sub(current_level.on_hand)
            .ok_or_else(|| {
                ApiError::bad_request("invalid_inventory_movement", "that number is too large")
            })?,
        other => {
            return Err(ApiError::bad_request(
                "invalid_inventory_movement",
                format!("{other} is not a mode — use `delta` or `counted`"),
            ));
        }
    };
    let reason = omnion_module_inventory::items::default_reason(body.0.reason.as_deref())?;
    let kind = preview_kind(body.0.kind.as_deref(), reason, quantity)?;

    // The preview answers even when the write would be refused, because **the refusal is the
    // preview**: "this would leave −3, here is what is available" is the sentence the drawer
    // needs more than a 409 three fields later.
    let (on_hand_after, reserved_after) =
        match omnion_module_inventory::apply_movement(
            current_level.on_hand,
            current_level.reserved,
            kind,
            quantity,
            reason,
            may_go_negative,
        ) {
            Ok(after) => after,
            Err(error) => {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "inventory_movement_refused",
                    error.to_string(),
                ));
            }
        };
    let available_after = on_hand_after.checked_sub(reserved_after).unwrap_or(Quantity::ZERO);
    let status = omnion_module_inventory::StockStatus::of(
        available_after,
        current_level.min_threshold,
        current_level.reorder_point,
    );

    Ok(Json(Preview {
        on_hand_before: current_level.on_hand.to_text(),
        on_hand_after: on_hand_after.to_text(),
        reserved_before: current_level.reserved.to_text(),
        reserved_after: reserved_after.to_text(),
        available_after: available_after.to_text(),
        kind: kind.as_str().to_owned(),
        reason: reason.as_str().to_owned(),
        status: status.as_str().to_owned(),
    }))
}

/// The kind a preview would apply — the same inference the module makes, exposed so the drawer
/// can print the kind it is about to write.
fn preview_kind(
    named: Option<&str>,
    reason: omnion_module_inventory::ReasonCode,
    quantity: Quantity,
) -> Result<omnion_module_inventory::MovementKind, ApiError> {
    // Built by asking the module's own rule through a throwaway `NewMovement`, so the preview and
    // the write cannot pick different kinds. A named kind short-circuits.
    if let Some(raw) = named.map(str::trim).filter(|raw| !raw.is_empty()) {
        let kind = omnion_module_inventory::MovementKind::parse(raw).ok_or_else(|| {
            ApiError::bad_request(
                "invalid_inventory_movement",
                format!("{raw} is not a movement kind"),
            )
        })?;
        if !kind.is_recordable_by_hand() {
            return Err(ApiError::bad_request(
                "invalid_inventory_movement",
                "a reservation is made by confirming an order, not by recording a movement by hand",
            ));
        }
        return Ok(kind);
    }
    Ok(match reason {
        omnion_module_inventory::ReasonCode::PurchaseReceipt
        | omnion_module_inventory::ReasonCode::CustomerReturn => {
            omnion_module_inventory::MovementKind::Receipt
        }
        omnion_module_inventory::ReasonCode::SaleShipment
        | omnion_module_inventory::ReasonCode::SupplierReturn => {
            omnion_module_inventory::MovementKind::Issue
        }
        omnion_module_inventory::ReasonCode::Transfer => {
            if quantity.is_negative() {
                omnion_module_inventory::MovementKind::TransferOut
            } else {
                omnion_module_inventory::MovementKind::TransferIn
            }
        }
        _ => omnion_module_inventory::MovementKind::Adjustment,
    })
}

// ---------------------------------------------------------------------------------------------
// Warehouses and locations
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/inventory/warehouses` — the tree, with each node's totals.
pub async fn list_warehouses(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<Vec<WarehouseView>>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        store::list_warehouses(state.db().pool(), organization_id).await?,
    ))
}

/// `POST /api/v1/inventory/warehouses` — create a warehouse.
pub async fn create_warehouse(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewWarehouse>,
) -> Result<(StatusCode, Json<WarehouseView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let created = store::create_warehouse(state.db().pool(), organization_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.warehouse.created")
            .organization(organization_id)
            .target("inventory_warehouse", created.id.to_string())
            .metadata(json!({ "request_id": created.id, "after": created.reference() }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.location.changed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(created.reference()),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /api/v1/inventory/warehouses/{id}` — rename or deactivate a warehouse.
pub async fn update_warehouse(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(warehouse_id): Path<Uuid>,
    body: Json<WarehousePatch>,
) -> Result<Json<WarehouseView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_warehouse(pool, organization_id, warehouse_id).await?;
    let after = store::patch_warehouse(pool, organization_id, warehouse_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.warehouse.updated")
            .organization(organization_id)
            .target("inventory_warehouse", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "before": before.reference(),
                "after": after.reference(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.location.changed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(after.reference()),
    )
    .await;

    Ok(Json(after))
}

/// `GET /api/v1/inventory/locations` — every location, ordered by warehouse then code.
pub async fn list_locations(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<Vec<LocationView>>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        store::list_locations(state.db().pool(), organization_id).await?,
    ))
}

/// `POST /api/v1/inventory/locations` — create a location under a warehouse.
pub async fn create_location(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewLocation>,
) -> Result<(StatusCode, Json<LocationView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let created = store::create_location(state.db().pool(), organization_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.location.created")
            .organization(organization_id)
            .target("inventory_location", created.id.to_string())
            .metadata(json!({ "request_id": created.id, "after": created.reference() }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.location.changed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(created.reference()),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /api/v1/inventory/locations/{id}` — rename, re-kind or deactivate a location.
pub async fn update_location(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(location_id): Path<Uuid>,
    body: Json<LocationPatch>,
) -> Result<Json<LocationView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_location(pool, organization_id, location_id).await?;
    let after = store::patch_location(pool, organization_id, location_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.location.updated")
            .organization(organization_id)
            .target("inventory_location", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "before": before.reference(),
                "after": after.reference(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.location.changed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(after.reference()),
    )
    .await;

    Ok(Json(after))
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/inventory/settings` — the thresholds the drawer reads before it writes.
pub async fn get_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<omnion_module_inventory::Settings>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        store::get_settings(state.db().pool(), organization_id).await?,
    ))
}

/// `PUT /api/v1/inventory/settings` — write the thresholds.
pub async fn update_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<SettingsPatch>,
) -> Result<Json<omnion_module_inventory::Settings>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_settings(pool, organization_id).await?;
    let after = store::update_settings(pool, organization_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.settings.updated")
            .organization(organization_id)
            .target("inventory_settings", organization_id.to_string())
            .metadata(json!({ "before": before, "after": after }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(after))
}

// ---------------------------------------------------------------------------------------------
// Adjustment approvals (slice 2)
// ---------------------------------------------------------------------------------------------

/// The body of a decision.
#[derive(Debug, Deserialize)]
pub struct ApprovalDecisionBody {
    /// `approve` or `reject`.
    pub decision: String,
    /// Required on a rejection.
    #[serde(default)]
    pub comment: Option<String>,
}

/// The body of a raise.
#[derive(Debug, Deserialize)]
pub struct RaiseApprovalBody {
    /// The item.
    pub item_id: Uuid,
    /// The location.
    pub location_id: Uuid,
    /// The kind, when the caller knows it.
    #[serde(default)]
    pub kind: Option<String>,
    /// The number as typed: a delta, or with `mode: "counted"` the counted total.
    pub quantity: String,
    /// `delta` or `counted` — the same two modes the drawer offers.
    #[serde(default)]
    pub mode: Option<String>,
    /// Why.
    #[serde(default)]
    pub reason: Option<String>,
    /// A note for the approver.
    #[serde(default)]
    pub note: Option<String>,
    /// Where the write came from.
    #[serde(default)]
    pub source_kind: Option<String>,
    /// That document's id.
    #[serde(default)]
    pub source_id: Option<Uuid>,
    /// Organization to act on.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/inventory/approvals` — the decision inbox.
pub async fn list_approvals(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ApprovalListParams>,
) -> Result<Json<omnion_module_inventory::store::Page<omnion_module_inventory::approvals::ApprovalView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = omnion_module_inventory::approvals::ApprovalQuery {
        status: params.status,
        item_id: params.item_id,
        limit: params.limit.unwrap_or(50),
        cursor: params.cursor,
    };
    Ok(Json(
        omnion_module_inventory::approvals::list_approvals(state.db().pool(), organization_id, &query)
            .await?,
    ))
}

/// The inbox's filter.
#[derive(Debug, Deserialize)]
pub struct ApprovalListParams {
    /// `pending`, `approved`, `rejected` or `cancelled`.
    #[serde(default)]
    pub status: Option<String>,
    /// One item's history.
    #[serde(default)]
    pub item_id: Option<Uuid>,
    /// How many rows.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The page cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/inventory/approvals/{id}` — one request.
pub async fn get_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(approval_id): Path<Uuid>,
) -> Result<Json<omnion_module_inventory::approvals::ApprovalView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let view = omnion_module_inventory::approvals::get_approval(
        state.db().pool(),
        organization_id,
        approval_id,
    )
    .await?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "inventory_approval_not_found",
            "no such adjustment request in this organization",
        )
    })?;
    Ok(Json(view))
}

/// `POST /api/v1/inventory/approvals` — raise a request for an over-threshold adjustment.
///
/// **The route that makes slice 1's threshold real.** The save path checks the threshold first
/// and calls this instead of writing, so the drawer never has to decide whether a number is big
/// enough — that comparison lives in one place and this route is what it calls.
pub async fn raise_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<RaiseApprovalBody>,
) -> Result<(StatusCode, Json<omnion_module_inventory::approvals::ApprovalView>), ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id).await?;
    let pool = state.db().pool();
    let body = body.0;

    let quantity = store::parse_quantity("movement", "quantity", &body.quantity)?;
    let reason = omnion_module_inventory::items::default_reason(body.reason.as_deref())?;
    // Read before the arithmetic so a foreign id is a 404 rather than a request carrying a
    // quantity measured against a row that does not exist.
    ledger::stock_level(pool, organization_id, body.item_id, body.location_id).await?;
    let kind = preview_kind(body.kind.as_deref(), reason, quantity)?;

    let request = omnion_module_inventory::approvals::NewApproval {
        item_id: body.item_id,
        location_id: body.location_id,
        kind: kind.as_str().to_owned(),
        mode: body.mode.clone().unwrap_or_else(|| "delta".to_owned()),
        quantity,
        reason: reason.as_str().to_owned(),
        note: body.note.unwrap_or_default(),
        source_kind: body.source_kind,
        source_id: body.source_id,
    };
    let view = omnion_module_inventory::approvals::request_approval(
        pool,
        organization_id,
        &request,
        current.user.id,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.adjustment.requested")
            .organization(organization_id)
            .target("inventory_adjustment_approval", view.id.to_string())
            .metadata(json!({
                "approval_id": view.id,
                "item_id": view.item_id,
                "location_id": view.location_id,
                "amount": view.amount.to_text(),
                "threshold": view.threshold.to_text(),
                "reason": view.reason,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.adjustment.requested")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "approval_id": view.id,
                "sku": view.sku,
                "item_id": view.item_id,
                "location_id": view.location_id,
                "amount": view.amount.to_text(),
                "threshold": view.threshold.to_text(),
            })),
    )
    .await;

    Ok((StatusCode::CREATED, Json(view)))
}

/// `POST /api/v1/inventory/approvals/{id}/decision` — approve or reject.
///
/// The approve branch returns the **movement it produced** alongside the request, so a screen
/// can show the new balance without a second round trip — and so a caller can tell the
/// difference between "approved" and "approved, and here is what it did".
pub async fn decide_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(approval_id): Path<Uuid>,
    body: Json<ApprovalDecisionBody>,
) -> Result<Json<ApprovalDecisionOutcome>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    // The approver's own negative-stock permission, not the requester's: the control exists so a
    // decision is made by somebody in a position to make it, and a person without the key
    // approving a correction into negative stock is exactly the case it should stop.
    let may_go_negative =
        holds_permission(&state, &current, organization_id, "inventory.negative.manage").await?;

    let (view, recorded) = omnion_module_inventory::approvals::decide(
        pool,
        organization_id,
        approval_id,
        current.user.id,
        &body.0.decision,
        body.0.comment.as_deref(),
        may_go_negative,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.adjustment.decided")
            .organization(organization_id)
            .target("inventory_adjustment_approval", view.id.to_string())
            .metadata(json!({
                "approval_id": view.id,
                "decision": view.decision,
                "comment": view.comment,
                "amount": view.amount.to_text(),
                "threshold": view.threshold.to_text(),
                "movement_id": view.movement_id,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    if let Some(recorded) = &recorded {
        emit(
            &state,
            NewEvent::new("inventory.movement.recorded")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(recorded.movement.reference()),
        )
        .await;
    }

    Ok(Json(ApprovalDecisionOutcome {
        approval: view,
        movement: recorded.map(|entry| entry.movement),
    }))
}

/// What a decision produced: the request, and the movement an approval wrote.
///
/// Only those two. The new balance is deliberately **not** echoed here: the stock row is read
/// once, by the write path, and a second copy of it in a response is a second thing that can be
/// stale. The screen re-reads `/inventory/stock`, which is the screen that owns the balance.
#[derive(Debug, serde::Serialize)]
pub struct ApprovalDecisionOutcome {
    /// The request, now decided.
    pub approval: omnion_module_inventory::approvals::ApprovalView,
    /// The movement the approval wrote, when it was approved.
    pub movement: Option<omnion_module_inventory::Movement>,
}

/// `POST /api/v1/inventory/approvals/{id}/cancel` — the requester withdraws.
pub async fn cancel_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(approval_id): Path<Uuid>,
) -> Result<Json<omnion_module_inventory::approvals::ApprovalView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let view = omnion_module_inventory::approvals::cancel(
        state.db().pool(),
        organization_id,
        approval_id,
        current.user.id,
    )
    .await?;
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.adjustment.cancelled")
            .organization(organization_id)
            .target("inventory_adjustment_approval", view.id.to_string())
            .metadata(json!({ "approval_id": view.id }))
            .ip_address(address.as_text()),
    )
    .await?;
    Ok(Json(view))
}

/// `GET /api/v1/inventory/approvals/pending-count` — the nav badge.
pub async fn pending_approval_count(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<PendingCount>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(PendingCount {
        pending: omnion_module_inventory::approvals::pending_count(
            state.db().pool(),
            organization_id,
        )
        .await?,
    }))
}

/// How many requests are waiting.
#[derive(Debug, serde::Serialize)]
pub struct PendingCount {
    /// The count.
    pub pending: i64,
}

// ---------------------------------------------------------------------------------------------
// CSV export
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/inventory/stock/export` — the stock list as a CSV.
///
/// The body is produced from **the same list the table rendered**: the route re-runs the screen's
/// filter with the screen's limit and hands the resulting page to the writer. A second query with
/// its own filter list would be a second answer, and the export is the file people paste into a
/// spreadsheet — it has to be the rows they were looking at.
pub async fn export_stock(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<StockListParams>,
) -> Result<Response, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    // **The same `StockQuery::from` the list route builds.** That is the whole argument for the
    // export: one filter type, one builder, two callers. A hand-rolled filter list here would be
    // a second answer to "which rows is the operator looking at".
    let query = StockQuery::from(params);
    let page = store::list_stock(state.db().pool(), organization_id, &query).await?;
    csv_response(omnion_module_inventory::csv::stock_csv(&page), "stock")
}

/// `GET /api/v1/inventory/movements/export` — the ledger as a CSV.
pub async fn export_movements(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<MovementListParams>,
) -> Result<Response, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    // The same builder the ledger screen used, for the same reason as the stock export above.
    let query = params.into_query()?;
    let page = ledger::list_movements(state.db().pool(), organization_id, &query).await?;
    csv_response(omnion_module_inventory::csv::movements_csv(&page), "movements")
}

/// `GET /api/v1/inventory/approvals/export` — the decision list as a CSV.
pub async fn export_approvals(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ApprovalListParams>,
) -> Result<Response, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = omnion_module_inventory::approvals::ApprovalQuery {
        status: params.status,
        item_id: params.item_id,
        limit: params.limit.unwrap_or(1_000),
        cursor: params.cursor,
    };
    let page = omnion_module_inventory::approvals::list_approvals(
        state.db().pool(),
        organization_id,
        &query,
    )
    .await?;
    csv_response(omnion_module_inventory::csv::approvals_csv(&page), "adjustments")
}

/// Wrap a CSV in the response the browser downloads, with a dated filename.
///
/// `Content-Disposition: attachment` rather than an inline body: an inline CSV opens in the
/// browser's text view, and the operator's next click is "save as" on a page that looks like
/// the platform. The date is in the name because a file called `stock.csv` is the one from last
/// Tuesday by Friday.
/// The reports screen's query string.
///
/// Every field is optional and every default is the module's, so `GET /reports` with
/// no query string is a real answer rather than an error page: the last 30 days, every
/// warehouse, 30-day idle. A report that demands a period before it will show one is
/// a report nobody opens on a Monday.
#[derive(Debug, Default, Deserialize)]
pub struct ReportParams {
    /// First day of the window, inclusive, as `YYYY-MM-DD`.
    #[serde(default)]
    pub from: Option<String>,
    /// Last day of the window, inclusive, as `YYYY-MM-DD`.
    #[serde(default)]
    pub to: Option<String>,
    /// One warehouse, to narrow every block.
    #[serde(default)]
    pub warehouse_id: Option<Uuid>,
    /// One category, to narrow every block.
    #[serde(default)]
    pub category: Option<String>,
    /// The days of silence that make a row idle. Defaults to 30.
    #[serde(default)]
    pub idle_days: Option<i32>,
    /// Rows in the idle block.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl From<ReportParams> for omnion_module_inventory::reports::ReportQuery {
    fn from(params: ReportParams) -> Self {
        Self {
            from: params.from,
            to: params.to,
            warehouse_id: params.warehouse_id,
            category: params.category,
            idle_days: params.idle_days,
            limit: params.limit,
        }
    }
}

/// The global search's query string.
#[derive(Debug, Default, Deserialize)]
pub struct SearchParams {
    /// The term. Required, and an empty one is refused rather than matching everything.
    #[serde(default)]
    pub q: Option<String>,
    /// Rows to return.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/inventory/reports` — value-lite, the period's movements and idle stock.
///
/// **No permission key of its own**: the three blocks are all reads of stock this
/// organization owns, so they sit under `inventory.items.read` with the stock list.
/// Inventing `inventory.reports.read` would give an operator a key to grant that
/// decides nothing — a role could hold it and still see nothing, which is the
/// confusingest possible permission.
pub async fn reports(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ReportParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = omnion_module_inventory::reports::ReportQuery::from(params);
    let report =
        omnion_module_inventory::reports::build_report(state.db().pool(), organization_id, &query)
            .await?;
    Ok(Json(serde_json::to_value(report).unwrap_or_else(|_| json!({}))))
}

/// `GET /api/v1/inventory/reports/export` — the same report as a CSV.
///
/// The file is produced from **the report the screen rendered**, not from a second
/// query: the route builds the report once and hands it to the writer, so the numbers
/// in the file and the numbers on the page cannot disagree. A report whose CSV is its
/// own query is a file that is trusted, and a trusted file that disagrees is worse than
/// no file.
pub async fn export_report(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ReportParams>,
) -> Result<Response, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = omnion_module_inventory::reports::ReportQuery::from(params);
    let report =
        omnion_module_inventory::reports::build_report(state.db().pool(), organization_id, &query)
            .await?;
    csv_response(omnion_module_inventory::reports::report_csv(&report), "report")
}

/// `GET /api/v1/inventory/search` — items by SKU, barcode or name.
///
/// One statement over the items and the stock, so the ⌘K path pays one round trip and
/// the two surfaces are ranked by one expression. A barcode is compared with its
/// separators stripped and its case folded, exactly as `items/lookup` does, because a
/// scanner and a search box describing one label two ways is the bug that walk caught.
pub async fn global_search(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<SearchParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let term = params.q.unwrap_or_default();
    let results = omnion_module_inventory::reports::global_search(
        state.db().pool(),
        organization_id,
        &term,
        params.limit.unwrap_or(20),
    )
    .await?;
    Ok(Json(serde_json::to_value(results).unwrap_or_else(|_| json!({}))))
}

fn csv_response(body: String, stem: &str) -> Result<Response, ApiError> {
    let day = time::OffsetDateTime::now_utc().date().to_string();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/csv; charset=utf-8",
        )
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{stem}-{day}.csv\""),
        )
        .body(axum::body::Body::from(body))
        .expect("a CSV response header is static and valid"))
}

// ---------------------------------------------------------------------------------------------
// Transfers (docs/requests/REQ-053, slice 3)
// ---------------------------------------------------------------------------------------------

/// Parse an opaque cursor into the id it points at.
///
/// A malformed cursor is a `None` rather than a refusal, and that is a deliberate asymmetry with
/// the other filters: a cursor is an opaque continuation token, and a client that lost one wants
/// the first page again, not a `400` it cannot recover from without reading our error body. The
/// filters that a person types (`status=`, `kind=`) *are* refused, because a typo there silently
/// produces a list the person did not ask for.
fn parse_cursor(raw: Option<String>) -> Option<Uuid> {
    raw.and_then(|value| Uuid::parse_str(value.trim()).ok())
}

/// `GET /api/v1/inventory/transfers` — the transfer list.
pub async fn list_transfers(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<TransferListParams>,
) -> Result<Json<Page<omnion_module_inventory::transfers::TransferView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = omnion_module_inventory::transfers::TransferQuery {
        search: params.search,
        // `?status=` repeated is a comma list in the query string, which is what a `<select
        // multiple>` and a hand-written URL both produce.
        statuses: split_multi(params.status),
        from_location_id: params.from_location_id,
        to_location_id: params.to_location_id,
        open_only: params.open_only.unwrap_or(false),
        limit: params.limit,
        cursor: parse_cursor(params.cursor),
    };
    Ok(Json(
        omnion_module_inventory::transfers::list_transfers(state.db().pool(), organization_id, &query)
            .await?,
    ))
}

/// The list's filter.
#[derive(Debug, Deserialize)]
pub struct TransferListParams {
    /// Free text over the number, the note, the SKU and the item name.
    #[serde(default)]
    pub search: Option<String>,
    /// `draft`, `dispatched`, `received` or `cancelled` — repeated, or comma separated.
    #[serde(default)]
    pub status: Option<String>,
    /// The source location.
    #[serde(default)]
    pub from_location_id: Option<Uuid>,
    /// The target location.
    #[serde(default)]
    pub to_location_id: Option<Uuid>,
    /// Only the ones still in flight.
    #[serde(default)]
    pub open_only: Option<bool>,
    /// How many rows.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The page cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// Split a repeated-or-comma query parameter into its values.
///
/// `?status=a&status=b` and `?status=a,b` are both what a browser produces — the first from a
/// multi-select, the second from a link somebody typed — and treating them differently would make
/// the filter work in the list and silently do nothing in a shared URL. Empty segments drop out,
/// so a trailing comma is not an error.
fn split_multi(raw: Option<String>) -> Vec<String> {
    raw.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(ToString::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// `GET /api/v1/inventory/transfers/{id}` — one transfer with its lines.
pub async fn get_transfer(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(transfer_id): Path<Uuid>,
) -> Result<Json<omnion_module_inventory::transfers::TransferView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        omnion_module_inventory::transfers::get_transfer(
            state.db().pool(),
            organization_id,
            transfer_id,
        )
        .await?,
    ))
}

/// `POST /api/v1/inventory/transfers` — write a draft.
///
/// A draft moves nothing: the available check happens at dispatch, where the stock actually
/// leaves, and the module's refusal then carries the number the person at the shelf needs.
pub async fn create_transfer(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    body: Json<omnion_module_inventory::transfers::NewTransfer>,
) -> Result<(StatusCode, Json<omnion_module_inventory::transfers::TransferView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let view = omnion_module_inventory::transfers::create_transfer(
        state.db().pool(),
        organization_id,
        &body.0,
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.transfer.created")
            .organization(organization_id)
            .target("inventory_transfer", view.id.to_string())
            .metadata(json!({
                "number": view.number,
                "from_location_id": view.from_location_id,
                "to_location_id": view.to_location_id,
                "lines": view.lines.len(),
                "quantity_total": view.quantity_total,
            }))
            .ip_address(None),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(view)))
}

/// `POST /api/v1/inventory/transfers/{id}/dispatch` — book the goods out and into transit.
pub async fn dispatch_transfer(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(transfer_id): Path<Uuid>,
) -> Result<Json<omnion_module_inventory::transfers::TransferView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let view =
        omnion_module_inventory::transfers::dispatch(state.db().pool(), organization_id, transfer_id, Some(current.user.id))
            .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.transfer.dispatched")
            .organization(organization_id)
            .target("inventory_transfer", view.id.to_string())
            .metadata(json!({
                "number": view.number,
                "lines": view.lines.len(),
                "quantity_total": view.quantity_total,
                "from_location_id": view.from_location_id,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.transfer.dispatched")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "transfer_id": view.id,
                "number": view.number,
                "quantity": view.quantity_total,
                "from_location_id": view.from_location_id,
            })),
    )
    .await;

    Ok(Json(view))
}

/// What a receive books in. Every line may be a **part** of what was sent.
#[derive(Debug, Deserialize)]
pub struct ReceiveBody {
    /// The lines that arrived.
    #[serde(default)]
    pub lines: Vec<omnion_module_inventory::transfers::TransferStepLine>,
    /// Organization to act on.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `POST /api/v1/inventory/transfers/{id}/receive` — book the goods in at the target.
///
/// The body's `organization_id` is read from the **body** rather than the query string because
/// this is the one transfer route a form posts to with a JSON body, and a form that has to put
/// the tenant in the URL as well as the payload is a form somebody will get wrong.
pub async fn receive_transfer(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(transfer_id): Path<Uuid>,
    body: Json<ReceiveBody>,
) -> Result<Json<omnion_module_inventory::transfers::TransferView>, ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id).await?;
    let view = omnion_module_inventory::transfers::receive(
        state.db().pool(),
        organization_id,
        transfer_id,
        &body.0.lines,
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.transfer.received")
            .organization(organization_id)
            .target("inventory_transfer", view.id.to_string())
            .metadata(json!({
                "number": view.number,
                "status": view.status.as_str(),
                "received_total": view.received_total,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("inventory.transfer.received")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "transfer_id": view.id,
                "number": view.number,
                "status": view.status.as_str(),
                "received_total": view.received_total,
            })),
    )
    .await;

    Ok(Json(view))
}

/// `POST /api/v1/inventory/transfers/{id}/cancel` — withdraw it.
///
/// Cancelling a **dispatched** transfer is not free: the goods come home, and that is two more
/// ledger rows. Writing only a status change would leave the transit balance holding goods the
/// document says returned, and the next replay would report a disagreement the module had
/// manufactured itself.
pub async fn cancel_transfer(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(transfer_id): Path<Uuid>,
) -> Result<Json<omnion_module_inventory::transfers::TransferView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let view =
        omnion_module_inventory::transfers::cancel(state.db().pool(), organization_id, transfer_id, Some(current.user.id))
            .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.transfer.cancelled")
            .organization(organization_id)
            .target("inventory_transfer", view.id.to_string())
            .metadata(json!({
                "number": view.number,
                "was_dispatched": view.dispatched_at.is_some(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(view))
}

// ---------------------------------------------------------------------------------------------
// Low-stock alerts (docs/requests/REQ-053, slice 3)
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/inventory/alerts` — the alert inbox.
pub async fn list_alerts(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<AlertListParams>,
) -> Result<Json<Page<omnion_module_inventory::alerts::AlertView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = omnion_module_inventory::alerts::AlertQuery {
        search: params.search,
        kinds: split_multi(params.kind),
        open_only: params.open_only.unwrap_or(true),
        limit: params.limit,
        cursor: parse_cursor(params.cursor),
    };
    Ok(Json(
        omnion_module_inventory::alerts::list_alerts(state.db().pool(), organization_id, &query)
            .await?,
    ))
}

/// The inbox's filter.
#[derive(Debug, Deserialize)]
pub struct AlertListParams {
    /// Free text over the SKU, the name and the location code.
    #[serde(default)]
    pub search: Option<String>,
    /// `low_stock` or `negative_stock` — repeated, or comma separated.
    #[serde(default)]
    pub kind: Option<String>,
    /// Only the unanswered ones. **On by default**, because an inbox that opens on a year of
    /// closed episodes is not an inbox.
    #[serde(default)]
    pub open_only: Option<bool>,
    /// How many rows.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The page cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/inventory/alerts/open-count` — the badge.
pub async fn open_alert_count(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let open = omnion_module_inventory::alerts::open_count(state.db().pool(), organization_id).await?;
    Ok(Json(json!({ "open": open })))
}

/// `POST /api/v1/inventory/alerts/sweep` — raise or clear what the current balances call for.
///
/// The sweep is **idempotent**, which is what lets it run on a read (`alerts_on_read`) without a
/// busy warehouse raising one alert per page view. It is exposed as a route as well because an
/// installation that turned that setting off needs something to call on a schedule, and a
/// document that says "run the sweep" with no way to run it is a feature that does not exist.
pub async fn sweep_alerts(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<omnion_module_inventory::alerts::Swept>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let swept = omnion_module_inventory::alerts::sweep(state.db().pool(), organization_id).await?;

    if swept.raised > 0 {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "inventory.alert.swept")
                .organization(organization_id)
                .target("inventory_alert", organization_id.to_string())
                .metadata(json!({
                    "examined": swept.examined,
                    "raised": swept.raised,
                    "cleared": swept.cleared,
                    "open": swept.open,
                }))
                .ip_address(None),
        )
        .await?;

        // One event per sweep rather than per alert, because the automation rule wants "this
        // shelf needs attention", not four hundred copies of it. The payload carries the count
        // so a rule can branch on it.
        emit(
            &state,
            NewEvent::new("inventory.alert.raised")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "raised": swept.raised,
                    "cleared": swept.cleared,
                    "open": swept.open,
                })),
        )
        .await;
    }

    Ok(Json(swept))
}

// ---------------------------------------------------------------------------------------------
// The stocktake (REQ-053 slice 4)
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/inventory/stocktake` — the session list.
pub async fn list_stocktakes(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<StocktakeListParams>,
) -> Result<Json<Page<omnion_module_inventory::stocktake::StocktakeView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = omnion_module_inventory::stocktake::StocktakeQuery {
        search: params.search,
        // `?status=` repeated is a comma list in the query string, which is what a `<select
        // multiple>` and a hand-written URL both produce.
        statuses: split_multi(params.status),
        open_only: params.open_only.unwrap_or(false),
        limit: params.limit,
        cursor: parse_cursor(params.cursor),
    };
    Ok(Json(
        omnion_module_inventory::stocktake::list_stocktakes(state.db().pool(), organization_id, &query)
            .await?,
    ))
}

/// The list's filter.
#[derive(Debug, Deserialize)]
pub struct StocktakeListParams {
    /// Free text over the number and the note.
    #[serde(default)]
    pub search: Option<String>,
    /// `open`, `closed` or `cancelled` — repeated, or comma separated.
    #[serde(default)]
    pub status: Option<String>,
    /// Only the sheets still counting.
    #[serde(default)]
    pub open_only: Option<bool>,
    /// How many rows.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The page cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Organization to read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/inventory/stocktake/{id}` — one session with its sheet.
pub async fn get_stocktake(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(stocktake_id): Path<Uuid>,
) -> Result<Json<omnion_module_inventory::stocktake::StocktakeView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        omnion_module_inventory::stocktake::get_stocktake(state.db().pool(), organization_id, stocktake_id)
            .await?,
    ))
}

/// `GET /api/v1/inventory/stocktake/{id}/report` — the variance report.
///
/// **A separate route rather than a `?report=1` on the detail**, because the report is the
/// thing somebody opens six months later: the sheet says what the counter *wrote*, and this says
/// what the ledger *holds*. Those are two claims, and a report that derived its numbers from the
/// sheet would agree with it by construction and prove nothing.
pub async fn stocktake_report(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(stocktake_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let view =
        omnion_module_inventory::stocktake::get_stocktake(state.db().pool(), organization_id, stocktake_id)
            .await?;
    let movements = omnion_module_inventory::stocktake::variance_movements(
        state.db().pool(),
        organization_id,
        stocktake_id,
    )
    .await?;

    // The header's frozen total beside the ledger's own sum: two numbers computed by different
    // routes, and the report is only honest if they are both shown. A report that printed one
    // of them could not fail, and a report that cannot fail is a screenshot.
    let from_ledger: Quantity = movements
        .iter()
        .fold(Quantity::ZERO, |acc, movement| {
            acc.checked_add(movement.quantity).unwrap_or(acc)
        });

    Ok(Json(json!({
        "stocktake": view,
        "movements": movements,
        "variance_total": view.variance_total,
        "ledger_total": from_ledger.to_text(),
        "agrees": from_ledger.to_text() == view.variance_total,
    })))
}

/// What a new count asks for.
#[derive(Debug, Deserialize)]
pub struct CreateStocktakeBody {
    /// The shelves being counted.
    #[serde(default)]
    pub location_ids: Vec<Uuid>,
    /// Narrow the sheet to one category.
    #[serde(default)]
    pub category: Option<String>,
    /// What the count is for.
    #[serde(default)]
    pub counted_on: Option<String>,
    /// The note.
    #[serde(default)]
    pub note: Option<String>,
    /// Organization to act on.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `POST /api/v1/inventory/stocktake` — open a sheet.
pub async fn create_stocktake(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<CreateStocktakeBody>,
) -> Result<(StatusCode, Json<omnion_module_inventory::stocktake::StocktakeView>), ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id).await?;
    let view = omnion_module_inventory::stocktake::create_stocktake(
        state.db().pool(),
        organization_id,
        &omnion_module_inventory::stocktake::NewStocktake {
            location_ids: body.0.location_ids,
            category: body.0.category,
            counted_on: body.0.counted_on,
            note: body.0.note,
        },
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.stocktake.opened")
            .organization(organization_id)
            .target("inventory_stocktake", view.id.to_string())
            .metadata(json!({
                "number": view.number,
                "locations": view.location_codes,
                "lines": view.lines_counted,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(view)))
}

/// What a count asks for.
#[derive(Debug, Deserialize)]
pub struct CountBody {
    /// The lines the counter saw.
    #[serde(default)]
    pub lines: Vec<omnion_module_inventory::stocktake::StocktakeCount>,
    /// Organization to act on.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `POST /api/v1/inventory/stocktake/{id}/count` — write what the counter saw.
///
/// **A count moves nothing.** The audit row records it because the *document* changes — a
/// number somebody typed is a fact about the count even when stock does not move yet — and the
/// route is deliberately a write guarded by the stocktake key: an open sheet is a document other
/// people read, and a counter who may not be trusted to close one may still be trusted to fill
/// it in.
pub async fn count_stocktake(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(stocktake_id): Path<Uuid>,
    body: Json<CountBody>,
) -> Result<Json<omnion_module_inventory::stocktake::StocktakeView>, ApiError> {
    let organization_id = organization_of(&state, &current, body.0.organization_id).await?;
    let view = omnion_module_inventory::stocktake::count(
        state.db().pool(),
        organization_id,
        stocktake_id,
        &body.0.lines,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.stocktake.counted")
            .organization(organization_id)
            .target("inventory_stocktake", stocktake_id.to_string())
            .metadata(json!({
                "number": view.number,
                "counted": body.0.lines.len(),
                "pending": view.lines_pending,
                "variances": view.variances_count,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(view))
}

/// `POST /api/v1/inventory/stocktake/{id}/close` — post the variances and finish the sheet.
pub async fn close_stocktake(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(stocktake_id): Path<Uuid>,
) -> Result<Json<omnion_module_inventory::stocktake::StocktakeOutcome>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let outcome = omnion_module_inventory::stocktake::close_stocktake(
        state.db().pool(),
        organization_id,
        stocktake_id,
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.stocktake.closed")
            .organization(organization_id)
            .target("inventory_stocktake", stocktake_id.to_string())
            .metadata(json!({
                "lines": outcome.lines,
                "variances": outcome.variances,
                "variance_total": outcome.variance_total,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    // **One event per close, not per variance** — the same reasoning the alert sweep gives. The
    // automation rule wants "the count is in", and a rule that fires two hundred times for two
    // hundred lines is a rule nobody will leave switched on. The count is in the payload so a
    // rule can branch on it.
    emit(
        &state,
        NewEvent::new("inventory.stocktake.closed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "stocktake_id": stocktake_id,
                "lines": outcome.lines,
                "variances": outcome.variances,
                "variance_total": outcome.variance_total,
            })),
    )
    .await;

    Ok(Json(outcome))
}

/// `POST /api/v1/inventory/stocktake/{id}/cancel` — withdraw the sheet.
///
/// Posts nothing, and that is the difference from a transfer's cancel: nothing moved, so a
/// ledger row would describe an event that did not happen. The audit row is still written,
/// because a session that vanished without a trace is the sort of thing an auditor asks about.
pub async fn cancel_stocktake(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(stocktake_id): Path<Uuid>,
) -> Result<Json<omnion_module_inventory::stocktake::StocktakeView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let view = omnion_module_inventory::stocktake::cancel_stocktake(
        state.db().pool(),
        organization_id,
        stocktake_id,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "inventory.stocktake.cancelled")
            .organization(organization_id)
            .target("inventory_stocktake", stocktake_id.to_string())
            .metadata(json!({ "number": view.number, "lines": view.lines_counted }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(view))
}

// ---------------------------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------------------------

/// Whether the caller holds a permission in this organization, resolved once per write.
///
/// The negative-stock rule needs an answer the module cannot produce for itself, and getting it
/// **wrong in the permissive direction** would let any warehouse operator drive stock below zero,
/// so a failure to read the role store is treated as "does not hold it" rather than propagated:
/// the write is refused and the audit trail shows the refusal, which is the safe direction for
/// both of them.
async fn holds_permission(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
    key: &str,
) -> Result<bool, ApiError> {
    // `omnion_permissions::authorize` is the **same** resolution the route guard uses — role
    // bindings first, then the organization's ABAC policies — so the negative-stock rule and the
    // permission on the route cannot disagree about who may drive stock below zero. Reading the
    // role graph anywhere else would be a second implementation of one decision.
    let scope = omnion_permissions::model::Scope::Organization { organization_id };
    match omnion_permissions::evaluate::authorize(state.db().pool(), current.user.id, scope, key).await {
        Ok(decision) => Ok(decision.is_allowed()),
        Err(error) => {
            tracing::warn!(error = %error, %key, "the role store could not be read; refusing");
            Ok(false)
        }
    }
}

/// `true` when the database refused to answer, rather than refusing the statement.
///
/// A `503` for this and a `500` for everything else: the two have opposite recoveries, and a
/// client that cannot tell them apart retries the one that will never succeed.
fn database_unavailable(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_)
    )
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

impl From<InventoryError> for ApiError {
    /// The inventory module's refusals, mapped the way every other module's are: a validation
    /// failure is a `400` naming the field the form renders it under, a missing or
    /// out-of-organization record is a `404`, a taken SKU or code is a `409`, and **a write the
    /// numbers refuse is a `422`** — nothing the caller typed is malformed, the request is
    /// well-formed and the warehouse does not have the stock, which is a different thing and
    /// deserves a different status.
    fn from(error: InventoryError) -> Self {
        match error {
            InventoryError::Invalid {
                entity,
                field,
                message,
            } => Self::bad_request("invalid_inventory_record", message)
                .with_details(json!({ "entity": entity, "field": field })),
            InventoryError::InvalidQuery(message) => {
                Self::bad_request("invalid_inventory_query", message)
            }
            // A `422` and not a `400`: nothing about the request was malformed, the
            // organization simply cannot be valued as one number. The currencies
            // travel in `details` so the screen can list them.
            InventoryError::MixedCurrency { currencies } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "inventory_mixed_currency",
                format!(
                    "this scope prices stock in {} currencies, so it has no single value",
                    currencies.len()
                ),
            )
            .with_details(json!({ "currencies": currencies })),
            InventoryError::NotFound(kind) => Self::new(
                StatusCode::NOT_FOUND,
                match kind {
                    "item" => "inventory_item_not_found",
                    "location" => "inventory_location_not_found",
                    "warehouse" => "inventory_warehouse_not_found",
                    "movement" => "inventory_movement_not_found",
                    _ => "inventory_record_not_found",
                },
                format!("no such {kind} in this organization"),
            ),
            InventoryError::CodeTaken { entity, code } => Self::new(
                StatusCode::CONFLICT,
                "inventory_code_taken",
                format!("another {entity} of this organization is already called {code}"),
            ),
            // The number is in the **sentence** as well as in `details`, and that duplication is
            // deliberate. `details` is what a form renders beside the quantity input; the sentence
            // is what a person reads in a log, in an email and on a terminal. The first version
            // put the available quantity only in `details`, and the walk's assertion — which
            // checks the sentence, because a human reads the sentence — failed on a message that
            // said "this would leave −3.000" without ever saying what *was* there.
            InventoryError::WouldGoNegative {
                message,
                available,
            } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "inventory_insufficient_stock",
                format!("{message} (available {available})"),
            )
            .with_details(json!({ "available": available })),
            InventoryError::InvalidStatusChange(message) => {
                Self::new(StatusCode::CONFLICT, "inventory_status_conflict", message)
            }
            InventoryError::ApprovalNotGranted(message) => Self::new(
                StatusCode::CONFLICT,
                "inventory_approval_required",
                message,
            ),
            InventoryError::InvalidNumber {
                entity,
                field,
                source,
            } => Self::bad_request("invalid_inventory_record", source.to_string())
                .with_details(json!({ "entity": entity, "field": field })),
            InventoryError::MissingPermission(key) => Self::new(
                StatusCode::FORBIDDEN,
                "inventory_permission_required",
                format!("this write needs {key}"),
            ),
            InventoryError::ForeignKey { kind, id } => Self::new(
                StatusCode::BAD_REQUEST,
                "inventory_foreign_key",
                format!("{kind} {id} is not in this organization"),
            ),
            // The same split the sales module makes, for the same reason: a pool that timed out is
            // a **503 the caller retries**, and a statement the database refused is a `500` that
            // retrying will reproduce. Reporting both as one code teaches a client to retry a
            // unique violation forever.
            InventoryError::Database(error) if database_unavailable(&error) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            InventoryError::Database(error) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", error.to_string())
            }
        }
    }
}
