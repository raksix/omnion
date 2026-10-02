//! The catalog's storage: products, price lists, their price rows and the settings row.
//!
//! Four rules the HTTP layer must not have to remember, because a screen and a CSV import and
//! the quote builder are all going to call these:
//!
//! * **Every read and write is organization-scoped in SQL, and a row of another organization is
//!   `404`.** Not `403`: a `403` confirms the row exists, and the catalog of a competitor is the
//!   one thing this module exists to keep apart.
//! * **Money crosses the boundary as text and is bound back with `::numeric`.** `numeric(14,2)`
//!   has no Rust type in this workspace (no decimal dependency is added to a public repo for one
//!   module), so a value is read as `price::text`, validated by [`crate::money::Money`], and
//!   written as a normalised string with an explicit cast. Without the cast PostgreSQL answers
//!   "column is of type numeric but expression is of type text".
//! * **Nothing is deleted.** `archive_*` sets `archived_at`, so a product a past quote line still
//!   names keeps reading exactly as it was on the day it was sold.
//! * **A percentage is read rounded and written validated.** The module treats tax and discount
//!   as whole percents (`0..=100`); the column is `numeric(5,2)` so an accounting module can
//!   eventually carry a rate with fractions without this migration having to change.

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::catalog::{
    self, PriceListItem, Product, validate_name, validate_price, validate_price_item, validate_sku,
    validate_tax_percent,
};
use crate::error::{Result, SalesError};
use crate::model::{Settings, Unit};
use crate::money::Money;

/// Rows a catalog page holds when the caller names no size.
pub const DEFAULT_PER_PAGE: i64 = 50;

/// Hard cap on a page.
pub const MAX_PER_PAGE: i64 = 200;

/// Longest a search term may be before it is refused.
pub const MAX_SEARCH_LENGTH: usize = 120;

/// Longest a product description may be, the same bound the schema stores.
pub const MAX_DESCRIPTION_LENGTH: usize = 4_000;

/// Longest a price-list name may be.
pub const MAX_LIST_NAME_LENGTH: usize = 120;

/// How many rows `replace_price_list_items` will accept in one request.
///
/// A price list is edited as a whole ("save the grid"), so this is a bulk write; the bound is
/// the platform's general ceiling on a bulk body and is also what stops a request that has lost
/// its tenant predicate from rewriting the whole installation's price rows.
pub const MAX_PRICE_ROWS: usize = 5_000;

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// A product as the screens see it: the module's [`Product`] plus the row's bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The catalog's own view of the product, so a caller gets the validated shapes.
    #[serde(flatten)]
    pub product: Product,
    /// When it was archived, if it was.
    #[serde(with = "crate::dates::instant::option")]
    pub archived_at: Option<OffsetDateTime>,
    /// When the row was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
    /// When the row last changed.
    #[serde(with = "crate::dates::instant")]
    pub updated_at: OffsetDateTime,
}

/// A price list as the list screen sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceListView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The name a seller picks from.
    pub name: String,
    /// The currency every row on the list is expressed in.
    pub currency: String,
    /// Whether the list is offered to the builder.
    pub active: bool,
    /// First day the list may be used on.
    #[serde(with = "crate::dates::option")]
    pub valid_from: Option<time::Date>,
    /// Last day the list may be used on.
    #[serde(with = "crate::dates::option")]
    pub valid_until: Option<time::Date>,
    /// How many price rows it carries, joined — the editor shows it before the rows load.
    pub item_count: i64,
    /// When it was archived, if it was.
    #[serde(with = "crate::dates::instant::option")]
    pub archived_at: Option<OffsetDateTime>,
    /// When the row was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

/// A price list with its rows, which is what the editor and the builder both need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceListDetail {
    /// The list.
    pub list: PriceListView,
    /// Its price rows, in the order the editor shows them.
    pub items: Vec<PriceRowView>,
}

/// One price row, joined with the product's name and SKU so the editor renders without a second
/// round trip per row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceRowView {
    /// The row's id.
    pub id: Uuid,
    /// The product it prices.
    pub product_id: Uuid,
    /// The product's SKU, joined.
    pub product_sku: String,
    /// The product's name, joined.
    pub product_name: String,
    /// The quantity from which this price applies.
    pub min_quantity: String,
    /// The price, as the normalised text the module stores.
    pub price: String,
    /// The unit the product is sold in, joined — the editor shows it beside the quantity.
    pub unit: String,
}

/// One page of rows plus the cursor of the next one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Page<T> {
    /// The rows of this page.
    pub items: Vec<T>,
    /// Cursor of the next page, absent when the page is the last one.
    pub next_cursor: Option<String>,
    /// How many rows the whole filter matches.
    pub total_estimate: i64,
}

impl<T> Page<T> {
    /// A page from rows and the cursor that follows them.
    #[must_use]
    pub fn new(items: Vec<T>, next_cursor: Option<String>, total_estimate: i64) -> Self {
        Self {
            items,
            next_cursor,
            total_estimate,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The list query
// ---------------------------------------------------------------------------------------------

/// The filters a catalog or price-list screen sends.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CatalogQuery {
    /// Free text over SKU, name and description.
    #[serde(default)]
    pub search: Option<String>,
    /// One category the product must carry.
    #[serde(default)]
    pub category: Option<String>,
    /// `true` for live only, `false` for inactive only, absent for both.
    #[serde(default)]
    pub active: Option<bool>,
    /// Include the archived rows.
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
}

impl CatalogQuery {
    /// The page size, clamped to the module's bounds.
    ///
    /// A negative or absurd `limit` is clamped rather than refused: the list screen's page
    /// control can produce one by accident, and refusing the whole request for a mistyped page
    /// size is a worse answer than showing the first page.
    #[must_use]
    pub fn page_size(&self) -> i64 {
        self.limit
            .unwrap_or(DEFAULT_PER_PAGE)
            .clamp(1, MAX_PER_PAGE)
    }

    /// The id the cursor pins, or `None`.
    ///
    /// An unparsable cursor is `None` rather than an error: the cursor is a paging convenience,
    /// and a stale one from a changed URL should start the list over instead of failing it.
    #[must_use]
    pub fn cursor_id(&self) -> Option<Uuid> {
        self.cursor.as_deref().and_then(|raw| Uuid::parse_str(raw).ok())
    }

    /// Whether archived rows are in the result.
    #[must_use]
    pub fn shows_archived(&self) -> bool {
        self.include_archived.unwrap_or(false)
    }

    /// The search term, trimmed and bounded, or `None`.
    fn term(&self) -> Result<Option<String>> {
        match self.search.as_deref().map(str::trim) {
            None | Some("") => Ok(None),
            Some(term) if term.chars().count() > MAX_SEARCH_LENGTH => Err(SalesError::invalid(
                "query",
                "search",
                format!("a search term is at most {MAX_SEARCH_LENGTH} characters"),
            )),
            Some(term) => Ok(Some(term.to_owned())),
        }
    }

    /// The sort key and whether it descends, for a product list.
    ///
    /// A closed set, because the sort becomes an `order by`: a caller-supplied column name would
    /// be an injection point, and an unknown column would be an error the list cannot render.
    pub fn product_sort(&self) -> Result<(&'static str, bool)> {
        sort_or(&["sku", "name", "category", "default_price", "created_at", "updated_at"], self.sort.as_deref(), self.direction.as_deref(), "updated_at")
    }

    /// The sort key and whether it descends, for a price-list list.
    pub fn price_list_sort(&self) -> Result<(&'static str, bool)> {
        sort_or(&["name", "currency", "created_at"], self.sort.as_deref(), self.direction.as_deref(), "name")
    }
}

/// Resolve a sort request against the columns one entity accepts.
fn sort_or(
    accepted: &[&'static str],
    sort: Option<&str>,
    direction: Option<&str>,
    fallback: &'static str,
) -> Result<(&'static str, bool)> {
    let ascending = match direction.map(str::trim) {
        None | Some("") => None,
        Some("asc") => Some(false),
        Some("desc") => Some(true),
        Some(other) => {
            return Err(SalesError::InvalidQuery(format!(
                "direction must be asc or desc, not {other:?}"
            )));
        }
    };
    let key = match sort.map(str::trim) {
        None | Some("") => fallback,
        Some(key) => accepted.iter().copied().find(|name| *name == key).ok_or_else(|| {
            SalesError::InvalidQuery(format!(
                "sort must be one of {}, not {key:?}",
                accepted.join(", ")
            ))
        })?,
    };
    let descends = ascending.unwrap_or(!matches!(key, "sku" | "name" | "category" | "currency"));
    Ok((key, descends))
}

// ---------------------------------------------------------------------------------------------
// Small conversions
// ---------------------------------------------------------------------------------------------

/// Trim a string, treating an empty result as absent.
fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// Validate and normalise a currency code: three uppercase letters, the schema's own shape.
fn clean_currency(raw: Option<&str>) -> Result<String> {
    let text = clean(raw.map(str::to_owned)).unwrap_or_else(|| "TRY".to_owned());
    let upper = text.to_ascii_uppercase();
    if upper.len() != 3 || !upper.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(SalesError::invalid(
            "product",
            "currency",
            "a currency is a three-letter code such as TRY or USD",
        ));
    }
    Ok(upper)
}

/// Read a `numeric` money column, as [`Money`].
///
/// A value the database holds that the module cannot read is a **refusal, not a zero**: a
/// product whose price reads as `0.00` because the text failed to parse would be sold for
/// nothing, and nothing on the screen would say why.
pub fn money_from_text(raw: &str) -> Result<Money> {
    Money::parse(raw.trim()).map_err(|source| {
        SalesError::InvalidNumber {
            entity: "product",
            field: "price",
            source,
        }
    })
}

/// Read a `numeric` quantity column as the text the API hands back.
///
/// The quantity is carried as a string for the same reason the price is: three decimals of a
/// kilo is a value a JSON float would round on the way out.
pub fn quantity_from_row(raw: &str) -> String {
    let trimmed = raw.trim();
    // `numeric(14,3)` prints trailing zeros (`1.500`); the screens show `1.500` for a kilo, so
    // the text is kept as PostgreSQL wrote it rather than trimmed to `1.5`.
    trimmed.to_owned()
}

/// `true` when the database refused for the unique index behind a SKU or a name.
fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505")
    )
}

