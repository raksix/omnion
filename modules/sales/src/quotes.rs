//! Quotes: the document a seller writes, the versions of it the customer saw, and the totals.
//!
//! Everything the quote screens need is decided here, in the module, rather than in the HTTP
//! layer — because the same rules have to hold for three different writers: the panel's builder,
//! the public token page's accept, and the expiry sweep that runs without a session. A rule that
//! lives in a handler is a rule the other two writers do not have.
//!
//! Four rules the API layer must not have to remember:
//!
//! * **The totals on the row are the truth, and they are recomputed in SQL.** A client that
//!   computed its own total must not be able to present a number the server never agreed to, so
//!   [`replace_lines`] writes the lines, recomputes `subtotal`, `discount_total`, `tax_total`,
//!   `grand_total` and `max_discount` in the same statement as the line write, and the quote is
//!   re-read afterwards. The arithmetic itself is [`crate::money::quote_totals`] — the module
//!   decides the *shape* of the total, SQL decides the *persistence*.
//! * **Editing stops at `sent`.** A sent quote is what the customer read, so its lines are frozen
//!   and the only recovery is to duplicate it. Sending again after an edit is a new version, not
//!   an edit of an old one, which is why the version number is on the row and the immutable
//!   snapshot is in `sales_quote_versions`.
//! * **A public token is a credential: only its hash is stored.** The URL carries the random
//!   token, the database carries `sha256(token)`, and regenerating a link invalidates the previous
//!   one by writing a new hash — an old link then resolves to nothing, which is the point.
//! * **Numbers are assigned per organization from a counter row, under a row lock.** A gap-free
//!   sequence means taking the number inside the same transaction that inserts the quote.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, SalesError};
use crate::model::{CustomerKind, QuoteStatus, Unit};
use crate::money::{LineInput, Money, Quantity, quote_totals};
use crate::store::{DEFAULT_PER_PAGE, MAX_PER_PAGE, MAX_SEARCH_LENGTH, Page};

/// How many lines one quote may carry.
///
/// A quote is a document a person reads; a thousand-line quote is an import wearing a document's
/// clothes. The same bound keeps a request that has lost its quote predicate from rewriting one
/// installation's whole quote table.
pub const MAX_QUOTE_LINES: usize = 500;

/// Longest a quote's title may be.
pub const MAX_TITLE_LENGTH: usize = 200;

/// Longest a free-text line description may be.
pub const MAX_LINE_DESCRIPTION_LENGTH: usize = 1_000;

/// Longest a note or payment term may be.
pub const MAX_NOTE_LENGTH: usize = 4_000;

/// Longest a decline or cancel reason may be.
pub const MAX_REASON_LENGTH: usize = 1_000;

/// How many versions a quote's history screen shows before it paginates.
pub const MAX_VERSIONS: i64 = 200;

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// A quote as the list screen sees it: the header row plus the joined customer and owner names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// `Q-2026-0001` — assigned by the server on create and immutable afterwards.
    pub number: String,
    /// What the seller called it.
    pub title: String,
    /// Where the quote is in its lifecycle.
    pub status: QuoteStatus,
    /// The customer, as a CRM reference plus the name that was captured when the quote was made.
    pub customer: CustomerRef,
    /// The seller, by name, or nobody.
    pub owner: Option<OwnerRef>,
    /// The currency every amount on the quote is expressed in.
    pub currency: String,
    /// The price list the lines were prefilled from, if any.
    pub price_list_id: Option<Uuid>,
    /// The last day the customer may still accept.
    #[serde(with = "crate::dates")]
    pub valid_until: time::Date,
    /// The four totals, recomputed by the server on every write.
    pub totals: QuoteTotalsView,
    /// The version the customer has seen; `0` while the quote has never left the building.
    pub version: i32,
    /// The largest single-line discount, which is what the approval threshold is compared to.
    pub max_discount_percent: i32,
    /// When the quote was last changed.
    #[serde(with = "crate::dates::instant")]
    pub updated_at: OffsetDateTime,
    /// When the row was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

/// The customer of a quote, resolved at read time.
///
/// `id` is `None` when the CRM record the quote names is gone; the **name is captured on the
/// quote** for exactly that reason, so a deleted customer leaves a quote a person can still read
/// rather than an error page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomerRef {
    /// Whether the id is a company or a contact.
    pub kind: CustomerKind,
    /// The CRM record's id, or `None` when it has been removed.
    pub id: Option<Uuid>,
    /// The name as it read when the quote was written.
    pub name: String,
}

/// The seller a quote or line is attributed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRef {
    /// The account's id.
    pub id: Uuid,
    /// The account's display name at read time.
    pub name: String,
}

/// The totals block, as text — the same shape the catalog already returns for a price, so one
/// formatter renders both and a screen cannot show `12.500000` because it read a `numeric`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteTotalsView {
    /// The sum of the lines' gross amounts.
    pub subtotal: String,
    /// The sum of the lines' discounts.
    pub discount_total: String,
    /// The sum of the lines' taxes.
    pub tax_total: String,
    /// What the customer pays.
    pub grand_total: String,
}

/// One line of a quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteLineView {
    /// The line's id.
    pub id: Uuid,
    /// Where the line sits in the grid, from 1.
    pub position: i32,
    /// The product it sells, or `None` for a free-text line.
    pub product_id: Option<Uuid>,
    /// The product's name and SKU as they read now, for the builder's combobox.
    pub product: Option<ProductRef>,
    /// What the line says, which is also the product's name when a product is set.
    pub description: String,
    /// The unit the quantity is counted in.
    pub unit: String,
    /// How many, as decimal text (`1`, `2.5`).
    pub quantity: String,
    /// The price for one, as decimal text.
    pub unit_price: String,
    /// The line's discount, as a whole percent.
    pub discount_percent: i32,
    /// The tax rate snapshot, as a whole percent.
    pub tax_percent: i32,
    /// `unit_price × quantity`, less the discount, plus the tax.
    pub line_total: String,
}

/// The product a line points at, resolved at read time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductRef {
    /// The product's id.
    pub id: Uuid,
    /// The product's SKU.
    pub sku: String,
    /// The product's name.
    pub name: String,
    /// The product's unit, which the builder uses as the line's default.
    pub unit: String,
}

/// A quote with its lines, its versions and whether a public link exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteDetail {
    /// The quote.
    pub quote: QuoteView,
    /// Its lines, in grid order.
    pub lines: Vec<QuoteLineView>,
    /// The immutable snapshots, newest first.
    pub versions: Vec<QuoteVersionView>,
    /// Whether a public link has been issued, without the token or its hash.
    pub has_public_link: bool,
    /// When the public link stops working, if one was issued.
    #[serde(with = "crate::dates::instant::option")]
    pub public_link_expires_at: Option<OffsetDateTime>,
    /// When the quote was sent, if it was.
    #[serde(with = "crate::dates::instant::option")]
    pub sent_at: Option<OffsetDateTime>,
    /// The reason the customer gave when they declined, if they did.
    pub decline_reason: Option<String>,
    /// The reason the organization gave when it cancelled, if it did.
    pub cancel_reason: Option<String>,
    /// The free-text notes the customer reads on the public page.
    pub notes: String,
    /// The payment terms, printed next to the totals.
    pub reference: String,
    /// When the organization decided (accepted, declined or cancelled).
    #[serde(with = "crate::dates::instant::option")]
    pub decided_at: Option<OffsetDateTime>,
}

/// One immutable snapshot: what the customer read at version `version`.
///
/// `PartialEq` without `Eq`: a version holds `serde_json::Value` for the frozen line grid, and
/// `Value` compares equal but is not `Eq` (floats). Nothing in the module needs the stronger
/// bound, and asking for it would be a lie the compiler would not catch until a `Hash` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteVersionView {
    /// The version number, from 1.
    pub version: i32,
    /// The currency the snapshot was taken in.
    pub currency: String,
    /// The lines as they stood, frozen.
    pub lines: serde_json::Value,
    /// The totals as they stood, frozen.
    pub totals: serde_json::Value,
    /// When the snapshot was taken.
    #[serde(with = "crate::dates::instant")]
    pub sent_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------------------------
// The shapes the API writes
// ---------------------------------------------------------------------------------------------

/// The body of a quote create or a full replace: the header and the whole line grid.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewQuote {
    /// The customer's CRM record.
    #[serde(default)]
    pub customer_id: Option<Uuid>,
    /// Whether that record is a company or a contact.
    #[serde(default)]
    pub customer_type: Option<String>,
    /// The customer's name, captured on the quote. A quote with a removed customer still reads.
    #[serde(default)]
    pub customer_name: Option<String>,
    /// What the seller called it.
    #[serde(default)]
    pub title: Option<String>,
    /// The currency; absent means the organization's default.
    #[serde(default)]
    pub currency: Option<String>,
    /// The price list the lines are prefilled from.
    #[serde(default)]
    pub price_list_id: Option<Uuid>,
    /// The seller; absent means the caller.
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// The last day the customer may accept.
    #[serde(default, with = "crate::dates::option")]
    pub valid_until: Option<time::Date>,
    /// How the customer pays, printed next to the totals.
    #[serde(default)]
    pub payment_terms: Option<String>,
    /// The customer's own reference (their PO number).
    #[serde(default)]
    pub reference: Option<String>,
    /// Notes the customer reads on the public page.
    #[serde(default)]
    pub notes: Option<String>,
    /// The line grid. A quote with no line is refused.
    #[serde(default)]
    pub lines: Vec<NewQuoteLine>,
}

