//! `/api/v1/sales` — the sellable catalog and its price lists (docs/requests/REQ-052, slice 1).
//!
//! The catalog is the first half of the selling side, and the one thing it has to get right is
//! that **the price a quote line is prefilled with is the price it will be charged**. That is why
//! there is a route for the resolution ([`resolve_line_price`]) rather than three screens each
//! picking a price out of the two tables: a builder that prefilled from the list and a server that
//! charged the default would differ by the price list, on the first quote of the day.
//!
//! The rest is the shape the platform gives every module (docs/requests/REQ-051 wrote it and this
//! route follows it rather than inventing a second one):
//!
//! * the caller's organization is resolved by the CRM's rule, so a screen opened on `/sales/*`
//!   with no query string shows **its** records rather than a prompt to pick a tenant;
//! * a record of another organization is a `404`, never a `403` — a `403` would confirm it
//!   exists, and one organization's catalog is the thing this module exists to keep apart;
//! * every mutation writes an audit row with the actor, what changed and the before/after, and
//!   emits the documented `sales.*` events for automations and webhook subscribers.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_module_sales::store::{
    self, CatalogQuery, NewPriceList, NewPriceRow, NewProduct, Page, PriceListDetail,
    PriceListPatch, PriceListView, PriceRowView, ProductPatch, ProductView, SettingsPatch,
};
use omnion_module_sales::SalesError;
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

/// The query of a product list.
#[derive(Debug, Deserialize)]
pub struct ProductListParams {
    /// Free text over SKU, name and description.
    #[serde(default)]
    pub search: Option<String>,
    /// One category the product must carry.
    #[serde(default)]
    pub category: Option<String>,
    /// `true` for live only, `false` for inactive only, absent for both.
    #[serde(default)]
    pub active: Option<bool>,
    /// Include the archived products.
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// Sort key.
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
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl From<ProductListParams> for CatalogQuery {
    fn from(params: ProductListParams) -> Self {
        Self {
            search: params.search,
            category: params.category,
            active: params.active,
            include_archived: params.include_archived,
            sort: params.sort,
            direction: params.direction,
            limit: params.limit,
            cursor: params.cursor,
        }
    }
}

/// The query of a price-list list: the same shape, minus the columns a list does not have.
#[derive(Debug, Deserialize)]
pub struct PriceListListParams {
    /// Free text over the list's name.
    #[serde(default)]
    pub search: Option<String>,
    /// `true` for live only, `false` for inactive only, absent for both.
    #[serde(default)]
    pub active: Option<bool>,
    /// Include the archived lists.
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// Sort key.
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
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl From<PriceListListParams> for CatalogQuery {
    fn from(params: PriceListListParams) -> Self {
        Self {
            search: params.search,
            active: params.active,
            include_archived: params.include_archived,
            sort: params.sort,
            direction: params.direction,
            limit: params.limit,
            cursor: params.cursor,
            ..CatalogQuery::default()
        }
    }
}

/// The body of a price-row replacement: the whole grid, because the editor saves the whole grid.
#[derive(Debug, Deserialize)]
pub struct ReplacePriceRows {
    /// The rows the list should carry afterwards.
    #[serde(default)]
    pub items: Vec<NewPriceRow>,
}

/// The query of the price-resolution call the builder makes per line.
///
/// `product_id` is a **path** parameter here, not a query one: the route is
/// `/sales/products/{id}/price`, and a struct that also declared `product_id` would be a second,
/// contradictory source of the same value. The id is in the path and only the quantity and the
/// list are in the query.
#[derive(Debug, Deserialize)]
pub struct ResolveParams {
    /// How many of it, as a decimal text such as `1` or `2.5`.
    #[serde(default)]
    pub quantity: Option<String>,
    /// The price list the builder has selected, if any.
    #[serde(default)]
    pub price_list_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------------------------
// Audit and events
// ---------------------------------------------------------------------------------------------

/// Which product fields a write actually changed.
///
/// Typed rather than "serialise both rows and diff the JSON": the fields a person edits are the
/// ones a person can see, and a diff over two serialised views would also report a change to the
/// `updated_at` this same write moved.
fn product_changes(before: &ProductView, after: &ProductView) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    let before_product = &before.product;
    let after_product = &after.product;
    if before_product.sku != after_product.sku {
        changed.push("sku".to_owned());
    }
    if before_product.name != after_product.name {
        changed.push("name".to_owned());
    }
    if before_product.description != after_product.description {
        changed.push("description".to_owned());
    }
    if before_product.category != after_product.category {
        changed.push("category".to_owned());
    }
    if before_product.unit != after_product.unit {
        changed.push("unit".to_owned());
    }
    if before_product.tax_percent != after_product.tax_percent {
        changed.push("tax_percent".to_owned());
    }
    if before_product.default_price != after_product.default_price {
        changed.push("default_price".to_owned());
    }
    if before_product.currency != after_product.currency {
        changed.push("currency".to_owned());
    }
    if before_product.active != after_product.active {
        changed.push("active".to_owned());
    }
    changed
}

/// Which price-list fields a write actually changed.
fn price_list_changes(before: &PriceListView, after: &PriceListView) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    if before.name != after.name {
        changed.push("name".to_owned());
    }
    if before.currency != after.currency {
        changed.push("currency".to_owned());
    }
    if before.active != after.active {
        changed.push("active".to_owned());
    }
    if before.valid_from != after.valid_from {
        changed.push("valid_from".to_owned());
    }
    if before.valid_until != after.valid_until {
        changed.push("valid_until".to_owned());
    }
    changed
}

/// The identity of a product for an event payload.
fn product_ref(product: &ProductView) -> Value {
    json!({
        "product_id": product.id,
        "organization_id": product.organization_id,
        "sku": product.product.sku,
        "name": product.product.name,
        "currency": product.product.currency,
        "default_price": product.product.default_price,
        "active": product.product.active,
    })
}

/// The identity of a price list for an event payload.
fn price_list_ref(list: &PriceListView) -> Value {
    json!({
        "price_list_id": list.id,
        "organization_id": list.organization_id,
        "name": list.name,
        "currency": list.currency,
        "item_count": list.item_count,
        "active": list.active,
    })
}

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the sales event could not be recorded");
    }
}

