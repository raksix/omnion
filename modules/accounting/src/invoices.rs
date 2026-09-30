//! The invoices: the document a customer is asked to pay, and the status machine it walks.
//!
//! Slice 1 built the ledger's spine — an entry that does not balance cannot be read. This slice
//! builds the **document** that writes into it, and it inherits the same discipline in a form
//! the sales side never had to think about: *nothing here is trusted from the client.* Every total
//! on the wire is recomputed from the lines in this file, because the browser's copy of a
//! financial total is a display and never a fact.
//!
//! # The four rules this file exists to keep
//!
//! 1. **The server owns the arithmetic.** A client that posts `subtotal: "0.00"` on an invoice
//!    whose lines add up to 1,200 does not create a free invoice; it creates a 400 naming
//!    `lines`. The price list, the discount and the tax are all read from the submitted lines and
//!    rounded **once per line** — the rounding rule the whole workspace inherited, because a cent
//!    that disagrees between the panel, the PDF and the report is a support ticket.
//! 2. **A draft is editable, and nothing else is.** Editing a sent invoice is refused, and the
//!    answer names the way out: void it and duplicate. That is not pedantry — the number the
//!    customer was sent is a fact about the past, and a document whose number moves after the
//!    fact is a document nobody can reconcile.
//! 3. **Void keeps the number.** A voided invoice is excluded from the receivables and still
//!    listed under the Void tab. Reusing `INV-0007` for a new document would make two different
//!    amounts answer to one reference, which is exactly the collision the unique index on
//!    `(organization_id, number)` cannot protect against once the row is gone.
//! 4. **The total invariant is a fact about the row, not a claim about a table.** `grand_total`
//!    is a column written by the same statement as the lines, and the schema's CHECK says
//!    `grand_total = subtotal - discount_total + tax_total` — so a future route, a bulk import or
//!    a person with a psql prompt cannot write an invoice whose header disagrees with its lines.
//!
//! # The status machine
//!
//! ```text
//!            send                partial            paid
//!   draft ──────────► sent ──────────────────► partial ──────► paid
//!     │                 │                        │              │
//!     │                 └─── overdue (sweep) ────┘              │
//!     └────────────────── void ◄────────────────────────────────┘
//! ```
//!
//! `overdue` is not reachable by hand: it is what the sweep sets when the day passes the due date,
//! and the sweep is idempotent per invoice (an invoice flips **once** and emits
//! `accounting.invoice.overdue` once), which is why the column `overdue_at` is stored rather than
//! computed — "is it overdue" has to mean "has anybody acted on it yet".

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AccountingError, Result};
use crate::money::Amount;

/// The most lines one invoice may carry.
///
/// An invoice is a human-sized document. Like [`crate::journal::MAX_LINES`] this is not a real
/// limit; it is the bound that stops a request that has lost its tenant predicate from writing the
/// whole installation's ledger in one call.
pub const MAX_LINES: usize = 500;

/// Longest a line description may be.
pub const MAX_DESCRIPTION_LENGTH: usize = 300;

/// Longest a free-text note (payment terms, reference, notes, a void reason) may be.
pub const MAX_NOTE_LENGTH: usize = 500;

/// The default number of days an invoice is due when the caller names no due date.
///
/// Thirty is the convention the sales module's default terms use, and the two screens have to
/// agree: a due date the client guesses differently from the server is a receivable report that
/// disagrees with the list it was read from.
pub const DEFAULT_DUE_DAYS: i64 = 30;

// ---------------------------------------------------------------------------------------------
// The status machine
// ---------------------------------------------------------------------------------------------

/// Where an invoice is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceStatus {
    /// Being written. The only state that is editable.
    Draft,
    /// Issued to the customer, not yet paid.
    Sent,
    /// Issued and paid in part.
    Partial,
    /// Issued and paid in full.
    Paid,
    /// Issued, past its due date, not paid. Set by the sweep, never by a person.
    Overdue,
    /// Withdrawn. Keeps its number; excluded from the receivables.
    Void,
}

impl InvoiceStatus {
    /// The value stored in `accounting_invoices.invoice_status`, which the CHECK allows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Sent => "sent",
            Self::Partial => "partial",
            Self::Paid => "paid",
            Self::Overdue => "overdue",
            Self::Void => "void",
        }
    }

    /// Read a stored status.
    ///
    /// An unknown value is `None` rather than a default, so a row written by a newer version is
    /// reported instead of being shown as a draft somebody has to re-send.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "draft" => Some(Self::Draft),
            "sent" => Some(Self::Sent),
            "partial" => Some(Self::Partial),
            "paid" => Some(Self::Paid),
            "overdue" => Some(Self::Overdue),
            "void" => Some(Self::Void),
            _ => None,
        }
    }

    /// Every status, in the order the list screen's tabs show them.
    #[must_use]
    pub const fn all() -> [Self; 6] {
        [
            Self::Draft,
            Self::Sent,
            Self::Partial,
            Self::Paid,
            Self::Overdue,
            Self::Void,
        ]
    }

    /// The name the badge prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Sent => "Sent",
            Self::Partial => "Partially paid",
            Self::Paid => "Paid",
            Self::Overdue => "Overdue",
            Self::Void => "Void",
        }
    }

    /// Whether the invoice may still be edited.
    ///
    /// Only a draft, and the reason is the whole point: a sent invoice's number was given to a
    /// customer, so changing the amount afterwards is not an edit, it is a different document
    /// wearing the same number.
    #[must_use]
    pub const fn is_editable(self) -> bool {
        matches!(self, Self::Draft)
    }

    /// Whether the invoice counts as money still owed.
    ///
    /// A void invoice does not, which is the whole reason void exists as a status rather than a
    /// delete — the receivables report filters on this predicate, so a withdrawn invoice stops
    /// aging while staying in the list where the person who voided it can see it.
    #[must_use]
    pub const fn is_receivable(self) -> bool {
        matches!(self, Self::Sent | Self::Partial | Self::Overdue)
    }

    /// The status an invoice moves to when a payment of `paid` cents lands against `outstanding`.
    ///
    /// Zero pays nothing and leaves the status alone, because a payment that covers nothing has
    /// not paid the invoice. The function is `const` so the payment slice and the tests cannot
    /// disagree about what a partial payment does.
    #[must_use]
    pub const fn after_payment(self, paid: Amount, outstanding: Amount) -> Self {
        if outstanding.is_zero() {
            Self::Paid
        } else if paid.is_zero() {
            self
        } else {
            Self::Partial
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The shapes
// ---------------------------------------------------------------------------------------------

/// One line of an invoice, as the detail screen and the PDF show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvoiceLineView {
    /// The line's id.
    pub id: Uuid,
    /// The invoice it belongs to.
    pub invoice_id: Uuid,
    /// Where it sits in the invoice.
    pub position: i32,
    /// The catalog product it came from, when it came from one.
    pub product_id: Option<Uuid>,
    /// What is being billed.
    pub description: String,
    /// How many. Three decimal places, so a weight or an hour works as well as a piece.
    pub qty: String,
    /// The price of one, before the discount.
    pub unit_price: String,
    /// The percentage taken off the line.
    pub discount_percent: String,
    /// The percentage of tax on the line, **copied at issue time** and never read back from the
    /// rate table. Changing a rate must not rewrite an invoice that has already been issued.
    pub tax_percent: String,
    /// `qty × unit_price`, less the discount, plus the tax. Rounded once, here.
    pub line_total: String,
    /// The share of that total which is tax, for the totals block's breakdown.
    pub tax_amount: String,
}

/// An invoice with its lines, as the detail screen shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvoiceView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The per-organization number, e.g. `INV-0007`.
    pub number: String,
    /// The sales order it was converted from, when it was.
    pub order_id: Option<Uuid>,
    /// The number of that order, so the detail screen can print `order SO-0012` without a second
    /// request and without hard-coding a link into the sales module.
    pub order_number: Option<String>,
    /// The company being billed.
    pub company_id: Option<Uuid>,
    /// The contact being billed, when the invoice names a person rather than an account.
    pub contact_id: Option<Uuid>,
    /// The customer's name as the document says it.
    pub customer_name: String,
    /// Where it is in its life.
    pub status: InvoiceStatus,
    /// The currency every amount on this document is in.
    pub currency: String,
    /// The day it is dated.
    #[serde(with = "crate::dates")]
    pub issue_date: Date,
    /// The day it is due. `None` on a draft that has not been given one.
    #[serde(with = "crate::dates::option")]
    pub due_date: Option<Date>,
    /// The terms the customer was given, as free text ("net 30").
    pub payment_terms: String,
    /// The customer's own reference, e.g. a PO number.
    pub reference: String,
    /// A free-text note printed on the document.
    pub notes: String,
    /// The sum of the lines' pre-discount amounts.
    pub subtotal: String,
    /// The sum of the lines' discounts.
    pub discount_total: String,
    /// The sum of the lines' tax.
    pub tax_total: String,
    /// `subtotal - discount_total + tax_total`, stored.
    pub grand_total: String,
    /// How much has been paid against it.
    pub paid_total: String,
    /// `grand_total - paid_total`. Computed here rather than stored, because a stored outstanding
    /// is a second copy of a subtraction and every copy is a chance for the two to disagree.
    pub outstanding: String,
    /// How many days past the due date it is, `0` when it is not past due. The aging report
    /// groups on this, and the list screen's red/amber hint reads it.
    pub days_past_due: i32,
    /// When it was sent, if it has been.
    #[serde(with = "crate::dates::instant::option")]
    pub sent_at: Option<OffsetDateTime>,
    /// When the last payment landed.
    #[serde(with = "crate::dates::instant::option")]
    pub last_payment_at: Option<OffsetDateTime>,
    /// When it was paid in full, if it has been.
    #[serde(with = "crate::dates::instant::option")]
    pub paid_at: Option<OffsetDateTime>,
    /// When it was voided, if it was.
    #[serde(with = "crate::dates::instant::option")]
    pub voided_at: Option<OffsetDateTime>,
    /// Why it was voided. Required by the route; the column defaults to `''` for a row that never
    /// was, and a screen must not have to tell those two apart by the shape of the value.
    pub void_reason: String,
    /// When the sweep first flipped it to overdue. The idempotence anchor: an invoice with a
    /// value here has been announced and will not be announced again.
    #[serde(with = "crate::dates::instant::option")]
    pub overdue_at: Option<OffsetDateTime>,
    /// The lines, in order.
    pub lines: Vec<InvoiceLineView>,
    /// When the row was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
    /// When the row last changed.
    #[serde(with = "crate::dates::instant")]
    pub updated_at: OffsetDateTime,
}