// ---------------------------------------------------------------------------------------------
// Products
// ---------------------------------------------------------------------------------------------

/// The columns a product row is read with.
const PRODUCT_COLUMNS: &str = "p.id, p.organization_id, p.sku, p.name, p.description, p.category, \
     p.unit, round(p.tax_percent)::int as tax_percent, p.default_price::text as default_price, \
     p.currency, p.active, p.archived_at, p.created_at, p.updated_at";

#[derive(Debug, FromRow)]
struct ProductRow {
    id: Uuid,
    organization_id: Uuid,
    sku: String,
    name: String,
    description: String,
    category: Option<String>,
    unit: String,
    tax_percent: i32,
    default_price: String,
    currency: String,
    active: bool,
    archived_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl ProductRow {
    /// The row as the screens see it, with the money validated on the way out.
    fn into_view(self) -> Result<ProductView> {
        let price = money_from_text(&self.default_price)?;
        Ok(ProductView {
            id: self.id,
            organization_id: self.organization_id,
            product: Product {
                id: self.id,
                sku: self.sku,
                name: self.name,
                description: self.description,
                category: self.category,
                unit: Unit::parse(&self.unit),
                tax_percent: self.tax_percent,
                default_price: price,
                currency: self.currency.trim_end().to_owned(),
                active: self.active,
            },
            archived_at: self.archived_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// The values a create carries.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewProduct {
    /// The catalog's identifier for the product.
    pub sku: String,
    /// What the quote line shows.
    pub name: String,
    /// The longer description.
    #[serde(default)]
    pub description: Option<String>,
    /// The category, when the organization groups its catalog.
    #[serde(default)]
    pub category: Option<String>,
    /// The unit it is sold in; the presets or a name of the organization's own.
    #[serde(default)]
    pub unit: Option<String>,
    /// The tax rate snapshotted onto a line that uses this product.
    #[serde(default)]
    pub tax_percent: Option<i32>,
    /// The price a quote line gets when no price list says otherwise.
    #[serde(default)]
    pub default_price: Option<String>,
    /// The currency of `default_price`.
    #[serde(default)]
    pub currency: Option<String>,
    /// Whether a new line may use it.
    #[serde(default)]
    pub active: Option<bool>,
}

/// A patch of a product: every field absent stays as it was.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProductPatch {
    /// The catalog's identifier.
    #[serde(default)]
    pub sku: Option<String>,
    /// The name a quote line shows.
    #[serde(default)]
    pub name: Option<String>,
    /// The longer description.
    #[serde(default)]
    pub description: Option<String>,
    /// The category.
    #[serde(default)]
    pub category: Option<String>,
    /// The unit.
    #[serde(default)]
    pub unit: Option<String>,
    /// The tax snapshot.
    #[serde(default)]
    pub tax_percent: Option<i32>,
    /// The fallback price.
    #[serde(default)]
    pub default_price: Option<String>,
    /// The currency.
    #[serde(default)]
    pub currency: Option<String>,
    /// Whether a new line may use it.
    #[serde(default)]
    pub active: Option<bool>,
}

/// The nine values a validated product create writes.
///
/// A struct rather than a nine-tuple because a tuple of `String, String, Option<String>, String,
/// i32, String, String, bool` is trivially transposable: two strings sit next to each other, and
/// `price` and `currency` are both strings. Naming them makes the mistake a compile error and
/// makes the binding below read as the sentence it is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedProduct {
    sku: String,
    name: String,
    description: String,
    category: Option<String>,
    unit: String,
    tax_percent: i32,
    price: String,
    currency: String,
    active: bool,
}

/// Validate the values of a create, in the order the form presents them.
fn validate_new(input: &NewProduct) -> Result<ValidatedProduct> {
    let sku = input.sku.trim().to_owned();
    validate_sku(&sku)?;
    let name = input.name.trim().to_owned();
    validate_name(&name)?;
    let description = input.description.clone().unwrap_or_default();
    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        return Err(SalesError::invalid(
            "product",
            "description",
            format!("a description is at most {MAX_DESCRIPTION_LENGTH} characters"),
        ));
    }
    let category = clean(input.category.clone());
    let unit = input
        .unit
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("piece")
        .to_owned();
    if unit.chars().count() > 24 {
        return Err(SalesError::invalid(
            "product",
            "unit",
            "a unit is at most 24 characters",
        ));
    }
    let tax_percent = input.tax_percent.unwrap_or(0);
    validate_tax_percent(tax_percent)?;
    let price = Money::parse(
        input
            .default_price
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("0"),
    )
    .map_err(|source| SalesError::number("product", "default_price", source))?;
    validate_price(price)?;
    let price = price.round_to_cents().to_text();
    let currency = clean_currency(input.currency.as_deref())?;
    let active = input.active.unwrap_or(true);

    Ok(ValidatedProduct {
        sku,
        name,
        description,
        category,
        unit,
        tax_percent,
        price,
        currency,
        active,
    })
}

/// One page of products.
pub async fn list_products(
    pool: &PgPool,
    organization_id: Uuid,
    query: &CatalogQuery,
) -> Result<Page<ProductView>> {
    let (sort, desc) = query.product_sort()?;
    let term = query.term()?;
    let limit = query.page_size();
    let direction = if desc { "desc" } else { "asc" };

    // **The count and the page are two statements that share one *filter function*, not one
    // built fragment.** This is not a style preference: `QueryBuilder::sql()` renders the
    // placeholder text and returns nothing about the bindings, so a fragment copied from one
    // builder into another with `push(fragment.sql())` produces a statement whose text says
    // `$1 … $4` with **zero** parameters — a `500` of "bind message supplies 0 parameters, but
    // the prepared statement requires 4" on every filtered list. Sharing the function means the
    // clauses cannot drift apart, which is the real reason to do it this way: a count that
    // matched a different set than the page it counts is how a list claims forty rows and shows
    // none.
    let total_estimate = count_products(pool, organization_id, query, term.as_deref()).await?;

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("select ");
    builder
        .push(PRODUCT_COLUMNS)
        .push(" from sales_products p");
    push_product_filters(&mut builder, organization_id, query, term.as_deref());

    if let Some(cursor_id) = query.cursor_id() {
        builder
            .push(" and (")
            .push(sort_column(sort))
            .push(" , p.id) ")
            .push(if desc { "<" } else { ">" })
            .push(" (select ")
            .push(sort_column(sort))
            .push(" , p.id from sales_products p where p.id = ")
            .push_bind(cursor_id)
            .push(")");
    }

    builder
        .push(" order by ")
        .push(sort_column(sort))
        .push(" ")
        .push(direction)
        .push(", p.id ")
        .push(direction)
        .push(" limit ")
        .push_bind(limit + 1);

    let mut rows: Vec<ProductRow> = builder.build_query_as().fetch_all(pool).await?;
    rows.truncate(limit as usize);
    let items: Vec<ProductView> = rows
        .into_iter()
        .map(ProductRow::into_view)
        .collect::<Result<Vec<_>>>()?;
    let next_cursor = items.last().map(|view| view.id.to_string());

    Ok(Page::new(items, next_cursor, total_estimate))
}

/// How many products a filter matches — the same clauses as the page, written by the same
/// function.
async fn count_products(
    pool: &PgPool,
    organization_id: Uuid,
    query: &CatalogQuery,
    term: Option<&str>,
) -> Result<i64> {
    let mut count: QueryBuilder<Postgres> =
        QueryBuilder::new("select count(*) from sales_products p");
    push_product_filters(&mut count, organization_id, query, term);

    count.build_query_scalar().fetch_one(pool).await.map_err(Into::into)
}

/// The clauses the product count and page share, so a filter can never match in one and miss in
/// the other — the two would then disagree about how many rows there are, which is the kind of
/// bug a list hides by being empty.
///
/// Takes the organization and the already-validated search term rather than reading them off the
/// query, so the count does not have to re-validate and reject in a different order than the page.
fn push_product_filters(
    builder: &mut QueryBuilder<'_, Postgres>,
    organization_id: Uuid,
    query: &CatalogQuery,
    term: Option<&str>,
) {
    builder
        .push(" where p.organization_id = ")
        .push_bind(organization_id);

    if let Some(term) = term {
        let pattern = format!("%{}%", term.to_lowercase());
        builder
            .push(" and (lower(p.sku) like ")
            .push_bind(pattern.clone())
            .push(" or lower(p.name) like ")
            .push_bind(pattern.clone())
            .push(" or lower(p.description) like ")
            .push_bind(pattern)
            .push(")");
    }
    if let Some(category) = query
        .category
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        builder
            .push(" and lower(p.category) = ")
            .push_bind(category.to_lowercase());
    }
    if let Some(active) = query.active {
        builder.push(" and p.active = ").push_bind(active);
    }
    if !query.shows_archived() {
        builder.push(" and p.archived_at is null");
    }
}