// ---------------------------------------------------------------------------------------------
// Products
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/sales/products` — one page of the catalog.
pub async fn list_products(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ProductListParams>,
) -> Result<Json<Page<ProductView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = CatalogQuery::from(params);

    Ok(Json(
        store::list_products(state.db().pool(), organization_id, &query).await?,
    ))
}

/// `GET /api/v1/sales/products/vocabulary` — the categories and units the two dropdowns draw.
///
/// A separate route rather than part of the list response because the list is re-fetched on every
/// keystroke in its search box and the vocabulary is not: sending the categories with each page
/// would be a second query per keystroke for data that changes once a week.
pub async fn catalog_vocabulary(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<store::CatalogVocabulary>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        store::catalog_vocabulary(state.db().pool(), organization_id).await?,
    ))
}

/// The one optional organization parameter the small reads carry.
#[derive(Debug, Deserialize)]
pub struct OrganizationParam {
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/sales/products/{id}` — one product.
pub async fn get_product(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(product_id): Path<Uuid>,
) -> Result<Json<ProductView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        store::get_product(state.db().pool(), organization_id, product_id).await?,
    ))
}

/// `POST /api/v1/sales/products` — create a product.
pub async fn create_product(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewProduct>,
) -> Result<(StatusCode, Json<ProductView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let created = store::create_product(state.db().pool(), organization_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.product.created")
            .organization(organization_id)
            .target("sales_product", created.id.to_string())
            .metadata(json!({
                "request_id": created.id,
                "after": product_ref(&created),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.product.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(product_ref(&created)),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /api/v1/sales/products/{id}` — edit a product.
pub async fn update_product(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(product_id): Path<Uuid>,
    body: Json<ProductPatch>,
) -> Result<Json<ProductView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_product(pool, organization_id, product_id).await?;
    let after = store::patch_product(pool, organization_id, product_id, &body.0).await?;

    let changed = product_changes(&before, &after);
    if !changed.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "sales.product.updated")
                .organization(organization_id)
                .target("sales_product", after.id.to_string())
                .metadata(json!({
                    "request_id": after.id,
                    "changed": changed,
                    "before": product_ref(&before),
                    "after": product_ref(&after),
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("sales.product.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({ "product_id": after.id, "changed": changed })),
        )
        .await;
    }

    Ok(Json(after))
}

/// `DELETE /api/v1/sales/products/{id}` — archive a product.
pub async fn archive_product(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(product_id): Path<Uuid>,
) -> Result<Json<ProductView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_product(pool, organization_id, product_id).await?;
    let after = store::archive_product(pool, organization_id, product_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.product.archived")
            .organization(organization_id)
            .target("sales_product", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "before": product_ref(&before),
                "after": product_ref(&after),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.product.archived")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(product_ref(&after)),
    )
    .await;

    Ok(Json(after))
}

/// `GET /api/v1/sales/products/{id}/price` — what a line of this quantity would cost.
///
/// The one answer the builder and the server both take, which is why it is a route rather than
/// something each screen recomputes: the panel prefills with this, the quote write recomputes it,
/// and a difference between the two is a bug a person sees as a wrong total.
pub async fn resolve_product_price(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(product_id): Path<Uuid>,
    Query(params): Query<ResolveParams>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let product = store::get_product(pool, organization_id, product_id).await?;

    let quantity = omnion_module_sales::money::Quantity::parse(
        params
            .quantity
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("1"),
    )
    .map_err(|source| {
        ApiError::bad_request(
            "invalid_sales_record",
            source.to_string(),
        )
        .with_details(json!({ "entity": "product", "field": "quantity" }))
    })?;

    let price = store::resolve_line_price(
        pool,
        organization_id,
        params.price_list_id,
        &product.product,
        quantity,
    )
    .await?;

    Ok(Json(json!({
        "product_id": product.id,
        "quantity": quantity.to_text(),
        "unit": product.product.unit,
        "unit_price": price.to_text(),
        "currency": product.product.currency,
        "tax_percent": product.product.tax_percent,
        "price_list_id": params.price_list_id,
        // Said out loud, because "the price came from somewhere else" is a question a person asks
        // when a quote total is not what they expected. `default` is not a silent failure mode.
        "source": if params.price_list_id.is_some() { "resolved" } else { "default" },
    })))
}

// ---------------------------------------------------------------------------------------------
// Price lists
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/sales/pricelists` — one page of price lists.
pub async fn list_price_lists(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<PriceListListParams>,
) -> Result<Json<Page<PriceListView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = CatalogQuery::from(params);

    Ok(Json(
        store::list_price_lists(state.db().pool(), organization_id, &query).await?,
    ))
}

/// `GET /api/v1/sales/pricelists/{id}` — one price list with its rows.
pub async fn get_price_list(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(list_id): Path<Uuid>,
) -> Result<Json<PriceListDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        store::get_price_list(state.db().pool(), organization_id, list_id).await?,
    ))
}

/// `POST /api/v1/sales/pricelists` — create a price list.
pub async fn create_price_list(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewPriceList>,
) -> Result<(StatusCode, Json<PriceListView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let created = store::create_price_list(state.db().pool(), organization_id, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.price_list.created")
            .organization(organization_id)
            .target("sales_price_list", created.id.to_string())
            .metadata(json!({
                "request_id": created.id,
                "after": price_list_ref(&created),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.price_list.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(price_list_ref(&created)),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /api/v1/sales/pricelists/{id}` — edit a price list.
pub async fn update_price_list(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(list_id): Path<Uuid>,
    body: Json<PriceListPatch>,
) -> Result<Json<PriceListView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_price_list(pool, organization_id, list_id).await?;
    let after = store::patch_price_list(pool, organization_id, list_id, &body.0).await?;

    let changed = price_list_changes(&before.list, &after);
    if !changed.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "sales.price_list.updated")
                .organization(organization_id)
                .target("sales_price_list", after.id.to_string())
                .metadata(json!({
                    "request_id": after.id,
                    "changed": changed,
                    "before": price_list_ref(&before.list),
                    "after": price_list_ref(&after),
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("sales.price_list.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({ "price_list_id": after.id, "changed": changed })),
        )
        .await;
    }

    Ok(Json(after))
}

/// `DELETE /api/v1/sales/pricelists/{id}` — archive a price list.
pub async fn archive_price_list(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(list_id): Path<Uuid>,
) -> Result<Json<PriceListView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_price_list(pool, organization_id, list_id).await?;
    let after = store::archive_price_list(pool, organization_id, list_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.price_list.archived")
            .organization(organization_id)
            .target("sales_price_list", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "before": price_list_ref(&before.list),
                "after": price_list_ref(&after),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.price_list.archived")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(price_list_ref(&after)),
    )
    .await;

    Ok(Json(after))
}

/// `PUT /api/v1/sales/pricelists/{id}/items` — replace the list's price rows.
///
/// `PUT` and not `PATCH`, because it is a replacement: a row missing from the request is a row the
/// list no longer has. The editor sends the whole grid, which is also what makes the single
/// transaction possible — a save that deleted the rows and then failed on the third insert would
/// leave a list that prices nothing, and every quote built on it would silently fall back to the
/// default price.
pub async fn replace_price_list_items(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(list_id): Path<Uuid>,
    body: Json<ReplacePriceRows>,
) -> Result<Json<PriceListDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_price_list(pool, organization_id, list_id).await?;
    let after = store::replace_price_list_items(pool, organization_id, list_id, &body.0.items).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.price_list.items_replaced")
            .organization(organization_id)
            .target("sales_price_list", list_id.to_string())
            .metadata(json!({
                "request_id": list_id,
                "before_rows": rows_audit(&before.items),
                "after_rows": rows_audit(&after.items),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.price_list.items_replaced")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "price_list_id": list_id,
                "item_count": after.items.len(),
                "removed": before.items.len(),
            })),
    )
    .await;

    Ok(Json(after))
}

/// The price rows as an audit row carries them: product, quantity and price, and nothing else.
///
/// Deliberately not the ids of the rows themselves — a replaced grid gets entirely new row ids,
/// so a before/after diff keyed on them would report every row as both removed and added and say
/// nothing about the prices that actually changed.
fn rows_audit(rows: &[PriceRowView]) -> Vec<Value> {
    let mut ordered: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "product_id": row.product_id,
                "sku": row.product_sku,
                "min_quantity": row.min_quantity,
                "price": row.price,
            })
        })
        .collect();
    ordered.sort_by(|a, b| a["sku"].as_str().cmp(&b["sku"].as_str()));
    ordered
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/sales/settings` — the organization's sales settings.
pub async fn get_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<omnion_module_sales::Settings>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        store::get_settings(state.db().pool(), organization_id).await?,
    ))
}

/// `PUT /api/v1/sales/settings` — write the organization's sales settings.
pub async fn update_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<SettingsPatch>,
) -> Result<Json<omnion_module_sales::Settings>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = store::get_settings(pool, organization_id).await?;
    let after = store::update_settings(pool, organization_id, &body.0).await?;

    // The approval threshold is the one field worth its own event: it changes what a seller is
    // allowed to send without a manager, and an automation that watches for "needs approval"
    // should hear about the rule changing, not only about the quotes it catches afterwards.
    let mut changed: Vec<String> = Vec::new();
    if before.currency != after.currency {
        changed.push("currency".to_owned());
    }
    if before.discount_approval_threshold != after.discount_approval_threshold {
        changed.push("discount_approval_threshold".to_owned());
    }
    if before.quote_validity_days != after.quote_validity_days {
        changed.push("quote_validity_days".to_owned());
    }
    if before.quote_number_prefix != after.quote_number_prefix {
        changed.push("quote_number_prefix".to_owned());
    }
    if before.order_number_prefix != after.order_number_prefix {
        changed.push("order_number_prefix".to_owned());
    }

    if !changed.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "sales.settings.updated")
                .organization(organization_id)
                .target("sales_settings", organization_id.to_string())
                .metadata(json!({
                    "request_id": organization_id,
                    "changed": changed,
                    "before": before,
                    "after": after,
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("sales.settings.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({ "changed": changed })),
        )
        .await;
    }

    Ok(Json(after))
}

// ---------------------------------------------------------------------------------------------
// The module's refusals, in the platform's HTTP vocabulary
// ---------------------------------------------------------------------------------------------

impl From<SalesError> for ApiError {
    /// The sales module's refusals, mapped the way every other module's are: a validation failure
    /// is a `400` naming the field the form renders it under, a missing or out-of-organization
    /// record is a `404`, a taken SKU or name is a `409`, and **a write against a document the
    /// customer has already seen is its own conflict** — the caller is not wrong about the data,
    /// they are asking for something the module will not do, and reporting it as a `400` would
    /// send them looking for a bad field that does not exist.
    fn from(error: SalesError) -> Self {
        match error {
            SalesError::Invalid {
                entity,
                field,
                message,
            } => Self::bad_request("invalid_sales_record", message)
                .with_details(json!({ "entity": entity, "field": field })),
            SalesError::InvalidQuery(message) => {
                Self::bad_request("invalid_sales_query", message)
            }
            SalesError::NotFound(kind) => Self::new(
                StatusCode::NOT_FOUND,
                match kind {
                    "product" => "product_not_found",
                    "price list" => "price_list_not_found",
                    "quote" => "quote_not_found",
                    "order" => "order_not_found",
                    _ => "sales_record_not_found",
                },
                format!("no such {kind} in this organization"),
            ),
            SalesError::SkuTaken => Self::new(
                StatusCode::CONFLICT,
                "product_sku_taken",
                "another product of this organization already uses this SKU",
            ),
            SalesError::NameTaken { entity, name } => Self::new(
                StatusCode::CONFLICT,
                "sales_name_taken",
                format!("another {entity} of this organization is already called {name}"),
            ),
            SalesError::AlreadySent { entity, number } => Self::new(
                StatusCode::CONFLICT,
                "sales_document_already_sent",
                format!(
                    "{entity} {number} has already been sent — duplicate it into a new draft instead of editing it"
                ),
            ),
            SalesError::InvalidStatusChange(message) => {
                Self::bad_request("invalid_sales_status_change", message)
            }
            // One message for all three reasons a token does not resolve, so a caller cannot use
            // the error to learn which tokens exist.
            SalesError::InvalidPublicToken => Self::new(
                StatusCode::NOT_FOUND,
                "sales_link_not_valid",
                "this link is no longer valid",
            ),
            SalesError::InvalidNumber {
                entity,
                field,
                source,
            } => Self::bad_request("invalid_sales_record", source.to_string())
                .with_details(json!({ "entity": entity, "field": field })),
            SalesError::Database(err) if database_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            SalesError::Database(err) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", err.to_string())
            }
        }
    }
}

/// `true` when the database refused to answer, rather than refusing the statement.
fn database_unavailable(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// The JSON body an `ApiError` turns into, read through its own `IntoResponse` — the shape a
    /// client actually receives, nested under `error` as the rest of the API documents.
    async fn body_of(error: ApiError) -> Value {
        let response = error.into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("the error body must read");
        serde_json::from_slice(&bytes).expect("the error body is JSON")
    }

    /// A product as a price-list change diff would compare it.
    ///
    /// The two timestamps are `OffsetDateTime::UNIX_EPOCH` rather than "now" on purpose: the
    /// fixture has to be identical on both sides of a diff unless the test is *about* a timestamp,
    /// and a fresh `now()` in a helper called twice would make `updated_at` differ and teach the
    /// diff to ignore the one column it must never report.
    fn product(sku: &str, price: &str, active: bool) -> ProductView {
        ProductView {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            product: omnion_module_sales::catalog::Product {
                id: Uuid::nil(),
                sku: sku.to_owned(),
                name: "Sample".to_owned(),
                description: String::new(),
                category: None,
                unit: omnion_module_sales::Unit::Piece,
                tax_percent: 20,
                default_price: omnion_module_sales::money::Money::parse(price).unwrap(),
                currency: "TRY".to_owned(),
                active,
            },
            archived_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[tokio::test]
    async fn a_refused_field_is_a_four_hundred_that_names_where_it_goes() {
        let body = body_of(
            SalesError::invalid("product", "sku", "use 2–32 letters, digits, . _ or -").into(),
        )
        .await;
        assert_eq!(body["error"]["code"], "invalid_sales_record");
        assert_eq!(body["error"]["details"]["entity"], "product");
        assert_eq!(body["error"]["details"]["field"], "sku");
    }

    #[tokio::test]
    async fn a_bad_number_reports_the_fields_own_reason_not_a_generic_one() {
        let error = SalesError::number(
            "price_list_item",
            "min_quantity",
            omnion_module_sales::money::MoneyError::TooPrecise,
        );
        let body = body_of(error.into()).await;
        assert_eq!(body["error"]["code"], "invalid_sales_record");
        assert_eq!(body["error"]["details"]["field"], "min_quantity");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("too many decimal places"),
            "the module's own reason reaches the form: {body}"
        );
    }

    #[tokio::test]
    async fn a_record_of_another_organization_is_a_four_oh_four_and_names_only_its_kind() {
        for (kind, code) in [
            ("product", "product_not_found"),
            ("price list", "price_list_not_found"),
            ("quote", "quote_not_found"),
        ] {
            let body = body_of(SalesError::NotFound(kind).into()).await;
            assert_eq!(body["error"]["code"], code);
            let message = body["error"]["message"].as_str().unwrap().to_owned();
            assert!(message.contains(kind), "{message}");
            assert!(
                !message.contains("organization_id"),
                "a 404 must not coach a caller to name a tenant it may not name: {message}"
            );
        }
    }

    #[tokio::test]
    async fn a_taken_sku_and_a_taken_name_are_two_conflicts_not_one() {
        let sku = body_of(SalesError::SkuTaken.into()).await;
        assert_eq!(sku["error"]["code"], "product_sku_taken");

        let name = body_of(SalesError::name_taken("price list", "Wholesale").into()).await;
        assert_eq!(name["error"]["code"], "sales_name_taken");
        assert!(name["error"]["message"].as_str().unwrap().contains("Wholesale"));
    }

    #[tokio::test]
    async fn a_sent_document_is_a_conflict_that_says_duplicate_not_a_bad_field() {
        let body = body_of(SalesError::already_sent("quote", "Q-2026-0007").into()).await;
        assert_eq!(body["error"]["code"], "sales_document_already_sent");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("Q-2026-0007"), "{message}");
        assert!(message.contains("duplicate"), "{message}");
        assert!(
            !message.contains("invalid"),
            "this is not a validation failure and must not read like one: {message}"
        );
    }

    #[tokio::test]
    async fn a_dead_public_link_is_a_four_oh_four_with_one_message_for_every_reason() {
        let body = body_of(SalesError::InvalidPublicToken.into()).await;
        assert_eq!(body["error"]["code"], "sales_link_not_valid");
        // A caller must not be able to tell "expired" from "already accepted" from "never existed".
        let message = body["error"]["message"].as_str().unwrap();
        assert!(!message.contains("expired"), "{message}");
        assert!(!message.contains("accepted"), "{message}");
    }

    #[tokio::test]
    async fn the_audit_diff_names_only_the_fields_a_person_edited() {
        let before = product("AB-1", "10.00", true);
        let after = product("AB-1", "12.00", true);
        assert_eq!(product_changes(&before, &after), vec!["default_price"]);

        let renamed = product("AB-2", "10.00", true);
        let archived = product("AB-1", "10.00", false);
        let mut changed = product_changes(&renamed, &archived);
        changed.sort();
        assert_eq!(changed, vec!["active", "sku"]);

        // The same row read twice: the write moved `updated_at`, and a diff that included the
        // bookkeeping columns would report a change on every save and teach people to ignore it.
        assert!(product_changes(&before, &before.clone()).is_empty());
    }

    #[tokio::test]
    async fn an_audit_diff_of_price_rows_is_ordered_by_sku_so_it_can_be_read() {
        // A diff keyed on row ids would call every row removed and every row added, because a
        // replaced grid gets new ids — and would say nothing about the prices that changed.
        let row = |sku: &str, price: &str| PriceRowView {
            id: Uuid::new_v4(),
            product_id: Uuid::new_v4(),
            product_sku: sku.to_owned(),
            product_name: sku.to_owned(),
            min_quantity: "1.000".to_owned(),
            price: price.to_owned(),
            unit: "piece".to_owned(),
        };
        let ordered = rows_audit(&[row("Z-9", "1.00"), row("A-1", "2.00")]);
        let skus: Vec<&str> = ordered
            .iter()
            .map(|value| value["sku"].as_str().unwrap())
            .collect();
        assert_eq!(skus, vec!["A-1", "Z-9"], "the diff reads the same way twice");
    }
}