impl InvoiceView {
    /// The compact reference an audit row and an event payload carry.
    ///
    /// Deliberately small: an event travels to every webhook subscriber, so it carries the id, the
    /// number, the status, the currency and the three amounts — not the lines and not the notes.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "invoice_id": self.id,
            "number": self.number,
            "status": self.status.as_str(),
            "currency": self.currency,
            "grand_total": self.grand_total,
            "paid_total": self.paid_total,
            "outstanding": self.outstanding,
            "order_id": self.order_id,
        })
    }

    /// How many lines the invoice carries, for the list column and the PDF header.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Whether the document may still be edited.
    #[must_use]
    pub fn is_editable(&self) -> bool {
        self.status.is_editable()
    }
}

/// An invoice without its lines — what the list screen draws.
///
/// The list does not ship the lines on purpose: a hundred rows of five lines each is five hundred
/// rows a person never looked at, and the list screen's job is a number, a customer, a due date,
/// three amounts and a badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvoiceSummary {
    /// The row's id.
    pub id: Uuid,
    /// The per-organization number.
    pub number: String,
    /// The customer's name as the document says it.
    pub customer_name: String,
    /// Where it is in its life.
    pub status: InvoiceStatus,
    /// The currency.
    pub currency: String,
    /// The day it is dated.
    #[serde(with = "crate::dates")]
    pub issue_date: Date,
    /// The day it is due.
    #[serde(with = "crate::dates::option")]
    pub due_date: Option<Date>,
    /// The stored total.
    pub grand_total: String,
    /// How much has been paid.
    pub paid_total: String,
    /// `grand_total - paid_total`.
    pub outstanding: String,
    /// Days past the due date; the list's red/amber hint.
    pub days_past_due: i32,
    /// The order it came from, when it came from one.
    pub order_id: Option<Uuid>,
    /// How many lines it carries.
    pub line_count: i64,
    /// When the row last changed.
    #[serde(with = "crate::dates::instant")]
    pub updated_at: OffsetDateTime,
}