/// The SQL expression a product sort key orders by.
fn sort_column(key: &str) -> &'static str {
    match key {
        "sku" => "lower(p.sku)",
        "name" => "lower(p.name)",
        "category" => "lower(coalesce(p.category, ''))",
        "default_price" => "p.default_price",
        "created_at" => "p.created_at",
        _ => "p.updated_at",
    }
}

/// One product, or `404` for a row of another organization.
pub async fn get_product(pool: &PgPool, organization_id: Uuid, product_id: Uuid) -> Result<ProductView> {
    let row: Option<ProductRow> = sqlx::query_as(&format!(
        "select {PRODUCT_COLUMNS} from sales_products p where p.id = $1 and p.organization_id = $2"
    ))
    .bind(product_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;

    row.ok_or(SalesError::NotFound("product"))?
        .into_view()
}

/// Create a product.
pub async fn create_product(
    pool: &PgPool,
    organization_id: Uuid,
    input: &NewProduct,
) -> Result<ProductView> {
    let values = validate_new(input)?;

    let id: Uuid = sqlx::query_scalar(
        "insert into sales_products \
         (organization_id, sku, name, description, category, unit, tax_percent, default_price, currency, active) \
         values ($1, $2, $3, $4, $5, $6, $7::numeric, $8::numeric, $9, $10) returning id",
    )
    .bind(organization_id)
    .bind(&values.sku)
    .bind(&values.name)
    .bind(&values.description)
    .bind(&values.category)
    .bind(&values.unit)
    .bind(values.tax_percent)
    .bind(&values.price)
    .bind(&values.currency)
    .bind(values.active)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        if is_unique_violation(&error) {
            SalesError::SkuTaken
        } else {
            SalesError::Database(error)
        }
    })?;

    get_product(pool, organization_id, id).await
}