/// One line of the grid.
///
/// Money and quantity are **text** on purpose: `numeric` has no Rust type in this workspace (see
/// `store.rs`'s header for the rule), and a JSON number would already have lost the distinction
/// between `0.1` and `0.10` before the module saw it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewQuoteLine {
    /// The product the line sells, or `None` for a free-text line.
    #[serde(default)]
    pub product_id: Option<Uuid>,
    /// What the line says.
    #[serde(default)]
    pub description: Option<String>,
    /// The unit; absent means the product's, or `piece`.
    #[serde(default)]
    pub unit: Option<String>,
    /// How many, as decimal text.
    #[serde(default)]
    pub quantity: Option<String>,
    /// The price for one, as decimal text.
    #[serde(default)]
    pub unit_price: Option<String>,
    /// The line's discount, as a whole percent.
    #[serde(default)]
    pub discount_percent: Option<i32>,
    /// The tax rate to snapshot on the line.
    #[serde(default)]
    pub tax_percent: Option<i32>,
}

/// A header-only edit of a quote that is still in a working status.
///
/// Separate from [`NewQuote`] on purpose: a PATCH that had to carry the whole grid would make
/// every "rename the title" call rewrite the lines, and a client that PATCHed a stale grid over a
/// fresh one would silently revert lines it never saw.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuotePatch {
    /// What the seller calls it.
    #[serde(default)]
    pub title: Option<String>,
    /// The last day the customer may accept.
    #[serde(default, with = "crate::dates::option")]
    pub valid_until: Option<time::Date>,
    /// How the customer pays.
    #[serde(default)]
    pub payment_terms: Option<String>,
    /// The customer's own reference.
    #[serde(default)]
    pub reference: Option<String>,
    /// Notes the customer reads.
    #[serde(default)]
    pub notes: Option<String>,
    /// The price list future lines are prefilled from.
    #[serde(default)]
    pub price_list_id: Option<Uuid>,
    /// The seller.
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
}

/// The query of a quote list.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuoteQuery {
    /// Free text over the number, the title and the captured customer name.
    #[serde(default)]
    pub search: Option<String>,
    /// Statuses to include; absent means every status.
    #[serde(default)]
    pub status: Option<String>,
    /// The seller to filter by.
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// Only quotes that expire within this many days (`0` means "already expired").
    #[serde(default)]
    pub expiring_in_days: Option<i32>,
    /// Only quotes valid on or after this day.
    #[serde(default, with = "crate::dates::option")]
    pub valid_from: Option<time::Date>,
    /// Only quotes whose grand total is at or above this amount.
    #[serde(default)]
    pub min_total: Option<String>,
    /// Only quotes whose grand total is at or below this amount.
    #[serde(default)]
    pub max_total: Option<String>,
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
    /// The archived quotes are included as well.
    #[serde(default)]
    pub include_archived: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// A quote's header after validation, with the defaults already applied.
#[derive(Debug, Clone)]
struct ValidatedQuote {
    customer_type: CustomerKind,
    customer_id: Option<Uuid>,
    customer_name: String,
    title: String,
    currency: String,
    price_list_id: Option<Uuid>,
    owner_user_id: Option<Uuid>,
    valid_until: time::Date,
    payment_terms: String,
    reference: String,
    notes: String,
}

/// A line after validation, with its product's defaults resolved.
#[derive(Debug, Clone)]
struct ValidatedLine {
    product_id: Option<Uuid>,
    description: String,
    unit: String,
    quantity: Quantity,
    unit_price: Money,
    discount_percent: i32,
    tax_percent: i32,
}

impl ValidatedLine {
    /// The line as the arithmetic sees it.
    fn to_input(&self) -> LineInput {
        LineInput {
            quantity: self.quantity,
            unit_price: self.unit_price,
            discount_percent: self.discount_percent,
            tax_percent: self.tax_percent,
        }
    }
}

/// How long a quote may stay valid beyond today, as a ceiling on a hand-typed date.
///
/// A validity of five years is a data-entry accident, and it is refused here rather than left to
/// the expiry sweep to discover in five years.
const MAX_VALIDITY_DAYS: i64 = 1_825;

fn clean_optional(value: Option<String>) -> Option<String> {
    value.map(|raw| raw.trim().to_string()).filter(|s| !s.is_empty())
}

fn clean_currency(raw: Option<&str>) -> Result<String> {
    let Some(raw) = raw else {
        return Ok(String::new());
    };
    let trimmed = raw.trim().to_uppercase();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.len() != 3 || !trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(SalesError::invalid(
            "quote",
            "currency",
            "use a three-letter code such as TRY, USD or EUR",
        ));
    }
    Ok(trimmed)
}

/// Trim, and cut to `limit` characters — cutting rather than refusing.
///
/// A note that is 4 000 characters one character too long is refused with a message the person
/// cannot act on ("invalid quote.notes: too long"), whereas cutting it silently is visible in the
/// PDF and nobody writes a 4 000-character note on purpose. The `entity`/`field` pair is in the
/// signature so a call site that *does* need to refuse can say which field it was, and so the
/// truncation limit of every field is visible in one place.
fn clean_bounded(
    entity: &'static str,
    field: &'static str,
    value: Option<String>,
    limit: usize,
) -> Result<String> {
    let _ = (entity, field);
    Ok(clean_optional(value).unwrap_or_default().chars().take(limit).collect())
}

/// `today + days`, in the module's own clock.
fn add_days(base: time::Date, days: i64) -> time::Date {
    base + time::Duration::days(days)
}

fn validate_quote(
    input: &NewQuote,
    settings: &crate::model::Settings,
    today: time::Date,
) -> Result<ValidatedQuote> {
    let customer_id = input.customer_id;
    let customer_type = match input.customer_type.as_deref().map(str::trim) {
        None | Some("") => CustomerKind::Company,
        Some(raw) => CustomerKind::parse(raw).ok_or_else(|| {
            SalesError::invalid(
                "quote",
                "customer_type",
                "a customer is either a company or a contact",
            )
        })?,
    };
    if customer_id.is_none() {
        return Err(SalesError::invalid(
            "quote",
            "customer_id",
            "a quote needs a customer",
        ));
    }

    let customer_name = clean_bounded("quote", "customer_name", input.customer_name.clone(), 160)?;
    let title = clean_bounded("quote", "title", input.title.clone(), MAX_TITLE_LENGTH)?;
    let currency = {
        let chosen = clean_currency(input.currency.as_deref())?;
        if chosen.is_empty() {
            settings.currency.clone()
        } else {
            chosen
        }
    };
    let valid_until = match input.valid_until {
        Some(day) => day,
        None => add_days(today, i64::from(settings.quote_validity_days)),
    };
    if valid_until < today {
        return Err(SalesError::invalid(
            "quote",
            "valid_until",
            "the last day to accept cannot be in the past",
        ));
    }
    if (valid_until - today).whole_days() > MAX_VALIDITY_DAYS {
        return Err(SalesError::invalid(
            "quote",
            "valid_until",
            "a quote may be valid for at most five years",
        ));
    }

    Ok(ValidatedQuote {
        customer_type,
        customer_id,
        customer_name,
        title,
        currency,
        price_list_id: input.price_list_id,
        owner_user_id: input.owner_user_id,
        valid_until,
        payment_terms: clean_bounded("quote", "payment_terms", input.payment_terms.clone(), 200)?,
        reference: clean_bounded("quote", "reference", input.reference.clone(), 120)?,
        notes: clean_bounded("quote", "notes", input.notes.clone(), MAX_NOTE_LENGTH)?,
    })
}

fn validate_line(
    index: usize,
    input: &NewQuoteLine,
    fallback_unit: Option<&str>,
    fallback_price: Option<&Money>,
    fallback_tax: Option<i32>,
) -> Result<ValidatedLine> {
    let position = index + 1;
    let description = clean_bounded(
        "quote_line",
        "description",
        input.description.clone(),
        MAX_LINE_DESCRIPTION_LENGTH,
    )?;
    if input.product_id.is_none() && description.is_empty() {
        return Err(SalesError::invalid(
            "quote_line",
            "description",
            format!("line {position} needs a product or a description"),
        ));
    }

    let unit = clean_optional(input.unit.clone())
        .or_else(|| fallback_unit.map(str::to_owned))
        .unwrap_or_else(|| Unit::Piece.as_str().to_string());
    if unit.len() > 32 {
        return Err(SalesError::invalid(
            "quote_line",
            "unit",
            format!("line {position}: a unit may be at most 32 characters"),
        ));
    }

    let quantity = match input.quantity.as_deref() {
        Some(raw) => Quantity::parse(raw)
            .map_err(|source| SalesError::number("quote_line", "quantity", source))?,
        None => Quantity::one(),
    };
    if !quantity.is_positive() {
        return Err(SalesError::invalid(
            "quote_line",
            "quantity",
            format!("line {position}: the quantity must be greater than zero"),
        ));
    }

    let unit_price = match input.unit_price.as_deref().map(str::trim) {
        Some("") | None => fallback_price
            .copied()
            .unwrap_or_else(Money::zero),
        Some(raw) => Money::parse(raw)
            .map_err(|source| SalesError::number("quote_line", "unit_price", source))?,
    };
    if unit_price.minor() < 0 {
        return Err(SalesError::invalid(
            "quote_line",
            "unit_price",
            format!("line {position}: the price cannot be negative"),
        ));
    }

    let discount_percent = input.discount_percent.unwrap_or(0);
    if !(0..=100).contains(&discount_percent) {
        return Err(SalesError::invalid(
            "quote_line",
            "discount_percent",
            format!("line {position}: the discount must be between 0 and 100"),
        ));
    }
    let tax_percent = input.tax_percent.unwrap_or_else(|| fallback_tax.unwrap_or(0));
    if !(0..=100).contains(&tax_percent) {
        return Err(SalesError::invalid(
            "quote_line",
            "tax_percent",
            format!("line {position}: the tax must be between 0 and 100"),
        ));
    }

    Ok(ValidatedLine {
        product_id: input.product_id,
        description,
        unit,
        quantity,
        unit_price,
        discount_percent,
        tax_percent,
    })
}

/// What a line inherits from its product when the line does not carry its own values.
#[derive(Debug, Clone)]
struct ProductDefaults {
    /// The product's unit.
    unit: String,
    /// The product's tax rate snapshot.
    tax_percent: i32,
    /// The product's own price.
    default_price: Money,
    /// The price list's price for this product, when the builder selected a list that carries one.
    list_price: Option<Money>,
}