impl InvoiceSummary {
    /// Read one row of the list query.
    ///
    /// `row.get` is a **runtime** lookup by column name, so a field that does not match the
    /// alias in the SQL compiles, passes the unit tests and 500s on the list — the one route that
    /// has no test that reads a body it forgot to print. The names here and the aliases in
    /// [`list_invoices`] are the same contract, which is why the two live in one file.
    pub(crate) fn from_row(row: &PgRow) -> Result<Self> {
        let status_text: String = row.get("invoice_status");
        let status = InvoiceStatus::parse(&status_text).ok_or_else(|| {
            // A status the schema's CHECK forbids is a row this build cannot read. Naming the
            // value is the difference between a 500 a person can report and one they cannot.
            AccountingError::not_allowed(format!(
                "invoice {} carries the unknown status {status_text:?}",
                row.get::<Uuid, _>("id")
            ))
        })?;

        Ok(Self {
            id: row.get("id"),
            number: row.get("number"),
            customer_name: row.get("customer_name"),
            status,
            currency: row.get("currency"),
            issue_date: row.get("issue_date"),
            due_date: row.get("due_date"),
            grand_total: row.get("grand_total"),
            paid_total: row.get("paid_total"),
            outstanding: outstanding_text(
                row.get("grand_total"),
                row.get("paid_total"),
            )?,
            days_past_due: row.get("days_past_due"),
            order_id: row.get("order_id"),
            line_count: row.get("line_count"),
            updated_at: row.get("updated_at"),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

/// One line of the invoice form.
///
/// Every numeric field is text because `numeric` has no Rust type in this workspace — the same
/// shape the journal, the sales and the inventory modules all wrote, for the same reason.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewInvoiceLine {
    /// The catalog product, when the line came from one. Optional: a manual invoice may bill a
    /// description that is not on the price list.
    #[serde(default)]
    pub product_id: Option<Uuid>,
    /// What is being billed. Required unless a product is named.
    #[serde(default)]
    pub description: Option<String>,
    /// How many. Must be greater than zero — a zero line is a heading wearing a line's clothes.
    #[serde(default)]
    pub qty: Option<String>,
    /// The price of one, before the discount. Must not be negative.
    #[serde(default)]
    pub unit_price: Option<String>,
    /// The percentage taken off. 0–100.
    #[serde(default)]
    pub discount_percent: Option<String>,
    /// The percentage of tax. 0–100.
    #[serde(default)]
    pub tax_percent: Option<String>,
}

impl NewInvoiceLine {
    /// The line priced out, rounded once, with its discount and tax split out.
    ///
    /// This is the only place an invoice line's arithmetic happens, and the order is the rule the
    /// whole workspace inherited: **discount before tax.** A line of 100.00 with a 10% discount
    /// and 20% tax is 90.00 of net, 18.00 of tax, 108.00 of gross. Taxing the pre-discount amount
    /// and then discounting the taxed amount gives the same gross here and differs by a cent in
    /// enough cases to be a reconciliation problem, so the two steps are written in that order
    /// here and nowhere else.
    fn priced(&self) -> Result<PricedLine> {
        let qty = parse_qty(self.qty.as_deref().unwrap_or("1"))?;
        let unit_price = Amount::parse(self.unit_price.as_deref().unwrap_or("0"))
            .map_err(|source| AccountingError::number("invoice", "unit_price", source))?;
        if unit_price.cents() < 0 {
            return Err(AccountingError::invalid(
                "invoice",
                "unit_price",
                "a unit price is zero or more — a negative price is a credit note, not a line",
            ));
        }

        let discount_percent = parse_percent(
            self.discount_percent.as_deref().unwrap_or("0"),
            "discount_percent",
        )?;
        let tax_percent = parse_percent(self.tax_percent.as_deref().unwrap_or("0"), "tax_percent")?;

        let description = normalize_description(self.description.as_deref(), self.product_id)?;

        // qty is thousandths, so the gross multiplication happens in the same integer space.
        // `multiply_qty` is on `Amount` and rounds half-away-from-zero once, which is the
        // difference between 33.335 and 33.34 and is the reason the PDF and the panel agree.
        let gross = unit_price.multiply_qty(qty);
        let discount = gross.percent_of(discount_percent);
        let net = gross.minus(discount);
        let tax = net.percent_of(tax_percent);
        let line_total = net.plus(tax);

        Ok(PricedLine {
            description,
            qty_text: format_qty(qty),
            unit_price: unit_price.to_text(),
            discount_percent: percent_text(discount_percent),
            tax_percent: percent_text(tax_percent),
            line_total: line_total.to_text(),
            tax_amount: tax.to_text(),
        })
    }
}

/// A line after [`NewInvoiceLine::priced`], with everything the insert needs.
struct PricedLine {
    description: String,
    qty_text: String,
    unit_price: String,
    discount_percent: String,
    tax_percent: String,
    line_total: String,
    tax_amount: String,
}

/// The body of `POST /accounting/invoices` and the `PATCH` that replaces a draft's lines.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewInvoice {
    /// The company being billed. Either this or a contact names the customer.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// The contact being billed, when the invoice names a person.
    #[serde(default)]
    pub contact_id: Option<Uuid>,
    /// The sales order to convert. When present the order's lines are copied and the invoice is
    /// linked to it, and a second draft for the same order is refused.
    #[serde(default)]
    pub order_id: Option<Uuid>,
    /// The customer's name as the document says it. Copied from the company or the order when
    /// the caller omits it, so a renamed customer does not rewrite issued documents.
    #[serde(default)]
    pub customer_name: Option<String>,
    /// The day the invoice is dated. Today when absent.
    #[serde(default)]
    pub issue_date: Option<String>,
    /// The day it is due. `issue_date + DEFAULT_DUE_DAYS` when absent, and never before the
    /// issue date.
    #[serde(default)]
    pub due_date: Option<String>,
    /// The currency. `USD` when absent; the schema's `^[A-Z]{3}$` is the authority.
    #[serde(default)]
    pub currency: Option<String>,
    /// The terms printed on the document.
    #[serde(default)]
    pub payment_terms: Option<String>,
    /// The customer's own reference.
    #[serde(default)]
    pub reference: Option<String>,
    /// A free-text note.
    #[serde(default)]
    pub notes: Option<String>,
    /// The lines. Required unless an order supplies them.
    #[serde(default)]
    pub lines: Vec<NewInvoiceLine>,
}

// ---------------------------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------------------------

/// Create an invoice, from lines or from a sales order.
///
/// The two paths share one function on purpose. An invoice converted from an order is not a
/// different kind of document with a different set of rules — it is the same document whose lines
/// were copied — and a second code path is a second set of bugs.
pub async fn create_invoice(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewInvoice,
    created_by: Option<Uuid>,
) -> Result<InvoiceView> {
    let issue_date = match new.issue_date.as_deref() {
        None | Some("") => OffsetDateTime::now_utc().date(),
        Some(raw) => crate::dates::parse(raw).map_err(|_| {
            AccountingError::invalid(
                "invoice",
                "issue_date",
                format!("a date such as 2026-12-01, not {raw:?} — the format is YYYY-MM-DD"),
            )
        })?,
    };

    let mut tx = pool.begin().await?;

    // The order is read **inside** the transaction and before the insert, for two reasons: the
    // second-draft guard is a `select` that has to be serialised against a concurrent conversion
    // of the same order, and the number is allocated from the same transaction as the lines so a
    // rolled-back invoice does not burn a number.
    let from_order = match new.order_id {
        Some(order_id) => Some(read_order_for_invoice(&mut tx, organization_id, order_id).await?),
        None => None,
    };

    if let Some(order) = &from_order {
        if !order.convertible {
            return Err(AccountingError::not_allowed(format!(
                "order {} is {} — only a confirmed or already-invoiced order becomes an invoice",
                order.number, order.status
            )));
        }
        if order.invoice_id.is_some() {
            return Err(AccountingError::not_allowed(format!(
                "order {} already has invoice {} — void it or duplicate it instead of \
                 converting the order twice",
                order.number,
                order.invoice_id.expect("checked above")
            )));
        }
    }

    let due_date = resolve_due_date(new.due_date.as_deref(), issue_date)?;

    // Lines: the order's when converting, the submitted ones otherwise. A conversion that also
    // carries lines is refused rather than merged — silently preferring one over the other is
    // how an invoice for 1,200 is created for an order of 900.
    let source_lines: Vec<NewInvoiceLine> = match &from_order {
        Some(order) => {
            if !new.lines.is_empty() {
                return Err(AccountingError::invalid(
                    "invoice",
                    "lines",
                    "an invoice converted from an order takes the order's lines — \
                     remove `lines` from the request",
                ));
            }
            order.lines.clone()
        }
        None => new.lines.clone(),
    };

    if source_lines.is_empty() {
        return Err(AccountingError::invalid(
            "invoice",
            "lines",
            "an invoice needs at least one line",
        ));
    }
    if source_lines.len() > MAX_LINES {
        return Err(AccountingError::invalid(
            "invoice",
            "lines",
            format!("an invoice carries at most {MAX_LINES} lines"),
        ));
    }

    // Price every line once, here, and keep the three totals in integer hundredths. The client's
    // arithmetic is never read — it does not exist on this struct, which is the point.
    let mut subtotal = Amount::ZERO;
    let mut discount_total = Amount::ZERO;
    let mut tax_total = Amount::ZERO;
    let mut priced = Vec::with_capacity(source_lines.len());
    for line in &source_lines {
        let priced_line = line.priced()?;
        let gross = gross_of(&priced_line);
        subtotal = subtotal.plus(gross);
        discount_total = discount_total.plus(discount_of(&priced_line));
        tax_total = tax_total.plus(Amount::parse(&priced_line.tax_amount).unwrap_or(Amount::ZERO));
        priced.push((line, priced_line));
    }
    let grand_total = subtotal.minus(discount_total).plus(tax_total);

    let customer_name = resolve_customer_name(pool, new, from_order.as_ref()).await?;
    let currency = normalize_currency(new.currency.as_deref())?;

    // The three free-text fields are normalised **before** the statement is built, not inside the
    // `.bind()` chain. `bind` takes a value, so a `?` in that position binds the `Result` itself —
    // and the compiler's complaint (`Result<String, AccountingError>: Encode` is not satisfied) is
    // three lines away from the cause. Normalising here also means the refusal happens before any
    // statement is sent, which is the property the whole create path is built on.
    let payment_terms = normalize_note(new.payment_terms.as_deref(), "payment_terms")?;
    let reference = normalize_note(new.reference.as_deref(), "reference")?;
    let notes = normalize_note(new.notes.as_deref(), "notes")?;

    let invoice_number = next_invoice_number(&mut tx, organization_id).await?;
    let number = format!("INV-{invoice_number:06}");

    let invoice_id: Uuid = sqlx::query_scalar(
        "insert into accounting_invoices \
             (organization_id, number, order_id, company_id, contact_id, customer_name, \
              invoice_status, currency, issue_date, due_date, payment_terms, reference, notes, \
              subtotal, discount_total, tax_total, grand_total, created_by) \
         values ($1, $2, $3, $4, $5, $6, 'draft', $7, $8, $9, $10, $11, $12, \
                 $13::numeric, $14::numeric, $15::numeric, $16::numeric, $17) \
         returning id",
    )
    .bind(organization_id)
    .bind(&number)
    .bind(new.order_id)
    .bind(new.company_id.or_else(|| from_order.as_ref().and_then(|o| o.customer_id)))
    .bind(new.contact_id)
    .bind(&customer_name)
    .bind(&currency)
    .bind(issue_date)
    .bind(due_date)
    .bind(payment_terms)
    .bind(reference)
    .bind(notes)
    .bind(subtotal.to_text())
    .bind(discount_total.to_text())
    .bind(tax_total.to_text())
    .bind(grand_total.to_text())
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await?;

    // One statement at a time, for the same reason the journal does it: a generated bulk INSERT
    // would abort the whole statement on the first line that trips a CHECK and name no line.
    for (position, (line, priced_line)) in priced.iter().enumerate() {
        sqlx::query(
            "insert into accounting_invoice_lines \
                 (invoice_id, organization_id, position, product_id, description, qty, \
                  unit_price, discount_percent, tax_percent, line_total) \
             values ($1, $2, $3, $4, $5, $6::numeric, $7::numeric, $8::numeric, $9::numeric, \
                     $10::numeric)",
        )
        .bind(invoice_id)
        .bind(organization_id)
        .bind(i32::try_from(position + 1).unwrap_or(i32::MAX))
        .bind(line.product_id)
        .bind(&priced_line.description)
        .bind(&priced_line.qty_text)
        .bind(&priced_line.unit_price)
        .bind(&priced_line.discount_percent)
        .bind(&priced_line.tax_percent)
        .bind(&priced_line.line_total)
        .execute(&mut *tx)
        .await?;
    }

    // A converted order says so, in the sales module's own words. The column is text rather than
    // a boolean because sales tracks three states ('none', 'draft', 'issued') and the invoice's
    // job is to move it to the second one.
    if new.order_id.is_some() {
        sqlx::query("update sales_orders set invoice_state = 'draft', updated_at = now() \
                     where id = $1 and organization_id = $2")
            .bind(new.order_id.expect("guarded above"))
            .bind(organization_id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    get_invoice(pool, organization_id, invoice_id).await
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

/// One invoice with its lines, or `404` — which is also the answer for another organization's.
pub async fn get_invoice(
    pool: &PgPool,
    organization_id: Uuid,
    invoice_id: Uuid,
) -> Result<InvoiceView> {
    let row = sqlx::query(
        "select i.id, i.organization_id, i.number, i.order_id, o.number as order_number, \
                i.company_id, i.contact_id, \
                coalesce(c.name, ct.full_name, i.customer_name) as customer_name, \
                i.invoice_status, i.currency, i.issue_date, i.due_date, i.payment_terms, \
                i.reference, i.notes, i.subtotal::text as subtotal, \
                i.discount_total::text as discount_total, i.tax_total::text as tax_total, \
                i.grand_total::text as grand_total, i.paid_total::text as paid_total, \
                i.sent_at, i.last_payment_at, i.paid_at, i.voided_at, i.void_reason, \
                i.overdue_at, i.created_at, i.updated_at \
         from accounting_invoices i \
         left join sales_orders o on o.id = i.order_id \
         left join crm_companies c on c.id = i.company_id \
         left join crm_contacts ct on ct.id = i.contact_id \
         where i.organization_id = $1 and i.id = $2",
    )
    .bind(organization_id)
    .bind(invoice_id)
    .fetch_optional(pool)
    .await?
    .ok_or(AccountingError::NotFound("invoice"))?;

    let id: Uuid = row.get("id");
    let status_text: String = row.get("invoice_status");
    let status = InvoiceStatus::parse(&status_text).ok_or_else(|| {
        AccountingError::not_allowed(format!("invoice {id} carries the unknown status {status_text:?}"))
    })?;

    let grand_total: String = row.get("grand_total");
    let paid_total: String = row.get("paid_total");
    let due_date: Option<Date> = row.get("due_date");

    Ok(InvoiceView {
        id,
        organization_id: row.get("organization_id"),
        number: row.get("number"),
        order_id: row.get("order_id"),
        order_number: row.get("order_number"),
        company_id: row.get("company_id"),
        contact_id: row.get("contact_id"),
        customer_name: row.get("customer_name"),
        status,
        currency: row.get("currency"),
        issue_date: row.get("issue_date"),
        due_date,
        payment_terms: row.get("payment_terms"),
        reference: row.get("reference"),
        notes: row.get("notes"),
        subtotal: row.get("subtotal"),
        discount_total: row.get("discount_total"),
        tax_total: row.get("tax_total"),
        outstanding: outstanding_text(&grand_total, &paid_total)?,
        grand_total,
        paid_total,
        days_past_due: days_past_due(status, due_date),
        sent_at: row.get("sent_at"),
        last_payment_at: row.get("last_payment_at"),
        paid_at: row.get("paid_at"),
        voided_at: row.get("voided_at"),
        void_reason: row.get("void_reason"),
        overdue_at: row.get("overdue_at"),
        lines: load_lines(pool, id).await?,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

/// A page of invoices, newest first.
///
/// Filters are the ones a bookkeeper actually filters by: a status, a day range, and free text over
/// the number and the customer. The `days_past_due` and `outstanding` columns are **computed in
/// SQL** rather than assembled in Rust from the two amounts, because the aging report and this
/// list have to agree about what "31 days past due" means and one definition written twice is two
/// definitions.
pub async fn list_invoices(
    pool: &PgPool,
    organization_id: Uuid,
    status: Option<InvoiceStatus>,
    from: Option<Date>,
    to: Option<Date>,
    overdue_only: bool,
    search: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<InvoiceSummary>> {
    let limit = limit
        .unwrap_or(crate::store::DEFAULT_PER_PAGE)
        .clamp(1, crate::store::MAX_PER_PAGE);

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "select i.id, i.number, \
                coalesce(c.name, ct.full_name, i.customer_name) as customer_name, \
                i.invoice_status, i.currency, i.issue_date, i.due_date, \
                i.grand_total::text as grand_total, i.paid_total::text as paid_total, \
                (i.grand_total - i.paid_total)::text as outstanding, \
                case when i.due_date is not null \
                     and i.invoice_status in ('sent', 'partial', 'overdue') \
                     and i.due_date < current_date \
                     then (current_date - i.due_date)::int else 0 end as days_past_due, \
                i.order_id, \
                (select count(*) from accounting_invoice_lines l where l.invoice_id = i.id) \
                    as line_count, \
                i.updated_at \
         from accounting_invoices i \
         left join crm_companies c on c.id = i.company_id \
         left join crm_contacts ct on ct.id = i.contact_id \
         where i.organization_id = ",
    );
    // **No hand-written placeholders and no always-on nullable predicate.** `QueryBuilder`
    // renumbers every bind itself, so a literal `$2` in the pushed SQL is a dollar-quoted token
    // rather than "the second bind" and the query dies with `syntax error at or near "$2"`. The
    // shape that survives: push the predicate ONLY when the filter is present. (Two earlier
    // versions of this family's filters failed in opposite directions for exactly this reason —
    // see `journal::list_entries`.)
    builder.push_bind(organization_id);
    if let Some(status) = status {
        builder.push(" and i.invoice_status = ");
        builder.push_bind(status.as_str());
    }
    if let Some(from) = from {
        builder.push(" and i.issue_date >= ");
        builder.push_bind(from);
    }
    if let Some(to) = to {
        builder.push(" and i.issue_date <= ");
        builder.push_bind(to);
    }
    if overdue_only {
        // The same predicate the `days_past_due` expression uses, spelled once more rather than
        // wrapping the whole query: a filter that re-implements the derived column is a filter
        // that can disagree with the column it filters.
        builder.push(
            " and i.due_date is not null and i.due_date < current_date \
             and i.invoice_status in ('sent', 'partial', 'overdue')",
        );
    }
    if let Some(term) = search.map(str::trim).filter(|t| !t.is_empty()) {
        if term.chars().count() > crate::store::MAX_SEARCH_LENGTH {
            return Err(AccountingError::invalid(
                "invoice",
                "search",
                format!("a search is at most {} characters", crate::store::MAX_SEARCH_LENGTH),
            ));
        }
        builder.push(" and (i.number ilike ");
        builder.push_bind(format!("%{term}%"));
        builder.push(" or coalesce(c.name, ct.full_name, i.customer_name) ilike ");
        builder.push_bind(format!("%{term}%"));
        builder.push(")");
    }
    builder.push(" order by i.issue_date desc, i.number desc limit ");
    builder.push_bind(limit);

    let rows = builder.build().fetch_all(pool).await?;
    rows.iter().map(InvoiceSummary::from_row).collect()
}

// ---------------------------------------------------------------------------------------------
// The transitions
// ---------------------------------------------------------------------------------------------

/// Mark a draft sent, stamping `sent_at`.
///
/// Refuses a second send, and says why in the message: the customer has the document, so what is
/// wanted next is a void and a duplicate, not a re-send that would give the same number two
/// different issues.
pub async fn send_invoice(
    pool: &PgPool,
    organization_id: Uuid,
    invoice_id: Uuid,
) -> Result<InvoiceView> {
    let mut tx = pool.begin().await?;
    let invoice = lock_invoice(&mut tx, organization_id, invoice_id).await?;

    match invoice.status {
        InvoiceStatus::Draft => {}
        InvoiceStatus::Void => {
            return Err(AccountingError::not_allowed(format!(
                "invoice {} is void — a withdrawn document is not sent again",
                invoice.number
            )));
        }
        other => {
            return Err(AccountingError::not_allowed(format!(
                "invoice {} is already {} — send applies to a draft",
                invoice.number,
                other.label()
            )));
        }
    }

    sqlx::query(
        "update accounting_invoices set invoice_status = 'sent', sent_at = now(), \
                updated_at = now() where id = $1 and organization_id = $2",
    )
    .bind(invoice_id)
    .bind(organization_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    get_invoice(pool, organization_id, invoice_id).await
}

/// Void an invoice, keeping its number.
///
/// The number is the reason this is not a delete: `INV-0007` was quoted on a purchase order, a
/// customer's ledger and a support ticket. Voiding leaves that reference resolving to a document
/// that says "withdrawn, and here is why"; deleting it leaves the reference pointing at nothing.
pub async fn void_invoice(
    pool: &PgPool,
    organization_id: Uuid,
    invoice_id: Uuid,
    reason: &str,
) -> Result<InvoiceView> {
    let reason = normalize_note(Some(reason), "reason").map_err(|error| match error {
        // A void with no reason is the one thing this route will not do: "withdrawn" with no
        // sentence attached is indistinguishable from a mistake, and the row is permanent.
        _ => AccountingError::invalid(
            "invoice",
            "reason",
            "a void carries a reason — it is what the audit row and the customer are given",
        ),
    })?;
    if reason.is_empty() {
        return Err(AccountingError::invalid(
            "invoice",
            "reason",
            "a void carries a reason — it is what the audit row and the customer are given",
        ));
    }

    let mut tx = pool.begin().await?;
    let invoice = lock_invoice(&mut tx, organization_id, invoice_id).await?;

    if invoice.status == InvoiceStatus::Void {
        return Err(AccountingError::not_allowed(format!(
            "invoice {} is already void ({})",
            invoice.number, invoice.void_reason
        )));
    }
    if invoice.status == InvoiceStatus::Paid {
        return Err(AccountingError::not_allowed(format!(
            "invoice {} is paid — a paid document is reversed by a credit note, not voided",
            invoice.number
        )));
    }
    if !invoice.paid_total.is_zero() && invoice.status != InvoiceStatus::Draft {
        return Err(AccountingError::not_allowed(format!(
            "invoice {} has {} recorded against it — void the payments first, or credit note it",
            invoice.number, invoice.paid_total
        )));
    }

    sqlx::query(
        "update accounting_invoices set invoice_status = 'void', voided_at = now(), \
                void_reason = $3, updated_at = now() \
         where id = $1 and organization_id = $2",
    )
    .bind(invoice_id)
    .bind(organization_id)
    .bind(&reason)
    .execute(&mut *tx)
    .await?;

    // A converted order goes back to being un-invoiced, because the document that satisfied it
    // has been withdrawn and the sales screen must not keep claiming otherwise.
    if let Some(order_id) = invoice.order_id {
        sqlx::query(
            "update sales_orders set invoice_state = 'none', updated_at = now() \
             where id = $1 and organization_id = $2",
        )
        .bind(order_id)
        .bind(organization_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    get_invoice(pool, organization_id, invoice_id).await
}

/// Flip past-due invoices to `overdue`, once each, and return what it changed.
///
/// **Idempotence is the whole design.** The sweep runs on a schedule, so it will see the same
/// invoice on every tick until somebody pays it; a sweep that emitted `accounting.invoice.overdue`
/// each time would fire the documented automation — an e-mail, a task, a notification — once per
/// tick. The guard is the `overdue_at is null` predicate inside the same UPDATE that flips the
/// status, so two sweeps racing cannot both win, and the returned rows are exactly the ones that
/// changed and therefore exactly the ones that should be announced.
pub async fn sweep_overdue(pool: &PgPool, organization_id: Uuid) -> Result<Vec<Uuid>> {
    // The status is flipped and `overdue_at` stamped by ONE statement, and the predicate is part
    // of it. A `select` followed by an `update` would race: two sweeps would both read the same
    // unpaid row and both announce it.
    let rows = sqlx::query(
        "update accounting_invoices \
             set invoice_status = 'overdue', overdue_at = now(), updated_at = now() \
         where organization_id = $1 \
           and overdue_at is null \
           and due_date is not null \
           and due_date < current_date \
           and invoice_status in ('sent', 'partial') \
         returning id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|row| row.get("id")).collect())
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// What the module needs from a sales order, read inside the create transaction.
struct OrderForInvoice {
    /// The order's number, for the second-draft message and the detail screen.
    number: String,
    /// Its status, for the convertible check.
    status: String,
    /// The invoice already made from it, if any.
    invoice_id: Option<Uuid>,
    /// The customer, when the order names a company.
    customer_id: Option<Uuid>,
    /// The lines to copy.
    lines: Vec<NewInvoiceLine>,
    /// Whether the order's status is one an invoice may be made from.
    convertible: bool,
}

/// Read an order and the lines an invoice would copy.
async fn read_order_for_invoice(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    organization_id: Uuid,
    order_id: Uuid,
) -> Result<OrderForInvoice> {
    let row = sqlx::query(
        "select id, number, status, customer_id, customer_type \
         from sales_orders where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(order_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AccountingError::ForeignKey {
        kind: "sales_order",
        id: order_id,
    })?;

    let status: String = row.get("status");
    let customer_type: String = row.get("customer_type");
    let lines = sqlx::query(
        "select product_id, description, quantity::text as qty, \
                unit_price::text as unit_price, discount_percent::text as discount_percent, \
                tax_percent::text as tax_percent \
         from sales_order_lines where order_id = $1 order by position",
    )
    .bind(order_id)
    .fetch_all(&mut **tx)
    .await?;

    Ok(OrderForInvoice {
        number: row.get("number"),
        // A cancelled order is not a sale, and a delivered one has already been invoiced by
        // whatever workflow did it — either way this is not a draft to create.
        convertible: matches!(status.as_str(), "confirmed" | "delivered" | "invoiced"),
        status,
        invoice_id: sqlx::query_scalar(
            "select id from accounting_invoices where organization_id = $1 and order_id = $2 \
             and invoice_status <> 'void' limit 1",
        )
        .bind(organization_id)
        .bind(order_id)
        .fetch_optional(&mut **tx)
        .await?,
        customer_id: if customer_type == "company" {
            row.get("customer_id")
        } else {
            None
        },
        lines: lines
            .into_iter()
            .map(|line| NewInvoiceLine {
                product_id: line.get("product_id"),
                description: line.get("description"),
                qty: line.get("qty"),
                unit_price: line.get("unit_price"),
                discount_percent: line.get("discount_percent"),
                tax_percent: line.get("tax_percent"),
            })
            .collect(),
    })
}

/// The invoice's current row, locked for update, or `404`.
struct LockedInvoice {
    number: String,
    status: InvoiceStatus,
    paid_total: Amount,
    order_id: Option<Uuid>,
    void_reason: String,
}

/// Take the row's lock so two sends (or a send racing a void) cannot both see a draft.
async fn lock_invoice(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    organization_id: Uuid,
    invoice_id: Uuid,
) -> Result<LockedInvoice> {
    let row = sqlx::query(
        "select number, invoice_status, paid_total::text as paid_total, order_id, void_reason \
         from accounting_invoices where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(invoice_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AccountingError::NotFound("invoice"))?;

    let status_text: String = row.get("invoice_status");
    let status = InvoiceStatus::parse(&status_text).ok_or_else(|| {
        AccountingError::not_allowed(format!(
            "invoice {} carries the unknown status {status_text:?}",
            row.get::<String, _>("number")
        ))
    })?;
    let paid_text: String = row.get("paid_total");

    Ok(LockedInvoice {
        number: row.get("number"),
        status,
        paid_total: Amount::parse(&paid_text).unwrap_or(Amount::ZERO),
        order_id: row.get("order_id"),
        void_reason: row.get("void_reason"),
    })
}

/// The next invoice number for an organization.
///
/// Per organization and inside the caller's transaction, so a rolled-back create does not burn a
/// number: `INV-0008` that exists is a number somebody was given, and a gap in the sequence is
/// visible in a way a duplicate is not.
async fn next_invoice_number(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    organization_id: Uuid,
) -> Result<i64> {
    sqlx::query_scalar(
        "select coalesce(max((regexp_match(number, '[0-9]+'))[1]::bigint), 0) + 1 \
         from accounting_invoices where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(Into::into)
}

/// The due date: the caller's, or the issue date plus the default terms.
fn resolve_due_date(raw: Option<&str>, issue_date: Date) -> Result<Option<Date>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(Some(issue_date + time::Duration::days(DEFAULT_DUE_DAYS))),
        Some(value) => {
            let due = crate::dates::parse(value).map_err(|_| {
                AccountingError::invalid(
                    "invoice",
                    "due_date",
                    format!("a date such as 2026-12-01, not {value:?} — the format is YYYY-MM-DD"),
                )
            })?;
            if due < issue_date {
                return Err(AccountingError::invalid(
                    "invoice",
                    "due_date",
                    format!(
                        "a due date is not before the issue date: {} is before {}",
                        crate::dates::to_wire(&due),
                        crate::dates::to_wire(&issue_date)
                    ),
                ));
            }
            Ok(Some(due))
        }
    }
}

/// The customer's name: the caller's, the CRM's, or a refusal.
///
/// `async` because it reads the CRM's name when the caller named a company or a contact rather
/// than spelling the customer out.
async fn resolve_customer_name(
    pool: &PgPool,
    new: &NewInvoice,
    order: Option<&OrderForInvoice>,
) -> Result<String> {
    if let Some(name) = new
        .customer_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(truncate(name, MAX_DESCRIPTION_LENGTH));
    }
    // A company or a contact named directly: read the name here rather than making the client send
    // a second request to display it, and so the document says the name **at issue time** even if
    // the CRM record is renamed tomorrow.
    if let Some(company_id) = new.company_id {
        if let Some(name) = fetch_name(pool, "crm_companies", "name", company_id).await {
            return Ok(name);
        }
    }
    if let Some(contact_id) = new.contact_id {
        if let Some(name) = fetch_name(pool, "crm_contacts", "full_name", contact_id).await {
            return Ok(name);
        }
    }
    Err(AccountingError::invalid(
        "invoice",
        "customer",
        "an invoice names a customer — pass `customer_name`, or a `company_id`/`contact_id`",
    ))
}

/// Read a display name from the CRM, if the row is there.
///
/// Returns `None` rather than an error for a row that is not there, so a draft whose customer was
/// deleted still opens and can be renamed — the alternative is a document that cannot be edited
/// because of a record it is not responsible for.
async fn fetch_name(pool: &PgPool, table: &str, column: &str, id: Uuid) -> Option<String> {
    // `table` and `column` are **never** caller input: both are string literals at the two call
    // sites above. A caller that could name them would be a caller that could read
    // `pg_authid`, so the helper takes them as literals and the two uses in this file are the
    // proof that it stays a fixed set.
    let sql = format!("select {column} from {table} where id = $1");
    sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// `grand_total - paid_total`, as text.
fn outstanding_text(grand_total: &str, paid_total: &str) -> Result<String> {
    let gross = Amount::parse(grand_total)
        .map_err(|source| AccountingError::number("invoice", "grand_total", source))?;
    let paid = Amount::parse(paid_total)
        .map_err(|source| AccountingError::number("invoice", "paid_total", source))?;
    Ok(gross.minus(paid).to_text())
}

/// Days past the due date, `0` when the invoice is not past due.
fn days_past_due(status: InvoiceStatus, due_date: Option<Date>) -> i32 {
    // Only a receivable can be past due: a draft nobody has seen and a paid invoice are both
    // "late" by the calendar and neither is owed anything.
    if !status.is_receivable() {
        return 0;
    }
    let today = OffsetDateTime::now_utc().date();
    match due_date {
        Some(due) if due < today => i32::try_from((today - due).whole_days()).unwrap_or(i32::MAX),
        _ => 0,
    }
}

/// The gross of a priced line — `line_total` plus its tax, which is the pre-discount, pre-tax
/// figure the `subtotal` column holds.
fn gross_of(line: &PricedLine) -> Amount {
    let total = Amount::parse(&line.line_total).unwrap_or(Amount::ZERO);
    let tax = Amount::parse(&line.tax_amount).unwrap_or(Amount::ZERO);
    total.minus(tax)
}

/// The discount of a priced line — the gross less the pre-tax net.
fn discount_of(line: &PricedLine) -> Amount {
    let net = Amount::parse(&line.line_total)
        .unwrap_or(Amount::ZERO)
        .minus(Amount::parse(&line.tax_amount).unwrap_or(Amount::ZERO));
    let gross = gross_of(line);
    gross.minus(net)
}

/// A currency code, upper-cased and checked.
fn normalize_currency(raw: Option<&str>) -> Result<String> {
    let value = raw
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("USD")
        .to_ascii_uppercase();
    if value.len() != 3 || !value.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(AccountingError::invalid(
            "invoice",
            "currency",
            format!("a currency is three letters, such as USD or TRY — not {value:?}"),
        ));
    }
    Ok(value)
}

/// A free-text field: trimmed, bounded, and never silently truncated past the bound.
fn normalize_note(raw: Option<&str>, field: &'static str) -> Result<String> {
    let value = raw.unwrap_or_default().trim();
    if value.chars().count() > MAX_NOTE_LENGTH {
        return Err(AccountingError::invalid(
            "invoice",
            field,
            format!("at most {MAX_NOTE_LENGTH} characters"),
        ));
    }
    Ok(value.to_owned())
}

/// A line's description, or a refusal when it has neither a product nor words.
fn normalize_description(raw: Option<&str>, product_id: Option<Uuid>) -> Result<String> {
    let value = raw.unwrap_or_default().trim();
    if value.is_empty() && product_id.is_none() {
        return Err(AccountingError::invalid(
            "invoice",
            "description",
            "a line names what it bills — give a `description` or a `product_id`",
        ));
    }
    if value.chars().count() > MAX_DESCRIPTION_LENGTH {
        return Err(AccountingError::invalid(
            "invoice",
            "description",
            format!("at most {MAX_DESCRIPTION_LENGTH} characters"),
        ));
    }
    Ok(value.to_owned())
}

/// Cut a string to `max` characters without splitting a code point.
fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// A quantity in **thousandths**, from the form's text.
///
/// The column is `numeric(14,3)` and the arithmetic is integer, so `1.5` becomes `1500` here and
/// `33.335` becomes `33335` — a quantity is never a float on the way to a price, because a
/// float multiplied by a price is a line total nobody can reproduce on a PDF.
///
/// Three decimal places is the whole scale, and a fourth is a **refusal** for the same reason
/// `Amount::parse` refuses a third: a person typing `0.0001` is asking a question the column
/// cannot answer, and answering it by dropping a digit hands them a different number.
fn parse_qty(raw: &str) -> Result<i64> {
    let value = raw.trim();
    // Three decimals, then the value is already a count of thousandths.
    let parsed = scaled_decimal(value, 3).ok_or_else(|| {
        AccountingError::invalid(
            "invoice",
            "qty",
            format!("a quantity such as 2 or 1.5, not {value:?}"),
        )
    })?;
    if parsed <= 0 {
        return Err(AccountingError::invalid(
            "invoice",
            "qty",
            "a quantity is greater than zero — a zero line is a heading, not a line",
        ));
    }
    Ok(parsed)
}

/// A percent in **hundredths of a percent**, from the form's text.
///
/// `20` is `2000` and `2.5` is `250`. Two decimals is the scale `numeric(5,2)` stores, and a
/// third is refused rather than rounded: 7.777% of a line is a figure the person did not type.
fn parse_percent(raw: &str, field: &'static str) -> Result<i64> {
    let value = raw.trim();
    // Two decimals gives the count of hundredths of a percent directly: `20` parses as `20.00` ->
    // 2000, which is 20%. No further scaling — multiplying again would read 20% as 200%, which is
    // the sort of off-by-ten that makes a VAT return wrong in a way nobody spots until a filing.
    let parsed = scaled_decimal(value, 2).ok_or_else(|| {
        AccountingError::invalid(
            "invoice",
            field,
            format!("a percentage such as 20, not {value:?}"),
        )
    })?;
    if !(0..=10_000).contains(&parsed) {
        return Err(AccountingError::invalid(
            "invoice",
            field,
            format!("a percentage is between 0 and 100 — not {value}"),
        ));
    }
    Ok(parsed)
}

/// Read a decimal string as an integer count of thousandths, or `None` when it is not a number
/// with at most three decimals.
///
/// The shared core, and deliberately the **only** parser both scales use: a form that learns one
/// scale and not the other is a form that half works.
fn scaled_decimal(raw: &str, decimals: u32) -> Option<i64> {
    if raw.is_empty() {
        return None;
    }
    let (sign, digits) = match raw.as_bytes()[0] {
        b'-' => (-1_i64, &raw[1..]),
        b'+' => (1_i64, &raw[1..]),
        _ => (1_i64, raw),
    };
    if digits.is_empty() {
        return None;
    }
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (digits, ""),
    };
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // More decimals than the column holds is a refusal, not a truncation.
    if fraction.len() > decimals as usize {
        return None;
    }
    let padded = format!("{fraction:0<width$}", width = decimals as usize);
    let value: i64 = format!("{whole}{padded}").parse().ok()?;
    Some(sign * value)
}

/// A percentage back to the text the column stores: hundredths of a percent to `20.00`.
///
/// Two decimals because `numeric(5,2)` is the column, and trailing zeros trimmed because a line
/// that says `tax_percent: 20.00` and a rate that says `20` are the same number and a person
/// reading the grid should not have to wonder whether they are not.
fn percent_text(hundredths: i64) -> String {
    let whole = hundredths / 100;
    let fraction = hundredths % 100;
    if fraction == 0 {
        return whole.to_string();
    }
    let mut text = format!("{whole}.{fraction:02}");
    while text.ends_with('0') {
        text.pop();
    }
    text
}

/// A quantity back to the text the line stores.
fn format_qty(thousandths: i64) -> String {
    // Three decimal places, trailing zeros trimmed, so `1.500` is stored as `1.5` and the PDF
    // does not print a precision the sale did not have.
    let whole = thousandths / 1_000;
    let fraction = thousandths % 1_000;
    if fraction == 0 {
        return whole.to_string();
    }
    let mut text = format!("{whole}.{fraction:03}");
    while text.ends_with('0') {
        text.pop();
    }
    text
}

/// The lines of an invoice, in order.
async fn load_lines(pool: &PgPool, invoice_id: Uuid) -> Result<Vec<InvoiceLineView>> {
    let rows = sqlx::query(
        "select l.id, l.invoice_id, l.position, l.product_id, l.description, \
                l.qty::text as qty, l.unit_price::text as unit_price, \
                l.discount_percent::text as discount_percent, \
                l.tax_percent::text as tax_percent, l.line_total::text as line_total, \
                round(l.line_total - l.line_total / (1 + l.tax_percent / 100), 2)::text \
                    as tax_amount \
         from accounting_invoice_lines l where l.invoice_id = $1 order by l.position",
    )
    .bind(invoice_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| InvoiceLineView {
            id: row.get("id"),
            invoice_id: row.get("invoice_id"),
            position: row.get("position"),
            product_id: row.get("product_id"),
            description: row.get("description"),
            qty: row.get("qty"),
            unit_price: row.get("unit_price"),
            discount_percent: row.get("discount_percent"),
            tax_percent: row.get("tax_percent"),
            line_total: row.get("line_total"),
            tax_amount: row.get("tax_amount"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_round_trips_through_the_wire_and_the_schema() {
        // The CHECK constraint allows exactly these six. A variant the schema refuses is a status
        // that can be typed in Rust, printed in a badge and never stored — the enum is the only
        // place that fact can be checked, so it is checked here.
        for status in InvoiceStatus::all() {
            assert_eq!(InvoiceStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(InvoiceStatus::parse("written_off"), None);
    }

    #[test]
    fn only_a_draft_is_editable_and_only_a_receivable_is_owed() {
        assert!(InvoiceStatus::Draft.is_editable());
        for status in [InvoiceStatus::Sent, InvoiceStatus::Partial, InvoiceStatus::Paid] {
            assert!(!status.is_editable(), "{} must not be editable", status.as_str());
        }
        for status in [InvoiceStatus::Sent, InvoiceStatus::Partial, InvoiceStatus::Overdue] {
            assert!(status.is_receivable(), "{} must count as owed", status.as_str());
        }
        for status in [InvoiceStatus::Draft, InvoiceStatus::Paid, InvoiceStatus::Void] {
            assert!(!status.is_receivable(), "{} must not count as owed", status.as_str());
        }
    }

    #[test]
    fn a_payment_that_clears_the_outstanding_pays_and_one_that_does_not_partially_pays() {
        let hundred = Amount::from_cents(10_000).expect("100.00 is representable");
        let forty = Amount::from_cents(4_000).expect("40.00 is representable");
        let zero = Amount::ZERO;

        assert_eq!(
            InvoiceStatus::after_payment(InvoiceStatus::Sent, forty, sixty()),
            InvoiceStatus::Partial
        );
        assert_eq!(
            InvoiceStatus::after_payment(InvoiceStatus::Overdue, forty, sixty()),
            InvoiceStatus::Partial
        );
        assert_eq!(
            InvoiceStatus::after_payment(InvoiceStatus::Sent, hundred, zero),
            InvoiceStatus::Paid
        );
        // A payment of nothing changes nothing: the invoice is still exactly what it was.
        assert_eq!(
            InvoiceStatus::after_payment(InvoiceStatus::Sent, zero, hundred),
            InvoiceStatus::Sent
        );
    }

    fn sixty() -> Amount {
        Amount::from_cents(6_000).expect("60.00 is representable")
    }

    #[test]
    fn a_due_date_before_the_issue_date_is_refused_with_both_dates_in_the_message() {
        let issue = crate::dates::parse("2026-03-10").expect("a valid date");
        let error = resolve_due_date(Some("2026-03-01"), issue).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("2026-03-01"), "{message}");
        assert!(message.contains("2026-03-10"), "{message}");
    }

    #[test]
    fn a_due_date_the_caller_omits_is_the_issue_date_plus_the_default_terms() {
        let issue = crate::dates::parse("2026-03-10").expect("a valid date");
        let due = resolve_due_date(None, issue).expect("a due date");
        assert_eq!(due, Some(issue + time::Duration::days(DEFAULT_DUE_DAYS)));
    }

    #[test]
    fn a_currency_is_three_letters_and_upper_case() {
        assert_eq!(normalize_currency(None).expect("a default"), "USD");
        assert_eq!(normalize_currency(Some("try")).expect("lowercase"), "TRY");
        for refused in ["US", "USDD", "12", "U$D"] {
            assert!(
                normalize_currency(Some(refused)).is_err(),
                "{refused} must be refused"
            );
        }
    }

    #[test]
    fn the_outstanding_is_the_total_less_what_was_paid() {
        assert_eq!(outstanding_text("120.00", "0.00").expect("readable"), "120.00");
        assert_eq!(outstanding_text("120.00", "48.00").expect("readable"), "72.00");
        assert_eq!(outstanding_text("120.00", "120.00").expect("readable"), "0.00");
        // A stored `paid_total` above the total is refused by the schema; if one ever appears the
        // subtraction says so rather than printing a negative outstanding as a receipt.
        assert!(outstanding_text("not money", "0.00").is_err());
    }

    #[test]
    fn a_quantity_is_printed_without_the_precision_the_sale_did_not_have() {
        assert_eq!(format_qty(1_000), "1");
        assert_eq!(format_qty(1_500), "1.5");
        assert_eq!(format_qty(2_250), "2.25");
        assert_eq!(format_qty(12_000), "12");
    }

    #[test]
    fn a_percentage_is_between_zero_and_one_hundred() {
        assert_eq!(parse_percent("20", "tax_percent").expect("20%"), 2_000);
        assert_eq!(parse_percent("0", "tax_percent").expect("0%"), 0);
        assert_eq!(parse_percent("100", "tax_percent").expect("100%"), 10_000);
        assert!(parse_percent("101", "tax_percent").is_err());
        assert!(parse_percent("-1", "tax_percent").is_err());
        assert!(parse_percent("twenty", "tax_percent").is_err());
    }

    #[test]
    fn a_zero_or_negative_quantity_is_refused_as_a_heading_not_a_line() {
        assert!(parse_qty("0").is_err());
        assert!(parse_qty("-2").is_err());
        assert_eq!(parse_qty("1.5").expect("1.5"), 1_500);
    }

    #[test]
    fn a_line_with_neither_a_product_nor_a_description_is_refused() {
        assert!(normalize_description(None, None).is_err());
        assert!(normalize_description(Some("  "), None).is_err());
        assert_eq!(
            normalize_description(None, Some(Uuid::nil())).expect("a product names itself"),
            ""
        );
    }
}