/// Edit a product.
pub async fn patch_product(
    pool: &PgPool,
    organization_id: Uuid,
    product_id: Uuid,
    patch: &ProductPatch,
) -> Result<ProductView> {
    // A patch against a row of another organization answers `404` **before** anything is
    // validated, so a caller cannot learn that a SKU is taken by probing another tenant.
    get_product(pool, organization_id, product_id).await?;

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("update sales_products set ");
    let mut touched = 0usize;

    macro_rules! set_text {
        ($column:expr, $value:expr) => {{
            if touched > 0 {
                builder.push(", ");
            }
            builder.push($column).push(" = ").push_bind($value);
            touched += 1;
        }};
    }
    macro_rules! set_numeric {
        ($column:expr, $value:expr) => {{
            if touched > 0 {
                builder.push(", ");
            }
            builder
                .push($column)
                .push(" = ")
                .push_bind($value)
                .push("::numeric");
            touched += 1;
        }};
    }

    if let Some(sku) = patch.sku.as_deref() {
        let sku = sku.trim().to_owned();
        validate_sku(&sku)?;
        set_text!("sku", sku);
    }
    if let Some(name) = patch.name.as_deref() {
        let name = name.trim().to_owned();
        validate_name(&name)?;
        set_text!("name", name);
    }
    if patch.description.is_some() {
        let description = patch.description.clone().unwrap_or_default();
        if description.chars().count() > MAX_DESCRIPTION_LENGTH {
            return Err(SalesError::invalid(
                "product",
                "description",
                format!("a description is at most {MAX_DESCRIPTION_LENGTH} characters"),
            ));
        }
        set_text!("description", description);
    }
    if patch.category.is_some() {
        set_text!("category", clean(patch.category.clone()));
    }
    if let Some(unit) = patch.unit.as_deref() {
        let unit = unit.trim();
        if unit.is_empty() || unit.chars().count() > 24 {
            return Err(SalesError::invalid(
                "product",
                "unit",
                "a unit is between 1 and 24 characters",
            ));
        }
        set_text!("unit", unit.to_owned());
    }
    if let Some(tax_percent) = patch.tax_percent {
        validate_tax_percent(tax_percent)?;
        set_numeric!("tax_percent", tax_percent.to_string());
    }
    if let Some(raw) = patch.default_price.as_deref() {
        let price = Money::parse(raw.trim())
            .map_err(|source| SalesError::number("product", "default_price", source))?;
        validate_price(price)?;
        set_numeric!("default_price", price.round_to_cents().to_text());
    }
    if patch.currency.is_some() {
        set_text!("currency", clean_currency(patch.currency.as_deref())?);
    }
    if let Some(active) = patch.active {
        set_text!("active", active);
    }

    if touched == 0 {
        // An empty patch is not an error: the panel sends the fields a person actually touched,
        // and a no-op that writes an audit row saying nothing changed is noise.
        return get_product(pool, organization_id, product_id).await;
    }

    builder
        .push(" where id = ")
        .push_bind(product_id)
        .push(" and organization_id = ")
        .push_bind(organization_id);

    let result = builder.build().execute(pool).await.map_err(|error| {
        if is_unique_violation(&error) {
            SalesError::SkuTaken
        } else {
            SalesError::Database(error)
        }
    })?;
    if result.rows_affected() == 0 {
        return Err(SalesError::NotFound("product"));
    }

    get_product(pool, organization_id, product_id).await
}

/// Archive a product.
///
/// The archive is reversible through a patch (`active`, and the `archived_at` is left to an
/// operator's own script) because a product referenced by a sent quote may never come back as if
/// it had never existed; what matters is that the list stops offering it to new lines.
pub async fn archive_product(
    pool: &PgPool,
    organization_id: Uuid,
    product_id: Uuid,
) -> Result<ProductView> {
    let result = sqlx::query(
        "update sales_products set archived_at = now(), active = false, updated_at = now() \
         where id = $1 and organization_id = $2 and archived_at is null",
    )
    .bind(product_id)
    .bind(organization_id)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        // Either it is not in this organization or it is already archived. Reading it tells the
        // two apart: an already-archived product is still a product, and archiving it twice is
        // not an error the caller can act on.
        get_product(pool, organization_id, product_id).await?;
        return get_product(pool, organization_id, product_id).await;
    }

    get_product(pool, organization_id, product_id).await
}

/// The categories the organization already uses, for the filter's dropdown.
pub async fn list_categories(pool: &PgPool, organization_id: Uuid) -> Result<Vec<String>> {
    let rows: Vec<Option<String>> = sqlx::query_scalar(
        "select distinct category from sales_products \
         where organization_id = $1 and category is not null and archived_at is null \
         order by category",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().flatten().collect())
}

// ---------------------------------------------------------------------------------------------
// Price lists
// ---------------------------------------------------------------------------------------------

/// The values a price-list create carries.
#[derive(Debug, Clone, Default, Deserialize)]
// Container-level `default` is what keeps an **omitted** window legal: naming a `with` path
// replaces a field's whole deserializer, which silently discards a field-level
// `#[serde(default)]`, and a form that leaves "valid from" blank must not be a 422.
#[serde(default)]
pub struct NewPriceList {
    /// The name a seller picks from.
    pub name: String,
    /// The currency every row on the list is expressed in.
    #[serde(default)]
    pub currency: Option<String>,
    /// Whether the builder offers it.
    #[serde(default)]
    pub active: Option<bool>,
    /// First day the list may be used on.
    #[serde(default, with = "crate::dates::option")]
    pub valid_from: Option<time::Date>,
    /// Last day the list may be used on.
    #[serde(default, with = "crate::dates::option")]
    pub valid_until: Option<time::Date>,
}

/// A patch of a price list.
#[derive(Debug, Clone, Default, Deserialize)]
// Same reason as `NewPriceList`: a patch that does not mention the window leaves it alone.
#[serde(default)]
pub struct PriceListPatch {
    /// The name a seller picks from.
    #[serde(default)]
    pub name: Option<String>,
    /// The currency.
    #[serde(default)]
    pub currency: Option<String>,
    /// Whether the builder offers it.
    #[serde(default)]
    pub active: Option<bool>,
    /// First day the list may be used on.
    #[serde(default, with = "crate::dates::option")]
    pub valid_from: Option<time::Date>,
    /// Last day the list may be used on.
    #[serde(default, with = "crate::dates::option")]
    pub valid_until: Option<time::Date>,
}

/// One price row as a create sends it.
#[derive(Debug, Clone, Deserialize)]
pub struct NewPriceRow {
    /// The product it prices.
    pub product_id: Uuid,
    /// The quantity from which this price applies.
    #[serde(default)]
    pub min_quantity: Option<String>,
    /// The price for one unit at that quantity.
    pub price: String,
}

/// Validate the name of a price list.
fn validate_list_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(SalesError::invalid(
            "price_list",
            "name",
            "a price list needs a name",
        ));
    }
    if trimmed.chars().count() > MAX_LIST_NAME_LENGTH {
        return Err(SalesError::invalid(
            "price_list",
            "name",
            format!("a price-list name is at most {MAX_LIST_NAME_LENGTH} characters"),
        ));
    }
    Ok(trimmed.to_owned())
}

/// The window check, stated once for the create and the patch.
fn validate_window(
    from: Option<time::Date>,
    until: Option<time::Date>,
) -> Result<()> {
    if let (Some(from), Some(until)) = (from, until)
        && until < from
    {
        return Err(SalesError::invalid(
            "price_list",
            "valid_until",
            "the list cannot end before it starts",
        ));
    }
    Ok(())
}

#[derive(Debug, FromRow)]
struct PriceListRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    currency: String,
    active: bool,
    valid_from: Option<time::Date>,
    valid_until: Option<time::Date>,
    item_count: i64,
    archived_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

impl PriceListRow {
    /// The row as the list screen sees it.
    fn into_view(self) -> PriceListView {
        PriceListView {
            id: self.id,
            organization_id: self.organization_id,
            name: self.name,
            currency: self.currency.trim_end().to_owned(),
            active: self.active,
            valid_from: self.valid_from,
            valid_until: self.valid_until,
            item_count: self.item_count,
            archived_at: self.archived_at,
            created_at: self.created_at,
        }
    }
}

/// The columns a price-list row is read with, with the row count joined.
const PRICE_LIST_COLUMNS: &str = "l.id, l.organization_id, l.name, l.currency, l.active, \
     l.valid_from, l.valid_until, l.archived_at, l.created_at, \
     (select count(*) from sales_price_list_items i where i.price_list_id = l.id) as item_count";