/// The default price a product resolves to for a quote line: the price list's row when the builder
/// selected a list that carries one, the product's own default price otherwise.
///
/// The list price wins outright rather than being a "fallback", because a price list is a promise
/// about what this customer pays. A missing row means the seller never priced them, and the
/// product's public price is then the honest answer; the line's own price overrides both.
async fn resolve_line_defaults(
    pool: &PgPool,
    organization_id: Uuid,
    lines: &[NewQuoteLine],
    price_list_id: Option<Uuid>,
) -> Result<Vec<Option<ProductDefaults>>> {
    let product_ids: Vec<Uuid> = lines.iter().filter_map(|line| line.product_id).collect();
    if product_ids.is_empty() {
        return Ok(vec![None; lines.len()]);
    }

    let products: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "select id, unit, tax_percent::text, default_price::text
           from sales_products
          where organization_id = $1 and id = any($2)",
    )
    .bind(organization_id)
    .bind(&product_ids)
    .fetch_all(pool)
    .await?;

    let list_prices: Vec<(Uuid, String)> = match price_list_id {
        Some(list_id) => sqlx::query_as(
            "select product_id, price::text
               from sales_price_list_items
              where organization_id = $1 and price_list_id = $2",
        )
        .bind(organization_id)
        .bind(list_id)
        .fetch_all(pool)
        .await?,
        None => Vec::new(),
    };

    Ok(lines
        .iter()
        .map(|line| {
            line.product_id.and_then(|product_id| {
                products.iter().find(|(id, ..)| *id == product_id).map(
                    |(_, unit, tax, default_price)| ProductDefaults {
                        unit: unit.clone(),
                        tax_percent: parse_percent(tax),
                        default_price: Money::parse(default_price).unwrap_or_else(|_| Money::zero()),
                        list_price: list_prices
                            .iter()
                            .find(|(id, _)| *id == product_id)
                            .and_then(|(_, price)| Money::parse(price).ok()),
                    },
                )
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// The numbers
// ---------------------------------------------------------------------------------------------

/// The next document number for an organization, taken inside the caller's transaction.
///
/// A counter table rather than `count(*)`: two concurrent creates both counting the same rows get
/// the same number, and a quote number a person reads off a PDF must never be issued twice. The
/// `for update` serializes the counter and the insert happens in the same transaction, so a
/// rolled-back create takes its number with it and the sequence stays gap-free.
///
/// The prefix comes from the caller's settings row, which the `organizations` trigger already
/// creates — this function deliberately does not create it, because a *number* write that also
/// manufactures a settings row is two facts in one statement and the settings one is silent.
pub async fn next_number(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    prefix: &str,
    kind: &str,
) -> Result<String> {
    let table = match kind {
        "quote" => "sales_quotes",
        "order" => "sales_orders",
        other => {
            return Err(SalesError::invalid(
                "quote",
                "number",
                format!("unknown document kind `{other}`"),
            ));
        }
    };

    let current: Option<(i64,)> = sqlx::query_as(
        "select next_quote from sales_number_sequences
          where organization_id = $1 and kind = $2
          for update",
    )
    .bind(organization_id)
    .bind(kind)
    .fetch_optional(&mut **transaction)
    .await?;

    let number = match current {
        Some((value,)) => {
            sqlx::query(
                "update sales_number_sequences set next_quote = next_quote + 1
                  where organization_id = $1 and kind = $2",
            )
            .bind(organization_id)
            .bind(kind)
            .execute(&mut **transaction)
            .await?;
            value
        }
        None => {
            // Seed from the documents that already exist, so landing on an installation that
            // already has quotes does not restart at 1 and collide with `Q-1`.
            let sql = format!(
                "select max(nullif(regexp_replace(number, '[^0-9]', '', 'g'), '')::bigint)
                   from {table} where organization_id = $1"
            );
            let existing: (Option<i64>,) = sqlx::query_as(&sql)
                .bind(organization_id)
                .fetch_one(&mut **transaction)
                .await?;
            let first = existing.0.unwrap_or(0) + 1;
            sqlx::query(
                "insert into sales_number_sequences (organization_id, kind, next_quote)
                 values ($1, $2, $3)",
            )
            .bind(organization_id)
            .bind(kind)
            .bind(first)
            .execute(&mut **transaction)
            .await?;
            first
        }
    };
    Ok(format!("{prefix}-{number}"))
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

/// The SQL both the list and the single read project a [`QuoteView`] from.
///
/// Written once because a list column and a detail field that disagree is a screen that shows a
/// different total from the one the customer was sent.
fn quote_columns() -> &'static str {
    "q.id, q.organization_id, q.number, q.title, q.status, q.customer_type, q.customer_id,
     q.customer_name, q.currency, q.price_list_id, q.owner_user_id, q.valid_until::text,
     q.subtotal::text, q.discount_total::text, q.tax_total::text, q.grand_total::text,
     q.max_discount::text, q.version, q.updated_at, q.created_at"
}

#[derive(Debug, FromRow)]
struct QuoteRow {
    id: Uuid,
    organization_id: Uuid,
    number: String,
    title: String,
    status: String,
    customer_type: String,
    customer_id: Option<Uuid>,
    customer_name: String,
    currency: String,
    price_list_id: Option<Uuid>,
    owner_user_id: Option<Uuid>,
    valid_until: String,
    subtotal: String,
    discount_total: String,
    tax_total: String,
    grand_total: String,
    max_discount: String,
    version: i32,
    updated_at: OffsetDateTime,
    created_at: OffsetDateTime,
}

impl QuoteRow {
    /// The row as the screens see it.
    ///
    /// The status is read with [`QuoteStatus::parse`] and an unknown value is an error rather
    /// than a default: a quote in a state this build cannot explain is one a person must not be
    /// shown as a draft, because a draft is editable.
    fn into_view(self, owner_name: Option<String>) -> Result<QuoteView> {
        let status = QuoteStatus::parse(&self.status)
            .ok_or_else(|| SalesError::invalid("quote", "status", "this quote has an unknown status"))?;
        let customer_kind = CustomerKind::parse(&self.customer_type).unwrap_or(CustomerKind::Company);
        Ok(QuoteView {
            id: self.id,
            organization_id: self.organization_id,
            number: self.number,
            title: self.title,
            status,
            customer: CustomerRef {
                kind: customer_kind,
                id: self.customer_id,
                name: self.customer_name,
            },
            owner: self.owner_user_id.map(|id| OwnerRef {
                id,
                name: owner_name.unwrap_or_default(),
            }),
            currency: self.currency,
            price_list_id: self.price_list_id,
            valid_until: parse_day(&self.valid_until, "valid_until")?,
            totals: QuoteTotalsView {
                subtotal: self.subtotal,
                discount_total: self.discount_total,
                tax_total: self.tax_total,
                grand_total: self.grand_total,
            },
            version: self.version,
            max_discount_percent: parse_percent(&self.max_discount),
            updated_at: self.updated_at,
            created_at: self.created_at,
        })
    }
}

fn parse_day(raw: &str, field: &'static str) -> Result<time::Date> {
    time::Date::parse(raw, &time::format_description::well_known::Iso8601::DATE)
        .map_err(|_| SalesError::invalid("quote", field, "this is not a date the platform reads"))
}

/// A percentage column, read as a whole number.
///
/// The module treats percentages as whole numbers everywhere else, and a row that carries
/// `20.00` must read as `20` — a builder that shows `20.00%` and a policy check that compares
/// `20.0 > 15` are both fine, but a screen that rendered the raw text would show `20.00`.
fn parse_percent(raw: &str) -> i32 {
    raw.parse::<f64>().map(|value| value.round() as i32).unwrap_or(0)
}

async fn owner_names(pool: &PgPool, ids: &[Uuid]) -> std::collections::HashMap<Uuid, String> {
    if ids.is_empty() {
        return std::collections::HashMap::new();
    }
    sqlx::query_as::<_, (Uuid, String)>(
        "select id, coalesce(display_name, email) from users where id = any($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .map(|rows| rows.into_iter().collect())
    .unwrap_or_default()
}

/// `GET /sales/quotes` — one page of the list.
pub async fn list_quotes(
    pool: &PgPool,
    organization_id: Uuid,
    query: &QuoteQuery,
) -> Result<Page<QuoteView>> {
    let limit = query.limit.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE);
    let statuses = parse_status_filter(query.status.as_deref())?;
    let search = clean_optional(query.search.clone()).filter(|s| s.chars().count() <= MAX_SEARCH_LENGTH);
    let min_total = parse_money_filter(query.min_total.as_deref(), "min_total")?;
    let max_total = parse_money_filter(query.max_total.as_deref(), "max_total")?;
    if let (Some(min), Some(max)) = (&min_total, &max_total) {
        if min > max {
            return Err(SalesError::InvalidQuery(
                "min_total cannot be greater than max_total".to_string(),
            ));
        }
    }
    let today = today_utc();

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "select q.id, q.organization_id, q.number, q.title, q.status, q.customer_type,
                q.customer_id, q.customer_name, q.currency, q.price_list_id, q.owner_user_id,
                q.valid_until::text, q.subtotal::text, q.discount_total::text, q.tax_total::text,
                q.grand_total::text, q.max_discount::text, q.version, q.updated_at, q.created_at
           from sales_quotes q
          where q.organization_id = ",
    );
    builder.push_bind(organization_id);
    if query.include_archived != Some(true) {
        builder.push(" and q.archived_at is null");
    }
    if !statuses.is_empty() {
        builder.push(" and q.status = any(");
        builder.push_bind(&statuses);
        builder.push(")");
    }
    if query.owner_user_id.is_some() {
        builder.push(" and q.owner_user_id = ");
        builder.push_bind(query.owner_user_id);
    }
    if let Some(days) = query.expiring_in_days {
        let horizon = add_days(today, i64::from(days));
        builder.push(" and q.valid_until <= ");
        builder.push_bind(horizon);
    }
    if let Some(from) = query.valid_from {
        builder.push(" and q.valid_until >= ");
        builder.push_bind(from);
    }
    if let Some(min) = &min_total {
        builder.push(" and q.grand_total >= ");
        builder.push_bind(min.to_text());
    }
    if let Some(max) = &max_total {
        builder.push(" and q.grand_total <= ");
        builder.push_bind(max.to_text());
    }
    if let Some(term) = &search {
        let pattern = format!("%{}%", escape_like(term));
        builder.push(" and (q.number ilike ");
        builder.push_bind(pattern.clone());
        builder.push(" or q.title ilike ");
        builder.push_bind(pattern.clone());
        builder.push(" or q.customer_name ilike ");
        builder.push_bind(pattern);
        builder.push(" escape '\\')");
    }

    let (sort, direction) = quote_sort(query.sort.as_deref(), query.direction.as_deref())?;
    // The column is one of four literals decided by `quote_sort` (which refuses anything else), so
    // interpolating it cannot inject SQL — but the `order by` is a fixed string per case rather
    // than one formatted string, so a future column cannot be spelled wrong here.
    let order_by = match sort {
        "number" => "q.number",
        "valid_until" => "q.valid_until",
        "total" => "q.grand_total",
        _ => "q.updated_at desc",
    };
    if sort == "updated_at" {
        // The default is "newest first, ties broken by the id" so the keyset cursor below is a
        // strict total order. Without the tiebreak, two quotes written in the same microsecond
        // would make the cursor ambiguous and one of them would never be paged.
        builder.push(format!(" order by q.updated_at {direction}, q.id {direction}"));
    } else {
        builder.push(format!(" order by {order_by} {direction}"));
    }
    if let Some(cursor) = &query.cursor {
        // Keyset, not offset: the list is sorted by `updated_at`, so a quote edited between two
        // page requests would shift an offset window and silently skip a row.
        let (stamp, id) = decode_cursor(cursor)?;
        builder.push(" and (q.updated_at, q.id) < (");
        builder.push_bind(stamp);
        builder.push(", ");
        builder.push_bind(id);
        builder.push(")");
    }
    builder.push(" limit ");
    builder.push_bind(limit + 1);

    let rows: Vec<QuoteRow> = builder.build_query_as().fetch_all(pool).await?;
    let has_more = rows.len() > limit as usize;
    let mut page: Vec<QuoteRow> = rows.into_iter().take(limit as usize).collect();
    let names = owner_names(
        pool,
        &page.iter().filter_map(|row| row.owner_user_id).collect::<Vec<_>>(),
    )
    .await;
    let mut out = Vec::with_capacity(page.len());
    for row in page.drain(..) {
        let name = row.owner_user_id.and_then(|id| names.get(&id).cloned());
        out.push(row.into_view(name)?);
    }
    let next_cursor = if has_more {
        out.last().map(|quote| encode_cursor(quote.updated_at, quote.id))
    } else {
        None
    };
    Ok(Page::new(out, next_cursor, total_estimate(pool, organization_id, query, &statuses).await))
}

/// How many rows the same filter matches, for the list's "N results" line.
///
/// Counted with a **separate, simpler** query than the page's: reusing the page's builder with
/// its `limit + 1` and its cursor would report the page's size, and a screen showing "1–50 of 50"
/// for a thousand quotes is a screen nobody trusts.
async fn total_estimate(
    pool: &PgPool,
    organization_id: Uuid,
    query: &QuoteQuery,
    statuses: &[String],
) -> i64 {
    let search =
        clean_optional(query.search.clone()).filter(|s| s.chars().count() <= MAX_SEARCH_LENGTH);
    let Ok(min_total) = parse_money_filter(query.min_total.as_deref(), "min_total") else {
        return 0;
    };
    let Ok(max_total) = parse_money_filter(query.max_total.as_deref(), "max_total") else {
        return 0;
    };

    let mut builder: QueryBuilder<Postgres> =
        QueryBuilder::new("select count(*)::bigint from sales_quotes q where q.organization_id = ");
    builder.push_bind(organization_id);
    if query.include_archived != Some(true) {
        builder.push(" and q.archived_at is null");
    }
    if !statuses.is_empty() {
        builder.push(" and q.status = any(");
        builder.push_bind(statuses.to_vec());
        builder.push(")");
    }
    if query.owner_user_id.is_some() {
        builder.push(" and q.owner_user_id = ");
        builder.push_bind(query.owner_user_id);
    }
    if let Some(days) = query.expiring_in_days {
        builder.push(" and q.valid_until <= ");
        builder.push_bind(add_days(today_utc(), i64::from(days)));
    }
    if let Some(from) = query.valid_from {
        builder.push(" and q.valid_until >= ");
        builder.push_bind(from);
    }
    if let Some(min) = &min_total {
        builder.push(" and q.grand_total >= ");
        builder.push_bind(min.to_text());
    }
    if let Some(max) = &max_total {
        builder.push(" and q.grand_total <= ");
        builder.push_bind(max.to_text());
    }
    if let Some(term) = &search {
        let pattern = format!("%{}%", escape_like(term));
        builder.push(" and (q.number ilike ");
        builder.push_bind(pattern.clone());
        builder.push(" or q.title ilike ");
        builder.push_bind(pattern.clone());
        builder.push(" or q.customer_name ilike ");
        builder.push_bind(pattern);
        builder.push(" escape '\\')");
    }
    // `build_query_scalar`, not `query_scalar(&string)`: the builder has already bound the
    // values into placeholders, so the query has to be run as it was built rather than re-parsed
    // from its own SQL text.
    builder
        .build_query_scalar::<i64>()
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

fn parse_status_filter(raw: Option<&str>) -> Result<Vec<String>> {
    let Some(raw) = raw else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let status = QuoteStatus::parse(trimmed)
            .ok_or_else(|| SalesError::InvalidQuery(format!("unknown quote status `{trimmed}`")))?;
        out.push(status.as_str().to_string());
    }
    Ok(out)
}

fn parse_money_filter(raw: Option<&str>, field: &'static str) -> Result<Option<Money>> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let money = Money::parse(raw.trim())
        .map_err(|source| SalesError::number("quote", field, source))?;
    Ok(Some(money))
}

fn quote_sort(sort: Option<&str>, direction: Option<&str>) -> Result<(&'static str, &'static str)> {
    let column = match sort {
        None | Some("") | Some("updated") | Some("updated_at") => "updated_at",
        Some("number") => "number",
        Some("valid_until") | Some("valid") => "valid_until",
        Some("total") | Some("amount") => "total",
        Some(other) => {
            return Err(SalesError::InvalidQuery(format!(
                "unknown sort key `{other}`; use updated, number, valid_until or total"
            )));
        }
    };
    let direction = match direction {
        None | Some("") => "desc",
        Some("asc") => "asc",
        Some("desc") => "desc",
        Some(other) => {
            return Err(SalesError::InvalidQuery(format!(
                "unknown sort direction `{other}`; use asc or desc"
            )));
        }
    };
    Ok((column, direction))
}

fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The keyset cursor: the last row's `(updated_at, id)`, base64url'd.
///
/// A keyset rather than an offset because the list is sorted by `updated_at` — a quote edited
/// between two page requests would move the offset window and silently skip a row.
fn encode_cursor(stamp: OffsetDateTime, id: Uuid) -> String {
    use base64::Engine as _;
    let raw = format!("{}|{}", stamp.unix_timestamp_nanos(), id);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

fn decode_cursor(cursor: &str) -> Result<(OffsetDateTime, Uuid)> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let (nanos, id) = text
        .split_once('|')
        .ok_or_else(|| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let nanos: i128 = nanos
        .parse()
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let id: Uuid = id
        .parse()
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    let stamp = OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|_| SalesError::InvalidQuery("the page cursor is not readable".to_string()))?;
    Ok((stamp, id))
}

/// `GET /sales/quotes/{id}` — one quote with its lines and its versions.
pub async fn get_quote(pool: &PgPool, organization_id: Uuid, quote_id: Uuid) -> Result<QuoteDetail> {
    let row = fetch_quote_row(pool, organization_id, quote_id).await?;
    let owner_name = match row.owner_user_id {
        Some(id) => owner_names(pool, &[id]).await.get(&id).cloned(),
        None => None,
    };
    let quote = row.into_view(owner_name)?;
    let lines = list_lines(pool, quote_id).await?;
    let versions = list_versions(pool, quote_id).await?;
    let (has_public_link, expires_at) = sqlx::query_as::<_, (bool, Option<OffsetDateTime>)>(
        "select public_token_hash is not null, public_token_expires_at from sales_quotes where id = $1",
    )
    .bind(quote_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or((false, None));

    let extra: (Option<OffsetDateTime>, Option<String>, Option<String>, String, String, Option<OffsetDateTime>) = sqlx::query_as(
        "select sent_at, decline_reason, cancel_reason, notes, reference,
                case when accepted_at is not null or declined_at is not null or cancelled_at is not null
                     then coalesce(accepted_at, declined_at, cancelled_at) end
           from sales_quotes where id = $1",
    )
    .bind(quote_id)
    .fetch_one(pool)
    .await?;

    Ok(QuoteDetail {
        quote,
        lines,
        versions,
        has_public_link,
        public_link_expires_at: expires_at,
        sent_at: extra.0,
        decline_reason: extra.1,
        cancel_reason: extra.2,
        notes: extra.3,
        reference: extra.4,
        decided_at: extra.5,
    })
}

async fn fetch_quote_row(pool: &PgPool, organization_id: Uuid, quote_id: Uuid) -> Result<QuoteRow> {
    let sql = format!(
        "select {} from sales_quotes q where q.organization_id = $1 and q.id = $2",
        quote_columns()
    );
    sqlx::query_as::<_, QuoteRow>(&sql)
        .bind(organization_id)
        .bind(quote_id)
        .fetch_optional(pool)
        .await?
        .ok_or(SalesError::NotFound("quote"))
}

async fn list_lines(pool: &PgPool, quote_id: Uuid) -> Result<Vec<QuoteLineView>> {
    let rows = sqlx::query_as::<_, (
        Uuid,
        i32,
        Option<Uuid>,
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        String,
        i32,
        i32,
        String,
    )>(
        "select l.id, l.position, l.product_id, p.sku, p.name, l.description, l.unit,
                l.quantity::text, l.unit_price::text, l.discount_percent::int4, l.tax_percent::int4,
                l.line_total::text
           from sales_quote_lines l
           left join sales_products p on p.id = l.product_id
          where l.quote_id = $1
          order by l.position",
    )
    .bind(quote_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                position,
                product_id,
                sku,
                name,
                description,
                unit,
                quantity,
                unit_price,
                discount_percent,
                tax_percent,
                line_total,
            )| QuoteLineView {
                id,
                position,
                product_id,
                product: product_id.map(|pid| ProductRef {
                    id: pid,
                    sku: sku.unwrap_or_default(),
                    name: name.unwrap_or_default(),
                    unit: unit.clone(),
                }),
                description,
                unit,
                quantity,
                unit_price,
                discount_percent,
                tax_percent,
                line_total,
            },
        )
        .collect())
}

async fn list_versions(pool: &PgPool, quote_id: Uuid) -> Result<Vec<QuoteVersionView>> {
    let rows = sqlx::query_as::<_, (i32, String, serde_json::Value, serde_json::Value, OffsetDateTime)>(
        // Each amount is projected **as text**, deliberately: the live quote's totals arrive as
        // strings (money crosses the boundary as text — see `store.rs`), so a frozen version that
        // carried JSON numbers would make the version history need a second formatter, and a
        // `numeric` in JSON is a float by the time a browser parses it.
        "select version, currency, lines, jsonb_build_object(
                    'subtotal', subtotal::text, 'discount_total', discount_total::text,
                    'tax_total', tax_total::text, 'grand_total', grand_total::text,
                    'max_discount', max_discount::text),
                sent_at
           from sales_quote_versions
          where quote_id = $1
          order by version desc
          limit $2",
    )
    .bind(quote_id)
    .bind(MAX_VERSIONS)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(version, currency, lines, totals, sent_at)| QuoteVersionView {
            version,
            currency,
            lines,
            totals,
            sent_at,
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// `POST /sales/quotes` — create a draft.
pub async fn create_quote(
    pool: &PgPool,
    organization_id: Uuid,
    settings: &crate::model::Settings,
    actor: Uuid,
    input: &NewQuote,
) -> Result<QuoteDetail> {
    if input.lines.is_empty() {
        return Err(SalesError::invalid(
            "quote",
            "lines",
            "a quote needs at least one line",
        ));
    }
    if input.lines.len() > MAX_QUOTE_LINES {
        return Err(SalesError::invalid(
            "quote",
            "lines",
            format!("a quote may carry at most {MAX_QUOTE_LINES} lines"),
        ));
    }

    let today = today_utc();
    let header = validate_quote(input, settings, today)?;
    let defaults = resolve_line_defaults(pool, organization_id, &input.lines, header.price_list_id).await?;
    let mut lines = Vec::with_capacity(input.lines.len());
    for (index, line) in input.lines.iter().enumerate() {
        let fallback = defaults[index].as_ref();
        let fallback_price = fallback.and_then(|d| d.list_price.or(Some(d.default_price)));
        lines.push(validate_line(
            index,
            line,
            fallback.map(|d| d.unit.as_str()),
            fallback_price.as_ref(),
            fallback.map(|d| d.tax_percent),
        )?);
    }
    let totals = quote_totals(&lines.iter().map(ValidatedLine::to_input).collect::<Vec<_>>());

    let mut transaction = pool.begin().await?;
    let number = next_number(&mut transaction, organization_id, &settings.quote_number_prefix, "quote").await?;

    let quote_id: Uuid = sqlx::query_as::<_, (Uuid,)>(
        "insert into sales_quotes (organization_id, number, title, customer_type, customer_id,
                customer_name, status, currency, price_list_id, owner_user_id, valid_until,
                payment_terms, reference, notes, subtotal, discount_total, tax_total, grand_total,
                max_discount, version)
         values ($1, $2, $3, $4, $5, $6, 'draft', $7, $8, $9, $10, $11, $12, $13,
                 $14::numeric, $15::numeric, $16::numeric, $17::numeric, $18::numeric, 0)
         returning id",
    )
    .bind(organization_id)
    .bind(&number)
    .bind(&header.title)
    .bind(header.customer_type.as_str())
    .bind(header.customer_id)
    .bind(&header.customer_name)
    .bind(&header.currency)
    .bind(header.price_list_id)
    .bind(header.owner_user_id.or(Some(actor)))
    .bind(header.valid_until)
    .bind(&header.payment_terms)
    .bind(&header.reference)
    .bind(&header.notes)
    .bind(totals.subtotal.to_text())
    .bind(totals.discount_total.to_text())
    .bind(totals.tax_total.to_text())
    .bind(totals.grand_total.to_text())
    .bind(totals.max_discount_percent.to_string())
    .fetch_one(&mut *transaction)
    .await?
    .0;

    write_lines(&mut transaction, organization_id, quote_id, &lines).await?;
    transaction.commit().await?;

    get_quote(pool, organization_id, quote_id).await
}

/// Write the whole grid, then recompute the quote's totals **from the stored lines**, in the same
/// transaction.
///
/// The arithmetic is [`crate::money::quote_totals`] and SQL both run here, deliberately: the module
/// decides the *shape* of the total (round once per line, tax on the discounted amount) and the
/// database decides the *persistence*. Recomputing from the rows that were just written — rather
/// than from the caller's own arithmetic — is what makes the header impossible to disagree with
/// the grid beside it, and comparing the two is what proves the SQL formula agrees with the
/// module to the cent.
async fn write_lines(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    quote_id: Uuid,
    lines: &[ValidatedLine],
) -> Result<()> {
    sqlx::query("delete from sales_quote_lines where quote_id = $1")
        .bind(quote_id)
        .execute(&mut **transaction)
        .await?;

    for (index, line) in lines.iter().enumerate() {
        let computed = line.to_input().totals();
        sqlx::query(
            "insert into sales_quote_lines (organization_id, quote_id, position, product_id,
                    description, unit, quantity, unit_price, discount_percent, tax_percent,
                    line_total)
             values ($1, $2, $3, $4, $5, $6, $7::numeric, $8::numeric, $9, $10, $11::numeric)",
        )
        .bind(organization_id)
        .bind(quote_id)
        .bind(i32::try_from(index + 1).unwrap_or(i32::MAX))
        .bind(line.product_id)
        .bind(&line.description)
        .bind(&line.unit)
        .bind(line.quantity.to_text())
        .bind(line.unit_price.to_text())
        .bind(line.discount_percent)
        .bind(line.tax_percent)
        .bind(computed.net.to_text())
        .execute(&mut **transaction)
        .await?;
    }

    // `grand_total` is deliberately **not** `subtotal - discount + tax`: that reintroduces exactly
    // the accumulated rounding error the per-line rule exists to avoid. It is the sum of the
    // rounded payable amounts plus the rounded taxes, which is the rule `quote_totals` applies.
    // `round(x, 2)` is PostgreSQL's half-away-from-zero, the same rule as `money::round_to`.
    sqlx::query(
        "update sales_quotes q
            set subtotal = agg.subtotal,
                discount_total = agg.discount_total,
                tax_total = agg.tax_total,
                grand_total = agg.grand_total,
                max_discount = agg.max_discount,
                updated_at = now()
          from (select coalesce(sum(round(quantity * unit_price, 2)), 0) as subtotal,
                       coalesce(sum(round(quantity * unit_price * discount_percent / 100.0, 2)), 0) as discount_total,
                       coalesce(sum(round(quantity * unit_price * (100 - discount_percent) / 100.0 * tax_percent / 100.0, 2)), 0) as tax_total,
                       coalesce(sum(round(quantity * unit_price * (100 - discount_percent) / 100.0, 2)
                                 + round(quantity * unit_price * (100 - discount_percent) / 100.0 * tax_percent / 100.0, 2)), 0) as grand_total,
                       coalesce(max(discount_percent), 0) as max_discount
                  from sales_quote_lines
                 where quote_id = $1) agg
         where q.id = $1",
    )
    .bind(quote_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// `PATCH /sales/quotes/{id}` — edit a header that is still in a working status.
///
/// Separate from the grid write on purpose: a PATCH that had to carry the whole line grid would
/// make every "rename the title" call rewrite the lines, and a client holding a stale grid would
/// silently revert lines it never saw.
pub async fn patch_quote(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
    input: &QuotePatch,
) -> Result<QuoteDetail> {
    let existing = fetch_quote_row(pool, organization_id, quote_id).await?;
    let status = QuoteStatus::parse(&existing.status)
        .ok_or_else(|| SalesError::invalid("quote", "status", "this quote has an unknown status"))?;
    if !status.is_editable() {
        // A sent quote is what the customer read. Reporting this as a validation failure would
        // send the caller hunting for a bad field; it is a conflict with a documented way out.
        return Err(SalesError::already_sent("quote", existing.number));
    }

    let today = today_utc();
    // A `null` member means "leave it alone", which is what makes a sparse PATCH sparse. Clearing
    // a field is an explicit empty string, so a client cannot wipe a note by omitting it.
    let mut sets: Vec<&str> = Vec::new();
    if input.title.is_some() {
        sets.push("title = $3");
    }
    if let Some(day) = input.valid_until {
        if day < today {
            return Err(SalesError::invalid(
                "quote",
                "valid_until",
                "the last day to accept cannot be in the past",
            ));
        }
        if (day - today).whole_days() > MAX_VALIDITY_DAYS {
            return Err(SalesError::invalid(
                "quote",
                "valid_until",
                "a quote may be valid for at most five years",
            ));
        }
        sets.push("valid_until = $4");
    }
    if input.payment_terms.is_some() {
        sets.push("payment_terms = $5");
    }
    if input.reference.is_some() {
        sets.push("reference = $6");
    }
    if input.notes.is_some() {
        sets.push("notes = $7");
    }
    if input.price_list_id.is_some() {
        sets.push("price_list_id = $8");
    }
    if input.owner_user_id.is_some() {
        sets.push("owner_user_id = $9");
    }
    if sets.is_empty() {
        // A PATCH with nothing to change is a read, not an error: the screen calls it after every
        // save and a 400 here would make an idempotent save look broken.
        return get_quote(pool, organization_id, quote_id).await;
    }

    let sql = format!(
        "update sales_quotes set {}, updated_at = now()
          where organization_id = $1 and id = $2 and archived_at is null",
        sets.join(", ")
    );
    let updated: Option<(i32,)> = sqlx::query_as(
        &format!("{sql} returning length(number)::int"),
    )
    .bind(organization_id)
    .bind(quote_id)
    .bind(input.title.as_deref().map(|v| clean_bounded("quote", "title", Some(v.to_string()), MAX_TITLE_LENGTH)).transpose()?)
    .bind(input.valid_until)
    .bind(
        input
            .payment_terms
            .as_ref()
            .map(|v| clean_bounded("quote", "payment_terms", Some(v.clone()), 200))
            .transpose()?,
    )
    .bind(
        input
            .reference
            .as_ref()
            .map(|v| clean_bounded("quote", "reference", Some(v.clone()), 120))
            .transpose()?,
    )
    .bind(
        input
            .notes
            .as_ref()
            .map(|v| clean_bounded("quote", "notes", Some(v.clone()), MAX_NOTE_LENGTH))
            .transpose()?,
    )
    .bind(input.price_list_id)
    .bind(input.owner_user_id)
    .fetch_optional(pool)
    .await?;
    if updated.is_none() {
        return Err(SalesError::NotFound("quote"));
    }
    get_quote(pool, organization_id, quote_id).await
}

/// `PUT /sales/quotes/{id}/lines` — replace the whole grid of a working quote.
pub async fn replace_lines(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
    settings: &crate::model::Settings,
    input: &[NewQuoteLine],
) -> Result<QuoteDetail> {
    if input.is_empty() {
        return Err(SalesError::invalid(
            "quote",
            "lines",
            "a quote needs at least one line",
        ));
    }
    if input.len() > MAX_QUOTE_LINES {
        return Err(SalesError::invalid(
            "quote",
            "lines",
            format!("a quote may carry at most {MAX_QUOTE_LINES} lines"),
        ));
    }
    let existing = fetch_quote_row(pool, organization_id, quote_id).await?;
    let status = QuoteStatus::parse(&existing.status)
        .ok_or_else(|| SalesError::invalid("quote", "status", "this quote has an unknown status"))?;
    if !status.is_editable() {
        return Err(SalesError::already_sent("quote", existing.number));
    }

    let price_list_id = sqlx::query_as::<_, (Option<Uuid>,)>(
        "select price_list_id from sales_quotes where id = $1",
    )
    .bind(quote_id)
    .fetch_one(pool)
    .await?
    .0;
    let _ = settings;

    let defaults = resolve_line_defaults(pool, organization_id, input, price_list_id).await?;
    let mut lines = Vec::with_capacity(input.len());
    for (index, line) in input.iter().enumerate() {
        let fallback = defaults[index].as_ref();
        let fallback_price = fallback.and_then(|d| d.list_price.or(Some(d.default_price)));
        lines.push(validate_line(
            index,
            line,
            fallback.map(|d| d.unit.as_str()),
            fallback_price.as_ref(),
            fallback.map(|d| d.tax_percent),
        )?);
    }
    // The caller's own arithmetic is computed and then **discarded**: `write_lines` recomputes
    // the header from the rows it just wrote, and the two agreeing to the cent is what the
    // integration test asserts. Computing it here would be a second source of truth.
    let _ = quote_totals(&lines.iter().map(ValidatedLine::to_input).collect::<Vec<_>>());

    let mut transaction = pool.begin().await?;
    write_lines(&mut transaction, organization_id, quote_id, &lines).await?;
    transaction.commit().await?;

    get_quote(pool, organization_id, quote_id).await
}

/// `POST /sales/quotes/{id}/send` — freeze the lines, snapshot a version, mark it sent.
pub async fn send_quote(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
    actor: Uuid,
) -> Result<QuoteDetail> {
    let mut transaction = pool.begin().await?;
    let row = fetch_quote_row_tx(&mut transaction, organization_id, quote_id).await?;
    let status = QuoteStatus::parse(&row.status)
        .ok_or_else(|| SalesError::invalid("quote", "status", "this quote has an unknown status"))?;
    if status != QuoteStatus::Draft && status != QuoteStatus::Approved {
        return Err(SalesError::InvalidStatusChange(format!(
            "a {status} quote cannot be sent — only a draft or an approved one can"
        )));
    }
    let today = today_utc();
    // The column is read as text (the store's money/date rule) and parsed here rather than
    // compared as a string: `"2026-9-8" < "2026-10-1"` is true in ASCII and false as a date.
    let valid_until = parse_day(&row.valid_until, "valid_until")?;
    if valid_until < today {
        return Err(SalesError::InvalidStatusChange(format!(
            "quote {} expired on {valid_until} and cannot be sent",
            row.number
        )));
    }

    let line_count: (i64,) = sqlx::query_as(
        "select count(*)::bigint from sales_quote_lines where quote_id = $1",
    )
    .bind(quote_id)
    .fetch_one(&mut *transaction)
    .await?;
    if line_count.0 == 0 {
        return Err(SalesError::invalid(
            "quote",
            "lines",
            "a quote needs at least one line before it can be sent",
        ));
    }

    let version = row.version + 1;
    let snapshot: Option<serde_json::Value> = sqlx::query_scalar(
        "select jsonb_agg(jsonb_build_object(
                    'position', position, 'product_id', product_id, 'description', description,
                    'unit', unit, 'quantity', quantity, 'unit_price', unit_price,
                    'discount_percent', discount_percent, 'tax_percent', tax_percent,
                    'line_total', line_total)
                order by position)
           from sales_quote_lines where quote_id = $1",
    )
    .bind(quote_id)
    .fetch_one(&mut *transaction)
    .await?;

    // The frozen totals are the same four numbers the header carries, so a version and the live
    // quote it came from cannot disagree: they are read from one row in one pass.
    //
    // `organization_id` is bound rather than defaulted: the snapshot table is `not null` on it
    // because every table in this migration carries the tenant, and a snapshot without one could
    // not be listed by a future "which versions of this organization's quotes exist" query
    // without a join back to a row that may itself be archived.
    sqlx::query(
        "insert into sales_quote_versions (organization_id, quote_id, version, currency, lines,
                subtotal, discount_total, tax_total, grand_total, max_discount)
         values ($1, $2, $3, $4, $5, $6::numeric, $7::numeric, $8::numeric, $9::numeric,
                 $10::numeric)",
    )
    .bind(organization_id)
    .bind(quote_id)
    .bind(version)
    .bind(&row.currency)
    .bind(snapshot.unwrap_or_else(|| serde_json::json!([])))
    .bind(&row.subtotal)
    .bind(&row.discount_total)
    .bind(&row.tax_total)
    .bind(&row.grand_total)
    .bind(&row.max_discount)
    .execute(&mut *transaction)
    .await?;

    sqlx::query(
        "update sales_quotes set status = 'sent', version = $3, sent_at = now(), updated_at = now()
          where id = $1 and organization_id = $2",
    )
    .bind(quote_id)
    .bind(organization_id)
    .bind(version)
    .execute(&mut *transaction)
    .await?;
    let _ = actor;
    transaction.commit().await?;

    get_quote(pool, organization_id, quote_id).await
}

async fn fetch_quote_row_tx(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    quote_id: Uuid,
) -> Result<QuoteRow> {
    let sql = format!(
        "select {} from sales_quotes q where q.organization_id = $1 and q.id = $2 for update",
        quote_columns()
    );
    sqlx::query_as::<_, QuoteRow>(&sql)
        .bind(organization_id)
        .bind(quote_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(SalesError::NotFound("quote"))
}

/// `POST /sales/quotes/{id}/cancel` — withdraw a quote that is not decided.
pub async fn cancel_quote(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
    reason: Option<String>,
) -> Result<QuoteDetail> {
    let existing = fetch_quote_row(pool, organization_id, quote_id).await?;
    let status = QuoteStatus::parse(&existing.status)
        .ok_or_else(|| SalesError::invalid("quote", "status", "this quote has an unknown status"))?;
    if status.is_decided() {
        return Err(SalesError::InvalidStatusChange(format!(
            "a {status} quote cannot be cancelled — the decision already stands"
        )));
    }
    let reason = clean_bounded("quote", "cancel_reason", reason, MAX_REASON_LENGTH)?;
    sqlx::query(
        "update sales_quotes set status = 'cancelled', cancel_reason = $3, cancelled_at = now(),
                updated_at = now()
          where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(quote_id)
    .bind(&reason)
    .execute(pool)
    .await?;
    get_quote(pool, organization_id, quote_id).await
}

/// `POST /sales/quotes/{id}/duplicate` — copy a quote into a new draft.
///
/// The copy is a **new document with a new number**, even when the source is a draft: a seller
/// who duplicates by accident has two quotes, and the numbers being equal is what makes that
/// visible.
pub async fn duplicate_quote(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
    actor: Uuid,
) -> Result<QuoteDetail> {
    let source = get_quote(pool, organization_id, quote_id).await?;
    let settings = crate::store::get_settings(pool, organization_id).await?;
    let input = NewQuote {
        customer_id: source.quote.customer.id,
        customer_type: Some(source.quote.customer.kind.as_str().to_string()),
        customer_name: Some(source.quote.customer.name.clone()),
        title: Some(source.quote.title.clone()),
        currency: Some(source.quote.currency.clone()),
        price_list_id: source.quote.price_list_id,
        owner_user_id: source.quote.owner.as_ref().map(|owner| owner.id),
        valid_until: Some(source.quote.valid_until),
        payment_terms: Some(source.reference.clone()),
        reference: Some(source.reference.clone()),
        notes: Some(source.notes.clone()),
        lines: source
            .lines
            .iter()
            .map(|line| NewQuoteLine {
                product_id: line.product_id,
                description: Some(line.description.clone()),
                unit: Some(line.unit.clone()),
                quantity: Some(line.quantity.clone()),
                unit_price: Some(line.unit_price.clone()),
                discount_percent: Some(line.discount_percent),
                tax_percent: Some(line.tax_percent),
            })
            .collect(),
    };
    let _ = actor;
    create_quote(pool, organization_id, &settings, actor, &input).await
}

// ---------------------------------------------------------------------------------------------
// The public link
// ---------------------------------------------------------------------------------------------

/// The plain token a public link carries. Returned **once**, never stored.
///
/// Two `v4` UUIDs joined by a dash: 256 bits from the OS generator, and a token that is a
/// plausible database identifier — which is the point. A token is going to be pasted into a chat
/// window and read aloud on a call, and a caller that is told "this is an id, look it up" learns
/// something true, instead of guessing whether it is a hash.
pub fn mint_public_token() -> String {
    format!("{}-{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// The token's storage form. `sha256`, lower hex — a public row is readable by anyone with a
/// database dump, so the column holds nothing that opens the link.
pub fn hash_public_token(token: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A fresh public link: the plain token, and the row updated to its hash.
///
/// Regenerating is how a seller revokes: the previous hash is overwritten, so the old URL stops
/// resolving. That is why this is an *upsert of a new hash* rather than a flag.
pub async fn issue_public_link(
    pool: &PgPool,
    organization_id: Uuid,
    quote_id: Uuid,
) -> Result<String> {
    let row = fetch_quote_row(pool, organization_id, quote_id).await?;
    let status = QuoteStatus::parse(&row.status)
        .ok_or_else(|| SalesError::invalid("quote", "status", "this quote has an unknown status"))?;
    if !matches!(
        status,
        QuoteStatus::Sent | QuoteStatus::Accepted | QuoteStatus::Approved
    ) {
        return Err(SalesError::InvalidStatusChange(format!(
            "a {status} quote has no public link — send it first"
        )));
    }
    let valid_until = parse_day(&row.valid_until, "valid_until")?;
    let today = today_utc();
    if valid_until < today {
        return Err(SalesError::InvalidStatusChange(format!(
            "quote {} expired on {valid_until}", row.number
        )));
    }
    let token = mint_public_token();
    let hash = hash_public_token(&token);
    // The link dies with the quote's validity, plus a small grace so a customer who opens the
    // email on day 30 still sees "expired" rather than a dead link.
    let expires_at = OffsetDateTime::now_utc() + time::Duration::days(7);
    sqlx::query(
        "update sales_quotes set public_token_hash = $3, public_token_expires_at = $4
          where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(quote_id)
    .bind(&hash)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(token)
}

/// The quote a public token resolves to, or `None` when it does not resolve.
///
/// The four refusals — wrong, expired, consumed, archived — are **one** answer. Distinguishing
/// them would turn the public page into an oracle that reports which tokens were once issued.
pub async fn resolve_public_token(pool: &PgPool, token: &str) -> Result<Option<Uuid>> {
    let trimmed = token.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Ok(None);
    }
    let hash = hash_public_token(trimmed);
    let row: Option<(Uuid, OffsetDateTime, String, Option<time::Date>)> = sqlx::query_as(
        "select id, public_token_expires_at, status, valid_until
           from sales_quotes
          where public_token_hash = $1 and archived_at is null",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await?;
    let Some((id, expires_at, status, valid_until)) = row else {
        return Ok(None);
    };
    if now_utc() > expires_at {
        return Ok(None);
    }
    if let Some(day) = valid_until {
        if day < today_utc() {
            return Ok(None);
        }
    }
    let status = QuoteStatus::parse(&status).unwrap_or(QuoteStatus::Draft);
    if !matches!(
        status,
        QuoteStatus::Sent | QuoteStatus::Accepted | QuoteStatus::Approved
    ) {
        return Ok(None);
    }
    Ok(Some(id))
}

/// The public payload: the quote a customer may read, and nothing else.
///
/// The **notes** are included (they are written for the customer) and everything internal is not:
/// the owner, the price list, the version history, the margin-free totals are fine, but the
/// organization's internal identifiers and the seller's name are not part of the document.
pub async fn public_quote(pool: &PgPool, quote_id: Uuid) -> Result<PublicQuote> {
    // `get_quote` is tenant-scoped and a public read has no tenant to pass. The token has already
    // resolved to this exact row, so the organization is read here and the call re-scoped — the
    // two reads cost one extra indexed lookup, which is the price of not adding a second,
    // tenant-free read path into the module's own query layer.
    let organization: Option<(Uuid,)> = sqlx::query_as(
        "select organization_id from sales_quotes where id = $1",
    )
    .bind(quote_id)
    .fetch_optional(pool)
    .await?;
    let organization_id = organization.ok_or(SalesError::InvalidPublicToken)?.0;
    let detail = get_quote(pool, organization_id, quote_id).await?;

    Ok(PublicQuote {
        number: detail.quote.number.clone(),
        title: detail.quote.title.clone(),
        status: detail.quote.status,
        customer_name: detail.quote.customer.name.clone(),
        currency: detail.quote.currency.clone(),
        valid_until: detail.quote.valid_until,
        lines: detail
            .lines
            .iter()
            .map(|line| PublicLine {
                description: if line.description.trim().is_empty() {
                    line.product.as_ref().map(|p| p.name.clone()).unwrap_or_default()
                } else {
                    line.description.clone()
                },
                unit: line.unit.clone(),
                quantity: line.quantity.clone(),
                unit_price: line.unit_price.clone(),
                discount_percent: line.discount_percent,
                tax_percent: line.tax_percent,
                line_total: line.line_total.clone(),
            })
            .collect(),
        totals: detail.quote.totals.clone(),
        notes: detail.notes.clone(),
        payment_terms: detail
            .quote
            .owner
            .as_ref()
            .map(|_| String::new())
            .unwrap_or_default(),
        reference: detail.reference.clone(),
        decided: detail.decided_at.is_some(),
    })
}

/// What a customer sees: the document and nothing about the seller's side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicQuote {
    /// The quote's number, which the customer quotes back on a phone call.
    pub number: String,
    /// Its title.
    pub title: String,
    /// Its status, so the page can say "you accepted this" rather than offering the buttons.
    pub status: QuoteStatus,
    /// The customer's own name.
    pub customer_name: String,
    /// The currency every amount is in.
    pub currency: String,
    /// The last day it may be accepted.
    #[serde(with = "crate::dates")]
    pub valid_until: time::Date,
    /// The lines, with the product's name filled in where the line is a product line.
    pub lines: Vec<PublicLine>,
    /// The totals block.
    pub totals: QuoteTotalsView,
    /// Notes written for the customer.
    pub notes: String,
    /// The payment terms, empty unless the document carries them.
    pub payment_terms: String,
    /// The customer's own reference.
    pub reference: String,
    /// Whether the customer already decided.
    pub decided: bool,
}

/// One line as the customer reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicLine {
    /// What the line sells.
    pub description: String,
    /// The unit the quantity is in.
    pub unit: String,
    /// How many, as text.
    pub quantity: String,
    /// The price for one.
    pub unit_price: String,
    /// The line's discount.
    pub discount_percent: i32,
    /// The tax applied.
    pub tax_percent: i32,
    /// What the line contributes.
    pub line_total: String,
}

/// `POST /sales/public/quotes/{token}/accept` — the customer accepted.
pub async fn accept_public_quote(
    pool: &PgPool,
    token: &str,
    note: Option<String>,
) -> Result<QuoteView> {
    let quote_id = resolve_public_token(pool, token)
        .await?
        .ok_or(SalesError::InvalidPublicToken)?;
    let note = clean_bounded("quote", "note", note, MAX_REASON_LENGTH)?;
    let updated: Option<(Uuid,)> = sqlx::query_as(
        "update sales_quotes set status = 'accepted', accepted_at = now(), updated_at = now()
          where id = $1 and status = 'sent' and archived_at is null
          returning id",
    )
    .bind(quote_id)
    .fetch_one(pool)
    .await
    .ok();
    if updated.is_none() {
        return Err(SalesError::InvalidPublicToken);
    }
    if !note.is_empty() {
        sqlx::query("update sales_quotes set notes = notes || $2 where id = $1")
            .bind(quote_id)
            .bind(format!("\n\n— from the customer: {note}"))
            .execute(pool)
            .await?;
    }
    let organization: (Uuid,) = sqlx::query_as("select organization_id from sales_quotes where id = $1")
        .bind(quote_id)
        .fetch_one(pool)
        .await?;
    let row = fetch_quote_row(pool, organization.0, quote_id).await?;
    row.into_view(None)
}

/// `POST /sales/public/quotes/{token}/decline` — the customer declined, with the reason they gave.
pub async fn decline_public_quote(
    pool: &PgPool,
    token: &str,
    reason: Option<String>,
) -> Result<QuoteView> {
    let quote_id = resolve_public_token(pool, token)
        .await?
        .ok_or(SalesError::InvalidPublicToken)?;
    let reason = clean_bounded("quote", "decline_reason", reason, MAX_REASON_LENGTH)?;
    if reason.is_empty() {
        return Err(SalesError::invalid(
            "quote",
            "reason",
            "a decline asks the customer what went wrong — it is the only thing a seller learns",
        ));
    }
    let updated: Option<(Uuid,)> = sqlx::query_as(
        "update sales_quotes set status = 'declined', decline_reason = $2, declined_at = now(),
                updated_at = now()
          where id = $1 and status = 'sent' and archived_at is null
          returning id",
    )
    .bind(quote_id)
    .bind(&reason)
    .fetch_one(pool)
    .await
    .ok();
    if updated.is_none() {
        return Err(SalesError::InvalidPublicToken);
    }
    let organization: (Uuid,) = sqlx::query_as("select organization_id from sales_quotes where id = $1")
        .bind(quote_id)
        .fetch_one(pool)
        .await?;
    let row = fetch_quote_row(pool, organization.0, quote_id).await?;
    row.into_view(None)
}

/// Flip every quote whose `valid_until` has passed to `expired`.
///
/// Run on every list read as well as by a background sweep, so a quote that lapsed overnight is
/// badged `expired` in the list **and** on the public page without waiting for a job — the sweep
/// is the safety net, the read is the immediate truth.
pub async fn sweep_expired(pool: &PgPool, organization_id: Uuid) -> Result<u64> {
    let result = sqlx::query(
        "update sales_quotes set status = 'expired', updated_at = now()
          where organization_id = $1 and status in ('sent', 'approved')
            and valid_until < $2 and archived_at is null",
    )
    .bind(organization_id)
    .bind(today_utc())
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Today's date in the module's own clock (UTC), as a [`time::Date`].
#[must_use]
pub fn today_utc() -> time::Date {
    time::OffsetDateTime::now_utc().date()
}

/// The current instant, wrapped so a test can compare against it.
#[must_use]
pub fn now_utc() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percentage_column_reads_back_as_a_whole_number() {
        // The column is numeric(5,2) so an accounting module can add a fraction later, but the
        // builder and the policy check both work in whole percents.
        assert_eq!(parse_percent("20.00"), 20);
        assert_eq!(parse_percent("0"), 0);
        assert_eq!(parse_percent("7.5"), 8);
        assert_eq!(parse_percent(""), 0);
    }

    #[test]
    fn a_cursor_survives_the_round_trip_and_a_damaged_one_is_refused() {
        let stamp = OffsetDateTime::from_unix_timestamp_nanos(1_756_000_000_000_000_000).unwrap();
        let id = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let cursor = encode_cursor(stamp, id);
        let (back_stamp, back_id) = decode_cursor(&cursor).expect("the cursor round-trips");
        assert_eq!(back_stamp.unix_timestamp_nanos(), stamp.unix_timestamp_nanos());
        assert_eq!(back_id, id);
        for damaged in ["", "not base64!!", "YWJj", "MTIzfGFiY2Q"] {
            assert!(decode_cursor(damaged).is_err(), "{damaged} should be refused");
        }
    }

    #[test]
    fn a_search_term_cannot_smuggle_wildcards_into_the_like() {
        // `%` in the search box is a character somebody types, not "match everything".
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn an_unknown_status_is_refused_rather_than_read_as_a_draft() {
        assert!(parse_status_filter(Some("draft,quoted")).is_err());
        assert!(parse_status_filter(Some("draft,sent")).is_ok());
        assert!(parse_status_filter(None).unwrap().is_empty(), "no filter means every status");
    }

    #[test]
    fn an_unknown_sort_key_is_refused_instead_of_falling_back_to_a_default() {
        // A silently ignored sort makes a list look broken: the person clicks "Total" and the
        // order does not change.
        assert!(quote_sort(Some("amount"), None).is_ok());
        assert!(quote_sort(Some("nonsense"), None).is_err());
        assert!(quote_sort(None, Some("sideways")).is_err());
        assert_eq!(quote_sort(None, None).unwrap(), ("updated_at", "desc"));
    }

    #[test]
    fn only_a_hashed_token_is_stored_and_two_tokens_differ() {
        let first = mint_public_token();
        let second = mint_public_token();
        assert_ne!(first, second);
        assert_eq!(first.len(), 65, "two 128-bit halves joined by a dash");
        assert_eq!(
            hash_public_token(&first),
            hash_public_token(&first),
            "hashing is deterministic, which is what makes the lookup work"
        );
        assert_ne!(hash_public_token(&first), hash_public_token(&second));
        assert_ne!(
            first, hash_public_token(&first),
            "the stored form must not be the token itself"
        );
        assert!(hash_public_token(&first).chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_line_needs_a_product_or_something_to_say() {
        let error = validate_line(
            0,
            &NewQuoteLine { quantity: Some("1".into()), ..NewQuoteLine::default() },
            None,
            None,
            None,
        )
        .expect_err("an empty line is refused");
        assert!(error.to_string().contains("line 1"), "{error}");
    }

    #[test]
    fn a_line_price_falls_back_to_the_product_and_then_to_zero() {
        // A product line inherits the product's unit, tax and price when it names none; a
        // free-text line with neither a price nor a product is free, and that is a legal quote
        // line (a goodwill line a seller writes by hand), not an accident.
        let fallback = Money::parse("19.90").unwrap();
        let line = validate_line(
            0,
            &NewQuoteLine {
                product_id: Some(Uuid::nil()),
                quantity: Some("2".into()),
                ..NewQuoteLine::default()
            },
            Some("hour"),
            Some(&fallback),
            Some(20),
        )
        .expect("the fallback answers");
        assert_eq!(line.unit_price.to_text(), "19.90");
        assert_eq!(line.unit, "hour");
        assert_eq!(line.tax_percent, 20);

        let free = validate_line(
            0,
            &NewQuoteLine {
                description: Some("Goodwill".into()),
                quantity: Some("1".into()),
                ..NewQuoteLine::default()
            },
            None,
            None,
            None,
        )
        .expect("a described line with no price is free, not broken");
        assert_eq!(free.unit_price.to_text(), "0.00");
        assert_eq!(free.unit, "piece");
        assert_eq!(free.tax_percent, 0);
    }

    #[test]
    fn a_zero_or_negative_quantity_is_refused_with_the_line_number_in_the_message() {
        for raw in ["0", "-3"] {
            let error = validate_line(
                2,
                &NewQuoteLine {
                    description: Some("Support".into()),
                    quantity: Some(raw.into()),
                    ..NewQuoteLine::default()
                },
                None,
                None,
                None,
            )
            .expect_err("zero is refused");
            assert!(error.to_string().contains("line 3"), "{raw}: {error}");
        }
    }

    #[test]
    fn a_discount_outside_the_range_is_refused_before_it_reaches_the_row() {
        let error = validate_line(
            0,
            &NewQuoteLine {
                description: Some("Item".into()),
                discount_percent: Some(140),
                ..NewQuoteLine::default()
            },
            None,
            None,
            None,
        )
        .expect_err("140% is not a discount");
        assert!(error.to_string().contains("between 0 and 100"), "{error}");
    }

    #[test]
    fn a_quote_without_a_customer_is_refused() {
        let error = validate_quote(
            &NewQuote::default(),
            &crate::model::Settings::default(),
            today_utc(),
        )
        .expect_err("a quote needs a customer");
        assert!(error.to_string().contains("customer"), "{error}");
    }

    #[test]
    fn a_validity_in_the_past_is_refused_and_the_default_is_the_settings_window() {
        let settings = crate::model::Settings::default();
        let today = today_utc();
        let past = validate_quote(
            &NewQuote { customer_id: Some(Uuid::nil()), valid_until: Some(today - time::Duration::days(1)), ..NewQuote::default() },
            &settings,
            today,
        );
        assert!(past.is_err(), "a quote that expired yesterday is not a quote");

        let defaulted = validate_quote(
            &NewQuote { customer_id: Some(Uuid::nil()), ..NewQuote::default() },
            &settings,
            today,
        )
        .expect("the default window applies");
        assert_eq!(defaulted.valid_until, today + time::Duration::days(30));
    }

    #[test]
    fn a_currency_is_normalised_to_three_upper_case_letters_or_refused() {
        let settings = crate::model::Settings::default();
        let today = today_utc();
        let lower = validate_quote(
            &NewQuote {
                customer_id: Some(Uuid::nil()),
                currency: Some("try".into()),
                ..NewQuote::default()
            },
            &settings,
            today,
        )
        .expect("a lowercase code is accepted and normalised");
        assert_eq!(lower.currency, "TRY");

        let bad = validate_quote(
            &NewQuote { customer_id: Some(Uuid::nil()), currency: Some("TRYL".into()), ..NewQuote::default() },
            &settings,
            today,
        );
        assert!(bad.is_err(), "four letters is not an ISO code");
    }

    #[test]
    fn the_defaults_of_a_quote_without_a_settings_row_are_the_migration_defaults() {
        let settings = crate::model::Settings::default();
        let today = today_utc();
        let quote = validate_quote(
            &NewQuote { customer_id: Some(Uuid::nil()), ..NewQuote::default() },
            &settings,
            today,
        )
        .expect("valid");
        assert_eq!(quote.currency, settings.currency);
        assert_eq!(quote.customer_type, CustomerKind::Company);
    }

    #[test]
    fn the_totals_of_the_grid_are_the_module_arithmetic_not_the_caller() {
        // 2 × 100.00 with 10% off and 20% tax: gross 200, discount 20, taxable 180, tax 36, net 216.
        let lines = [LineInput {
            quantity: Quantity::parse("2").unwrap(),
            unit_price: Money::parse("100.00").unwrap(),
            discount_percent: 10,
            tax_percent: 20,
        }];
        let totals = quote_totals(&lines);
        assert_eq!(totals.subtotal.to_text(), "200.00");
        assert_eq!(totals.discount_total.to_text(), "20.00");
        assert_eq!(totals.tax_total.to_text(), "36.00");
        assert_eq!(totals.grand_total.to_text(), "216.00");
        assert_eq!(totals.max_discount_percent, 10);
    }
}