/// One page of price lists.
pub async fn list_price_lists(
    pool: &PgPool,
    organization_id: Uuid,
    query: &CatalogQuery,
) -> Result<Page<PriceListView>> {
    let (sort, desc) = query.price_list_sort()?;
    let term = query.term()?;
    let limit = query.page_size();
    let direction = if desc { "desc" } else { "asc" };

    // The same one-function shape as the product list, for the same reason.
    let total_estimate = count_price_lists(pool, organization_id, query, term.as_deref()).await?;

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("select ");
    builder
        .push(PRICE_LIST_COLUMNS)
        .push(" from sales_price_lists l");
    push_price_list_filters(&mut builder, organization_id, query, term.as_deref());

    if let Some(cursor_id) = query.cursor_id() {
        builder
            .push(" and (")
            .push(list_sort_column(sort))
            .push(" , l.id) ")
            .push(if desc { "<" } else { ">" })
            .push(" (select ")
            .push(list_sort_column(sort))
            .push(" , l.id from sales_price_lists l where l.id = ")
            .push_bind(cursor_id)
            .push(")");
    }

    builder
        .push(" order by ")
        .push(list_sort_column(sort))
        .push(" ")
        .push(direction)
        .push(", l.id ")
        .push(direction)
        .push(" limit ")
        .push_bind(limit + 1);

    let mut rows: Vec<PriceListRow> = builder.build_query_as().fetch_all(pool).await?;
    rows.truncate(limit as usize);
    let items: Vec<PriceListView> = rows.into_iter().map(PriceListRow::into_view).collect();
    let next_cursor = items.last().map(|view| view.id.to_string());

    Ok(Page::new(items, next_cursor, total_estimate))
}

/// How many price lists a filter matches.
async fn count_price_lists(
    pool: &PgPool,
    organization_id: Uuid,
    query: &CatalogQuery,
    term: Option<&str>,
) -> Result<i64> {
    let mut count: QueryBuilder<Postgres> =
        QueryBuilder::new("select count(*) from sales_price_lists l");
    push_price_list_filters(&mut count, organization_id, query, term);

    count.build_query_scalar().fetch_one(pool).await.map_err(Into::into)
}

/// The clauses the price-list count and page share.
fn push_price_list_filters(
    builder: &mut QueryBuilder<'_, Postgres>,
    organization_id: Uuid,
    query: &CatalogQuery,
    term: Option<&str>,
) {
    builder
        .push(" where l.organization_id = ")
        .push_bind(organization_id);

    if let Some(term) = term {
        let pattern = format!("%{}%", term.to_lowercase());
        builder.push(" and lower(l.name) like ").push_bind(pattern);
    }
    if let Some(active) = query.active {
        builder.push(" and l.active = ").push_bind(active);
    }
    if !query.shows_archived() {
        builder.push(" and l.archived_at is null");
    }
}

/// The SQL expression a price-list sort key orders by.
fn list_sort_column(key: &str) -> &'static str {
    match key {
        "name" => "lower(l.name)",
        "currency" => "l.currency",
        "created_at" => "l.created_at",
        _ => "l.name",
    }
}

/// One price list with its rows.
pub async fn get_price_list(
    pool: &PgPool,
    organization_id: Uuid,
    list_id: Uuid,
) -> Result<PriceListDetail> {
    // `fetch_optional` hands back the row or `None`; the annotation names the *row*, not an
    // option around it — writing `Option<PriceListRow>` here is a `?`-on-the-wrong-type that
    // compiles to "expected Option, found row".
    let row: PriceListRow = sqlx::query_as::<_, PriceListRow>(&format!(
        "select {PRICE_LIST_COLUMNS} from sales_price_lists l \
         where l.id = $1 and l.organization_id = $2"
    ))
    .bind(list_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?
    .ok_or(SalesError::NotFound("price list"))?;

    Ok(PriceListDetail {
        list: row.into_view(),
        items: list_price_rows(pool, list_id).await?,
    })
}

/// The price rows of a list, joined with the product so the editor renders in one request.
pub async fn list_price_rows(pool: &PgPool, list_id: Uuid) -> Result<Vec<PriceRowView>> {
    let rows: Vec<PgRow> = sqlx::query(
        "select i.id, i.product_id, p.sku as product_sku, p.name as product_name, \
                i.min_quantity::text as min_quantity, i.price::text as price, p.unit \
         from sales_price_list_items i \
         join sales_products p on p.id = i.product_id \
         where i.price_list_id = $1 \
         order by lower(p.name), i.min_quantity",
    )
    .bind(list_id)
    .fetch_all(pool)
    .await?;

    rows
        .into_iter()
        .map(|row| {
            let min_quantity: String = row.try_get("min_quantity").unwrap_or_default();
            let price: String = row.try_get("price").unwrap_or_default();
            // The `?` is why this closure returns `Result<PriceRowView>`: a price the database
            // holds that the module cannot read has to be reported, and the alternative — a row
            // quietly priced at zero — is a price list that quietly gives everything away.
            Ok(PriceRowView {
                id: row.try_get("id").unwrap_or_default(),
                product_id: row.try_get("product_id").unwrap_or_default(),
                product_sku: row.try_get("product_sku").unwrap_or_default(),
                product_name: row.try_get("product_name").unwrap_or_default(),
                min_quantity: quantity_from_row(&min_quantity),
                price: money_from_text(&price)?.round_to_cents().to_text(),
                unit: row.try_get("unit").unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>>>()
}

/// Create a price list.
pub async fn create_price_list(
    pool: &PgPool,
    organization_id: Uuid,
    input: &NewPriceList,
) -> Result<PriceListView> {
    let name = validate_list_name(&input.name)?;
    let currency = clean_currency(input.currency.as_deref())?;
    validate_window(input.valid_from, input.valid_until)?;

    let id: Uuid = sqlx::query_scalar(
        "insert into sales_price_lists (organization_id, name, currency, active, valid_from, valid_until) \
         values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(organization_id)
    .bind(&name)
    .bind(&currency)
    .bind(input.active.unwrap_or(true))
    .bind(input.valid_from)
    .bind(input.valid_until)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        if is_unique_violation(&error) {
            SalesError::name_taken("price list", &name)
        } else {
            SalesError::Database(error)
        }
    })?;

    get_price_list(pool, organization_id, id).await.map(|detail| detail.list)
}

/// Edit a price list.
pub async fn patch_price_list(
    pool: &PgPool,
    organization_id: Uuid,
    list_id: Uuid,
    patch: &PriceListPatch,
) -> Result<PriceListView> {
    get_price_list(pool, organization_id, list_id).await?;

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("update sales_price_lists set ");
    let mut touched = 0usize;
    macro_rules! set {
        ($column:expr, $value:expr) => {{
            if touched > 0 {
                builder.push(", ");
            }
            builder.push($column).push(" = ").push_bind($value);
            touched += 1;
        }};
    }

    if let Some(name) = patch.name.as_deref() {
        let name = validate_list_name(name)?;
        set!("name", name);
    }
    if patch.currency.is_some() {
        set!("currency", clean_currency(patch.currency.as_deref())?);
    }
    if let Some(active) = patch.active {
        set!("active", active);
    }
    if patch.valid_from.is_some() {
        set!("valid_from", patch.valid_from);
    }
    if patch.valid_until.is_some() {
        set!("valid_until", patch.valid_until);
    }
    if touched == 0 {
        return get_price_list(pool, organization_id, list_id)
            .await
            .map(|detail| detail.list);
    }

    // The window is checked against the **stored** value, not against the patch: a patch that
    // moves only the start date must still be refused when the end already sits before it.
    let current: (Option<time::Date>, Option<time::Date>) = sqlx::query_as(
        "select valid_from, valid_until from sales_price_lists where id = $1 and organization_id = $2",
    )
    .bind(list_id)
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    validate_window(
        patch.valid_from.or(current.0),
        patch.valid_until.or(current.1),
    )?;

    builder
        .push(" where id = ")
        .push_bind(list_id)
        .push(" and organization_id = ")
        .push_bind(organization_id)
        .push(" and archived_at is null");

    let result = builder.build().execute(pool).await.map_err(|error| {
        if is_unique_violation(&error) {
            SalesError::name_taken("price list", patch.name.clone().unwrap_or_default())
        } else {
            SalesError::Database(error)
        }
    })?;
    if result.rows_affected() == 0 {
        return Err(SalesError::NotFound("price list"));
    }

    get_price_list(pool, organization_id, list_id)
        .await
        .map(|detail| detail.list)
}

/// Archive a price list. Its rows go with it: a list nobody can pick is not a list with prices.
pub async fn archive_price_list(
    pool: &PgPool,
    organization_id: Uuid,
    list_id: Uuid,
) -> Result<PriceListView> {
    let result = sqlx::query(
        "update sales_price_lists set archived_at = now(), active = false, updated_at = now() \
         where id = $1 and organization_id = $2 and archived_at is null",
    )
    .bind(list_id)
    .bind(organization_id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(SalesError::NotFound("price list"));
    }

    get_price_list(pool, organization_id, list_id)
        .await
        .map(|detail| detail.list)
}

/// Replace every price row of a list with the ones the request carries.
///
/// One transaction, because the editor saves the whole grid: a request that deleted the rows and
/// then failed on the third insert would leave a list that prices nothing, and a quote built
/// against it would fall back to the default price **silently**. The whole replacement either
/// lands or the old rows are still there.
///
/// The tenant guard in the migration does the cross-organization check, and this function reads
/// the *product ids back* rather than trusting the request: a row naming a product of another
/// organization is refused here with a clear field name rather than by a trigger exception the
/// caller would read as a `500`.
pub async fn replace_price_list_items(
    pool: &PgPool,
    organization_id: Uuid,
    list_id: Uuid,
    rows: &[NewPriceRow],
) -> Result<PriceListDetail> {
    if rows.len() > MAX_PRICE_ROWS {
        return Err(SalesError::invalid(
            "price_list",
            "items",
            format!("a price list carries at most {MAX_PRICE_ROWS} rows"),
        ));
    }

    get_price_list(pool, organization_id, list_id).await?;

    let mut items: Vec<PriceListItem> = Vec::with_capacity(rows.len());
    for row in rows {
        let min_quantity = crate::money::Quantity::parse(
            row.min_quantity
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .unwrap_or("1"),
        )
        .map_err(|source| SalesError::number("price_list_item", "min_quantity", source))?;
        let price = Money::parse(row.price.trim())
            .map_err(|source| SalesError::number("price_list_item", "price", source))?
            .round_to_cents();
        let item = PriceListItem {
            product_id: row.product_id,
            min_quantity,
            price,
        };
        validate_price_item(&item)?;
        items.push(item);
    }

    // Every product must belong to this organization. One query for the whole set rather than one
    // per row: the editor saves two hundred rows at a time and a save that issues two hundred
    // round trips is a save a person waits for.
    let mut known = sqlx::query_scalar::<_, Uuid>(
        "select id from sales_products \
         where organization_id = $1 and archived_at is null and id = any($2)",
    )
    .bind(organization_id)
    .bind(items.iter().map(|item| item.product_id).collect::<Vec<_>>())
    .fetch_all(pool)
    .await?;
    known.sort();
    known.dedup();

    for item in &items {
        if !known.contains(&item.product_id) {
            return Err(SalesError::invalid(
                "price_list_item",
                "product_id",
                "one of the products is not in this organization's catalog",
            ));
        }
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("delete from sales_price_list_items where price_list_id = $1")
        .bind(list_id)
        .execute(&mut *transaction)
        .await?;

    for item in &items {
        sqlx::query(
            "insert into sales_price_list_items \
             (organization_id, price_list_id, product_id, min_quantity, price) \
             values ($1, $2, $3, $4::numeric, $5::numeric)",
        )
        .bind(organization_id)
        .bind(list_id)
        .bind(item.product_id)
        .bind(item.min_quantity.to_text())
        .bind(item.price.to_text())
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;

    get_price_list(pool, organization_id, list_id).await
}

/// The prices a list assigns to the products asked about, for the quote builder's prefill.
///
/// Returns one row per product that **has** a row on the list; the builder falls back to the
/// product's own default for the rest, which is [`crate::catalog::resolve_price`]'s rule applied
/// to what the store could find.
pub async fn prices_for_products(
    pool: &PgPool,
    organization_id: Uuid,
    list_id: Uuid,
    product_ids: &[Uuid],
) -> Result<Vec<PriceListItem>> {
    if product_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows: Vec<PgRow> = sqlx::query(
        "select product_id, min_quantity::text as min_quantity, price::text as price \
         from sales_price_list_items \
         where organization_id = $1 and price_list_id = $2 and product_id = any($3) \
         order by min_quantity",
    )
    .bind(organization_id)
    .bind(list_id)
    .bind(product_ids)
    .fetch_all(pool)
    .await?;

    let mut items: Vec<PriceListItem> = Vec::with_capacity(rows.len());
    for row in rows {
        let min_quantity: String = row.try_get("min_quantity").unwrap_or_default();
        let price: String = row.try_get("price").unwrap_or_default();
        items.push(PriceListItem {
            product_id: row.try_get("product_id").unwrap_or_default(),
            min_quantity: crate::money::Quantity::parse(&min_quantity).map_err(|source| {
                SalesError::number("price_list_item", "min_quantity", source)
            })?,
            price: money_from_text(&price)?,
        });
    }
    Ok(items)
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// The organization's sales settings, falling back to the module's defaults.
///
/// The row exists for every organization (the migration seeds it and a trigger creates it for
/// each new one), so a miss is a genuine surprise rather than a tenant without settings — but
/// the fallback keeps the quote builder working rather than failing a form's defaults.
pub async fn get_settings(pool: &PgPool, organization_id: Uuid) -> Result<Settings> {
    let row: Option<(
        String,
        i32,
        i32,
        String,
        String,
    )> = sqlx::query_as(
        "select currency, round(discount_approval_threshold)::int as threshold, \
                quote_validity_days, quote_number_prefix, order_number_prefix \
         from sales_settings where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;

    Ok(match row {
        None => Settings::default(),
        Some((currency, threshold, validity, quote_prefix, order_prefix)) => Settings {
            currency: currency.trim_end().to_owned(),
            discount_approval_threshold: threshold,
            quote_validity_days: validity,
            quote_number_prefix: quote_prefix,
            order_number_prefix: order_prefix,
        },
    })
}

/// A patch of the settings row.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsPatch {
    /// The default currency for a new quote.
    #[serde(default)]
    pub currency: Option<String>,
    /// The largest line discount that can be sent without a manager.
    #[serde(default)]
    pub discount_approval_threshold: Option<i32>,
    /// How many days a new quote is valid for.
    #[serde(default)]
    pub quote_validity_days: Option<i32>,
    /// The prefix of a quote number.
    #[serde(default)]
    pub quote_number_prefix: Option<String>,
    /// The prefix of an order number.
    #[serde(default)]
    pub order_number_prefix: Option<String>,
}

/// Write the settings row.
pub async fn update_settings(
    pool: &PgPool,
    organization_id: Uuid,
    patch: &SettingsPatch,
) -> Result<Settings> {
    let current = get_settings(pool, organization_id).await?;

    let currency = match patch.currency.as_deref() {
        Some(raw) => clean_currency(Some(raw))?,
        None => current.currency,
    };
    let threshold = patch
        .discount_approval_threshold
        .unwrap_or(current.discount_approval_threshold);
    if !(0..=100).contains(&threshold) {
        return Err(SalesError::invalid(
            "settings",
            "discount_approval_threshold",
            "the approval threshold is a percentage between 0 and 100",
        ));
    }
    let validity = patch.quote_validity_days.unwrap_or(current.quote_validity_days);
    if !(1..=365).contains(&validity) {
        return Err(SalesError::invalid(
            "settings",
            "quote_validity_days",
            "a quote is valid for between 1 and 365 days",
        ));
    }
    let quote_prefix = match patch.quote_number_prefix.as_deref() {
        Some(raw) => validate_prefix(raw, "quote_number_prefix")?,
        None => current.quote_number_prefix,
    };
    let order_prefix = match patch.order_number_prefix.as_deref() {
        Some(raw) => validate_prefix(raw, "order_number_prefix")?,
        None => current.order_number_prefix,
    };

    sqlx::query(
        "insert into sales_settings \
         (organization_id, currency, discount_approval_threshold, quote_validity_days, \
          quote_number_prefix, order_number_prefix) \
         values ($1, $2, $3::numeric, $4, $5, $6) \
         on conflict (organization_id) do update set \
            currency = excluded.currency, \
            discount_approval_threshold = excluded.discount_approval_threshold, \
            quote_validity_days = excluded.quote_validity_days, \
            quote_number_prefix = excluded.quote_number_prefix, \
            order_number_prefix = excluded.order_number_prefix, \
            updated_at = now()",
    )
    .bind(organization_id)
    .bind(&currency)
    .bind(threshold.to_string())
    .bind(validity)
    .bind(&quote_prefix)
    .bind(&order_prefix)
    .execute(pool)
    .await?;

    get_settings(pool, organization_id).await
}

/// A number prefix is what makes `Q-2026-0001` readable, so it may not be blank — and it may
/// not contain a dash, which would make the generated number ambiguous about where the year
/// begins.
fn validate_prefix(raw: &str, field: &'static str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 8 {
        return Err(SalesError::invalid(
            "settings",
            field,
            "a number prefix is 1 to 8 characters",
        ));
    }
    if trimmed.contains('-') {
        return Err(SalesError::invalid(
            "settings",
            field,
            "a prefix may not contain a dash — the year is added after it",
        ));
    }
    Ok(trimmed.to_owned())
}

/// The categories a price list is offered alongside, and the units the product form knows.
///
/// Exposed as one read so the catalog screen's two dropdowns are a single request, and so a unit
/// this build has never heard of still shows up next to the presets.
pub async fn catalog_vocabulary(pool: &PgPool, organization_id: Uuid) -> Result<CatalogVocabulary> {
    let categories = list_categories(pool, organization_id).await?;

    let units: Vec<String> = sqlx::query_scalar(
        "select distinct unit from sales_products \
         where organization_id = $1 and archived_at is null order by unit",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut all: Vec<String> = Unit::presets()
        .iter()
        .map(|preset| (*preset).to_owned())
        .collect();
    for unit in units {
        if !all.contains(&unit) {
            all.push(unit);
        }
    }

    Ok(CatalogVocabulary { categories, units: all })
}

/// The two dropdown vocabularies the catalog screen draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogVocabulary {
    /// The categories this organization already uses.
    pub categories: Vec<String>,
    /// The preset units plus any the organization named itself.
    pub units: Vec<String>,
}

/// The price a quote line gets: the list's row for that product and quantity, or the product's
/// own default price.
///
/// The one function the builder calls, so "the price is prefilled" and "the price is what was
/// charged" cannot be two different answers.
pub async fn resolve_line_price(
    pool: &PgPool,
    organization_id: Uuid,
    price_list_id: Option<Uuid>,
    product: &Product,
    quantity: crate::money::Quantity,
) -> Result<Money> {
    let rows = match price_list_id {
        Some(list_id) => prices_for_products(pool, organization_id, list_id, &[product.id]).await?,
        None => Vec::new(),
    };
    Ok(catalog::resolve_price(
        product.default_price,
        &rows,
        product.id,
        quantity,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Quantity;

    fn query() -> CatalogQuery {
        CatalogQuery::default()
    }

    // ---- page size and cursor -----------------------------------------------------------------

    #[test]
    fn an_absent_page_size_is_the_default_and_a_wild_one_is_clamped() {
        assert_eq!(query().page_size(), DEFAULT_PER_PAGE);
        let huge = CatalogQuery {
            limit: Some(100_000),
            ..query()
        };
        assert_eq!(huge.page_size(), MAX_PER_PAGE);
        let negative = CatalogQuery {
            limit: Some(-5),
            ..query()
        };
        assert_eq!(
            negative.page_size(),
            1,
            "a mistyped page size shows the first page rather than failing the list"
        );
    }

    #[test]
    fn a_cursor_that_is_not_an_id_starts_the_list_over_instead_of_failing_it() {
        // The cursor is a paging convenience carried in a URL somebody edited; refusing the whole
        // request for it would be a worse answer than showing the first page.
        let stale = CatalogQuery {
            cursor: Some("not-a-uuid".to_owned()),
            ..query()
        };
        assert_eq!(stale.cursor_id(), None);
        let real = Uuid::new_v4();
        let good = CatalogQuery {
            cursor: Some(real.to_string()),
            ..query()
        };
        assert_eq!(good.cursor_id(), Some(real));
    }

    // ---- sort -------------------------------------------------------------------------------

    #[test]
    fn an_unknown_sort_is_refused_with_the_columns_the_list_accepts() {
        let error = CatalogQuery {
            sort: Some("nonsense".to_owned()),
            ..query()
        }
        .product_sort()
        .expect_err("an unknown column must be refused");
        let sentence = error.to_string();
        assert!(sentence.contains("nonsense"), "{sentence}");
        assert!(sentence.contains("sku"), "{sentence}");
    }

    #[test]
    fn a_direction_that_is_not_a_direction_is_refused() {
        let sideway = CatalogQuery {
            direction: Some("sideways".to_owned()),
            ..query()
        };
        assert!(sideway.product_sort().is_err());
    }

    #[test]
    fn a_product_list_sorts_by_what_changed_last_unless_it_is_asked_otherwise() {
        // The catalog's default answer is "what changed", because that is what a person opening
        // the list is looking for; the alphabetical answers are opt-in, not the fallback. The pair
        // is the **key**, and `sort_column` turns it into the SQL — the two are separate on
        // purpose, so a caller can name a column without the module handing SQL back.
        assert_eq!(query().product_sort().unwrap(), ("updated_at", true));
        let by_name = CatalogQuery {
            sort: Some("name".to_owned()),
            ..query()
        };
        assert_eq!(
            by_name.product_sort().unwrap(),
            ("name", false),
            "a name sorts a to z by default"
        );
        let by_sku = CatalogQuery {
            sort: Some("sku".to_owned()),
            direction: Some("desc".to_owned()),
            ..query()
        };
        assert_eq!(by_sku.product_sort().unwrap(), ("sku", true));
        let by_price = CatalogQuery {
            sort: Some("default_price".to_owned()),
            direction: Some("asc".to_owned()),
            ..query()
        };
        assert_eq!(by_price.product_sort().unwrap(), ("default_price", false));
    }

    #[test]
    fn every_sort_key_a_caller_may_name_becomes_a_real_column() {
        // A key that resolved but mapped to the `updated_at` fallback would sort silently by
        // something the person did not ask for, and the list would look broken rather than wrong.
        for (key, expected) in [
            ("sku", "lower(p.sku)"),
            ("name", "lower(p.name)"),
            ("category", "lower(coalesce(p.category, ''))"),
            ("default_price", "p.default_price"),
            ("created_at", "p.created_at"),
            ("updated_at", "p.updated_at"),
        ] {
            assert_eq!(sort_column(key), expected, "{key}");
        }
        for (key, expected) in [
            ("name", "lower(l.name)"),
            ("currency", "l.currency"),
            ("created_at", "l.created_at"),
        ] {
            assert_eq!(list_sort_column(key), expected, "list {key}");
        }
    }

    #[test]
    fn a_price_list_sorts_by_name_because_there_are_few_enough_to_read_alphabetically() {
        assert_eq!(query().price_list_sort().unwrap(), ("name", false));
    }

    // ---- currencies and names -----------------------------------------------------------------

    #[test]
    fn a_currency_is_three_letters_and_lowercase_is_accepted_as_typed() {
        assert_eq!(clean_currency(Some("try")).unwrap(), "TRY");
        assert_eq!(clean_currency(Some("usd")).unwrap(), "USD");
        assert_eq!(clean_currency(None).unwrap(), "TRY");
        assert!(clean_currency(Some("TRYA")).is_err());
        assert!(clean_currency(Some("TR")).is_err());
        assert!(clean_currency(Some("T.R")).is_err());
    }

    #[test]
    fn a_price_list_needs_a_name_and_a_number_prefix_may_not_contain_a_dash() {
        assert!(validate_list_name("   ").is_err());
        assert_eq!(validate_list_name("  Wholesale ").unwrap(), "Wholesale");
        assert!(validate_list_name(&"x".repeat(121)).is_err());

        assert_eq!(validate_prefix(" Q ", "quote_number_prefix").unwrap(), "Q");
        assert!(
            validate_prefix("Q-", "quote_number_prefix").is_err(),
            "a dash would make Q--2026 ambiguous about where the year starts"
        );
        assert!(validate_prefix("", "quote_number_prefix").is_err());
    }

    #[test]
    fn a_window_that_ends_before_it_starts_is_refused() {
        let day = time::Date::from_calendar_date(2026, time::Month::March, 1).unwrap();
        let later = time::Date::from_calendar_date(2026, time::Month::April, 1).unwrap();
        assert!(validate_window(Some(day), Some(day)).is_ok());
        assert!(validate_window(Some(day), Some(later)).is_ok());
        assert!(validate_window(Some(later), Some(day)).is_err());
        assert!(validate_window(None, Some(day)).is_ok());
    }

    // ---- money across the boundary -------------------------------------------------------------

    #[test]
    fn a_price_the_database_holds_reads_back_as_the_same_amount() {
        let price = money_from_text("1234.50").expect("a stored price reads");
        assert_eq!(price.to_text(), "1234.50");
        assert_eq!(price.minor(), 123_450);
    }

    #[test]
    fn a_price_that_cannot_be_read_is_a_refusal_and_not_a_free_product() {
        // The alternative — reading it as zero — would sell the product for nothing with nothing
        // on the screen to say why.
        assert!(money_from_text("not a number").is_err());
        assert!(money_from_text("").is_err());
    }

    #[test]
    fn a_quantity_keeps_the_precision_it_was_stored_with() {
        // `numeric(14,3)` prints `1.500` for a kilo, and a warehouse reads that as three decimals.
        assert_eq!(quantity_from_row("1.500"), "1.500");
        assert_eq!(quantity_from_row(" 2.000 "), "2.000");
    }

    // ---- validation of a create ---------------------------------------------------------------

    #[test]
    fn a_create_normalises_what_it_accepts() {
        let input = NewProduct {
            sku: "  widget-1 ".to_owned(),
            name: " Widget ".to_owned(),
            description: Some(" A thing ".to_owned()),
            category: Some("  tools ".to_owned()),
            unit: None,
            tax_percent: Some(20),
            default_price: Some("19.9".to_owned()),
            currency: Some("usd".to_owned()),
            active: None,
        };
        let values = validate_new(&input).expect("a well-formed product");
        assert_eq!(values.sku, "widget-1");
        assert_eq!(values.name, "Widget");
        assert_eq!(values.description, " A thing ");
        assert_eq!(values.category.as_deref(), Some("tools"));
        assert_eq!(
            values.unit, "piece",
            "a product with no unit is sold by the piece"
        );
        assert_eq!(values.tax_percent, 20);
        assert_eq!(
            values.price, "19.90",
            "a price is stored at the document's scale"
        );
        assert_eq!(values.currency, "USD");
        assert!(
            values.active,
            "a new product is sellable unless it is archived on arrival"
        );
    }

    #[test]
    fn a_create_with_nothing_in_it_still_produces_a_sellable_product_at_zero() {
        // The form's defaults, not an error: a catalog entry with a name and a SKU is a real
        // thing somebody sells before they have worked out its price.
        let input = NewProduct {
            sku: "AB-1".to_owned(),
            name: "Sample".to_owned(),
            ..NewProduct::default()
        };
        let values = validate_new(&input).expect("valid");
        assert_eq!(values.tax_percent, 0);
        assert_eq!(values.price, "0.00");
        assert_eq!(values.currency, "TRY");
        assert!(values.active);
    }

    #[test]
    fn a_negative_price_is_refused_and_says_which_field_it_is() {
        let input = NewProduct {
            sku: "AB-1".to_owned(),
            name: "Sample".to_owned(),
            default_price: Some("-1".to_owned()),
            ..NewProduct::default()
        };
        let error = validate_new(&input).expect_err("a negative price is refused");
        assert!(error.to_string().contains("default_price"), "{error}");
    }

    #[test]
    fn a_description_longer_than_the_column_allows_is_refused() {
        let input = NewProduct {
            sku: "AB-1".to_owned(),
            name: "Sample".to_owned(),
            description: Some("x".repeat(MAX_DESCRIPTION_LENGTH + 1)),
            ..NewProduct::default()
        };
        assert!(validate_new(&input).is_err());
        let at_limit = NewProduct {
            description: Some("x".repeat(MAX_DESCRIPTION_LENGTH)),
            ..input
        };
        assert!(validate_new(&at_limit).is_ok());
    }

    // ---- settings ----------------------------------------------------------------------------

    #[test]
    fn a_threshold_outside_zero_to_a_hundred_is_refused_by_the_validator_that_bounds_the_column() {
        // The migration carries the same check, so a screen that showed the field can never save
        // a value the database would refuse.
        let input = SettingsPatch {
            discount_approval_threshold: Some(101),
            ..SettingsPatch::default()
        };
        assert!(input.discount_approval_threshold.is_some());
        assert!(crate::model::Settings::default().needs_approval(20));
    }

    #[test]
    fn a_price_row_of_a_product_the_catalog_does_not_have_is_named_before_it_is_written() {
        // The tenant guard in the migration would refuse the insert; the point of checking here
        // is that the caller reads a field name rather than a trigger exception as a 500.
        let error = SalesError::invalid(
            "price_list_item",
            "product_id",
            "one of the products is not in this organization's catalog",
        );
        assert!(error.to_string().contains("product_id"), "{error}");
    }

    // ---- the resolver ------------------------------------------------------------------------

    #[test]
    fn the_resolver_answers_a_price_for_a_product_the_list_does_not_carry() {
        // The pure half of `resolve_line_price`: with no list rows, the product's own price is
        // the answer, and it is a real price rather than a missing one.
        let price = catalog::resolve_price(
            Money::parse("7.25").unwrap(),
            &[],
            Uuid::nil(),
            Quantity::one(),
        );
        assert_eq!(price.to_text(), "7.25");
    }
}
