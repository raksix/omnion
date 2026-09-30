//! Payments, their allocations and the journal entry each one writes (REQ-054, slice 3).
//!
//! ## The rule this module exists to enforce
//!
//! **An allocation can never exceed what is still owed on its invoice.** Everything else here —
//! the auto-allocation, the partial state, the reversal — is a convenience layered on top of that
//! one sentence, and each of them is a way for the sentence to be broken if it is only checked
//! in the route. So the check is in this module, inside the same transaction that writes the
//! allocation, against a figure read with `for update` on the invoice row: a payment recorded
//! twice from two requests at the same moment both read the same outstanding, and only the lock
//! makes the second one see the first one's write.
//!
//! ## Why the payment is not a column on the invoice
//!
//! Slice 1 made `accounting_payments.invoice_id` not null, which is true of the first payment
//! and false of the rest: a customer who pays three invoices in one transfer made one payment.
//! Splitting it into three loses the fact that the money arrived once, on one date, with one
//! reference — which is exactly the fact an auditor asks about. Migration `0175` makes the
//! column nullable and adds the allocation table; the column survives as a convenience for the
//! single-invoice case so slice 2's rows and its queries keep working.
//!
//! ## The four states a payment can be in
//!
//! `unallocated` (money in, nothing applied), `partially applied`, `fully applied` and
//! `reversed`. The first three are computed from the allocations on every read rather than
//! stored, because a stored flag that disagrees with the allocations is a bug waiting for a
//! report; `reversed` is a column, because it is a fact about the document and not a sum.

// `PgRow` lives under `sqlx::postgres`, not the crate root — `use sqlx::PgRow` does not
// resolve and the error names no module, so it reads like a missing dependency.
use sqlx::postgres::PgRow;
// `QueryBuilder` is referenced by its full path in the list query rather than imported: it is
// used once, and an import for a single use invites a `sqlx::QueryBuilder::new` that loses the
// `<Postgres>` parameter the compiler then cannot infer.
use sqlx::{PgPool, Postgres, Row, Transaction};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AccountingError, Result};
use crate::invoices::InvoiceStatus;
use crate::money::Amount;
use crate::store::Page;

// ---------------------------------------------------------------------------------------------
// The shape
// ---------------------------------------------------------------------------------------------

/// Longest a reference (a bank reference, a cheque number) may be.
pub const MAX_REFERENCE_LENGTH: usize = 120;

/// Longest a free-text note on a payment may be.
pub const MAX_NOTE_LENGTH: usize = 500;

/// Longest a reversal reason may be.
pub const MAX_REASON_LENGTH: usize = 300;

/// The most allocations one payment may carry.
///
/// A payment is a bank statement line, not a batch: a transfer that settles forty invoices is
/// forty transfers that happened to share a morning. The cap exists so the loop that writes
/// them is bounded, the way `MAX_LINES` bounds a journal entry.
pub const MAX_ALLOCATIONS: usize = 100;

/// How a payment reached the business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMethod {
    /// The normal case: money that moved between accounts.
    BankTransfer,
    /// A card payment, settled through an acquirer.
    Card,
    /// Handed over across a desk.
    Cash,
    /// Anything the four above do not describe.
    Other,
}

impl PaymentMethod {
    /// The value stored in `accounting_payments.method`, which the CHECK allows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BankTransfer => "bank_transfer",
            Self::Card => "card",
            Self::Cash => "cash",
            Self::Other => "other",
        }
    }

    /// Read a stored method.
    ///
    /// `None` for an unknown value, so a row this build cannot read is reported rather than
    /// shown as a cash payment.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "bank_transfer" => Some(Self::BankTransfer),
            "card" => Some(Self::Card),
            "cash" => Some(Self::Cash),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    /// Every method, in the order the recorder's picker lists them.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [Self::BankTransfer, Self::Card, Self::Cash, Self::Other]
    }

    /// The name the badge prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BankTransfer => "Bank transfer",
            Self::Card => "Card",
            Self::Cash => "Cash",
            Self::Other => "Other",
        }
    }
}

/// Whether a payment's money is all applied to invoices, some of it, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationState {
    /// Money in, nothing applied — a deposit or an unidentified receipt.
    Unallocated,
    /// Applied to some invoices, with a remainder sitting as customer credit.
    Partial,
    /// Every cent is applied; the invoice that took the last one is closed or moved to partial.
    Applied,
}

impl AllocationState {
    /// The name the list's badge prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unallocated => "Unallocated",
            Self::Partial => "Partially applied",
            Self::Applied => "Applied",
        }
    }

    /// The state a payment is in, given its amount and how much of it is applied.
    ///
    /// A function rather than a match written twice: the list query and the detail read both
    /// need it, and the two disagreeing is how a payment shows "Applied" on one screen while the
    /// receipt says "Partially applied".
    #[must_use]
    pub fn of(amount: &str, allocated: &str) -> Self {
        let remaining = remaining_text(amount, allocated).unwrap_or_else(|_| String::from("0.00"));
        if unallocated_text_is_zero(&remaining) {
            Self::Applied
        } else if allocated_text_is_zero(allocated) {
            Self::Unallocated
        } else {
            Self::Partial
        }
    }
}

/// One allocation: a slice of a payment applied to one invoice.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AllocationView {
    /// The allocation's id.
    pub id: Uuid,
    /// The invoice it was applied to.
    pub invoice_id: Uuid,
    /// That invoice's number, so the screen does not have to resolve it.
    pub invoice_number: String,
    /// The customer on that invoice, for the same reason.
    pub invoice_customer: String,
    /// How much of the payment went here.
    pub amount: String,
    /// The invoice's total.
    pub invoice_total: String,
    /// What the invoice still owed **before** this allocation, which is what makes a row
    /// readable on its own: an operator checks the arithmetic without fetching the invoice.
    pub invoice_outstanding_before: String,
    /// When the row was written.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

/// A payment as the list draws it: no allocations, no journal lines.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaymentSummary {
    /// The row's id.
    pub id: Uuid,
    /// The per-organization number, e.g. `PAY-000007`.
    pub number: String,
    /// The customer the money came from, as the document says it.
    pub customer_name: String,
    /// The day it was received.
    #[serde(with = "crate::dates")]
    pub paid_on: Date,
    /// How it arrived.
    pub method: PaymentMethod,
    /// The amount received.
    pub amount: String,
    /// The currency.
    pub currency: String,
    /// The bank reference or cheque number.
    pub reference: String,
    /// How much of it is applied to invoices.
    pub allocated: String,
    /// `amount - allocated`. Money the customer is owed back until it is applied.
    pub unallocated: String,
    /// Whether it is fully applied, partly, or not at all.
    pub allocation_state: AllocationState,
    /// The journal entry it wrote, when it wrote one.
    pub journal_entry_id: Option<Uuid>,
    /// Whether it has been reversed.
    pub reversed: bool,
    /// Who recorded it.
    pub recorded_by: Option<Uuid>,
    /// When the row was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl PaymentSummary {
    /// Read one row of the list query.
    ///
    /// The alias names here and in [`list_payments`] are the same contract — `row.get` is a
    /// runtime lookup, so a mismatch compiles, passes the unit tests and 500s on the list, which
    /// is the one route whose body nobody has read.
    pub(crate) fn from_row(row: &PgRow) -> Result<Self> {
        let method_text: String = row.get("method");
        let method = PaymentMethod::parse(&method_text).ok_or_else(|| {
            AccountingError::not_allowed(format!(
                "payment {} carries the unknown method {method_text:?}",
                row.get::<Uuid, _>("id")
            ))
        })?;

        let amount = row.get::<String, _>("amount");
        let allocated = row.get::<String, _>("allocated");
        let unallocated = remaining_text(&amount, &allocated)?;
        let state = AllocationState::of(&amount, &allocated);

        Ok(Self {
            id: row.get("id"),
            number: row.get("number"),
            customer_name: row.get("customer_name"),
            paid_on: row.get("paid_on"),
            method,
            amount,
            currency: row.get("currency"),
            reference: row.get("reference"),
            allocated,
            unallocated,
            allocation_state: state,
            journal_entry_id: row.get("journal_entry_id"),
            reversed: row.get::<Option<OffsetDateTime>, _>("reversed_at").is_some(),
            recorded_by: row.get("recorded_by"),
            created_at: row.get("created_at"),
        })
    }
}

/// A payment with its allocations — what the detail screen and the receipt print.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaymentView {
    /// Everything the list carries.
    #[serde(flatten)]
    pub summary: PaymentSummary,
    /// The note the recorder typed.
    pub note: String,
    /// Why it was reversed, when it has been.
    pub reversal_reason: String,
    /// When it was reversed.
    #[serde(with = "crate::dates::instant::option")]
    pub reversed_at: Option<OffsetDateTime>,
    /// The entry the reversal wrote.
    pub reversal_entry_id: Option<Uuid>,
    /// The allocations, in the order they were written.
    pub allocations: Vec<AllocationView>,
    /// The invoices whose status this payment changed — what the detail screen's "what this
    /// changed" strip reads, so a person does not have to open each invoice to learn that one
    /// of them is now closed.
    pub settled_invoices: Vec<SettledInvoice>,
}

/// An invoice a payment moved, and where it left it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SettledInvoice {
    /// The invoice's id.
    pub invoice_id: Uuid,
    /// Its number.
    pub invoice_number: String,
    /// Its status **before** this payment.
    pub status_before: InvoiceStatus,
    /// Its status after.
    pub status_after: InvoiceStatus,
    /// What it still owes, which is `0.00` when the payment closed it.
    pub outstanding: String,
}

/// One line of a request to record a payment.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AllocationInput {
    /// The invoice the money is applied to.
    pub invoice_id: Uuid,
    /// How much of the payment goes here. Must be positive; a zero row is dropped rather than
    /// refused, because the recorder's grid leaves an empty row behind when somebody clears a
    /// picker and the amount never changes.
    pub amount: String,
}

/// The body of `POST /accounting/payments`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct NewPayment {
    /// The customer the money came from. Optional: an unidentified receipt is a real row, and
    /// refusing to record it is how a business loses the note that the money arrived.
    #[serde(default)]
    pub customer_id: Option<Uuid>,
    /// The name to show for the customer, when there is no CRM record behind it.
    #[serde(default)]
    pub customer_name: Option<String>,
    /// The company the money belongs to, when the caller says.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// `YYYY-MM-DD`. Defaults to today.
    #[serde(default)]
    pub paid_on: Option<String>,
    /// How it arrived.
    #[serde(default)]
    pub method: Option<String>,
    /// The amount received. Required and strictly positive.
    #[serde(default)]
    pub amount: Option<String>,
    /// The currency. Defaults to the organization's invoice currency, then `USD`.
    #[serde(default)]
    pub currency: Option<String>,
    /// The bank reference or cheque number.
    #[serde(default)]
    pub reference: Option<String>,
    /// A note.
    #[serde(default)]
    pub note: Option<String>,
    /// What the money is applied to. Empty means "record it, apply it later".
    #[serde(default)]
    pub allocations: Vec<AllocationInput>,
    /// Apply the money to the organization's open invoices, oldest first, without naming them.
    ///
    /// A boolean rather than a mode string, because the two ways of getting here are mutually
    /// exclusive by intent: a caller who named allocations has already said what the money is
    /// for, and silently overriding it with a sweep would be the worst possible answer.
    #[serde(default)]
    pub auto_allocate: Option<bool>,
    /// Allow an allocation to exceed an invoice's outstanding.
    ///
    /// Off by default. The route maps it to a permission; the module still refuses an
    /// over-allocation that would exceed the **payment's own amount**, because that is not a
    /// business decision but an arithmetic one.
    #[serde(default)]
    pub allow_overpayment: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------------------------

/// Record a payment and its allocations, and write the journal entry.
///
/// The order, and each step's reason:
///
/// 1. **parse and validate in Rust**, so the refusal names a field and a number rather than a
///    constraint;
/// 2. **auto-allocate if asked** — against the same read that the manual path uses, so the two
///    cannot disagree about what is owed;
/// 3. begin, and **lock every invoice the payment touches** with `for update`, in a stable order
///    (sorted by id) so two payments touching the same two invoices cannot deadlock each other;
/// 4. re-read each outstanding **under the lock** and refuse any allocation above it;
/// 5. write the payment, the allocations, the invoice updates and the journal entry;
/// 6. commit, then read the record back.
///
/// Step 3 is the one that makes the headline acceptance criterion true rather than merely
/// intended. Two requests recording payments against the same invoice at the same moment both
/// read the same outstanding without it, and the `check (paid_total <= grand_total)` on the
/// invoice turns the loser into a constraint error whose message names nothing. With the lock the
/// second one reads the first one's write and is refused with a message naming the invoice and
/// the two amounts.
pub async fn record_payment(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewPayment,
    recorded_by: Option<Uuid>,
) -> Result<PaymentView> {
    // -- 1. the fields, before any connection is taken --------------------------------------
    let method = match new.method.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        None => PaymentMethod::BankTransfer,
        Some(raw) => PaymentMethod::parse(raw).ok_or_else(|| {
            // The four names are in the message: a misspelling that returned an empty list would
            // read like "no payments like that exist".
            AccountingError::invalid(
                "payment",
                "method",
                format!(
                    "{raw:?} is not a payment method — use bank_transfer, card, cash or other"
                ),
            )
        })?,
    };

    let amount = parse_required_amount(new.amount.as_deref(), "payment", "amount")?;
    if amount.is_zero() {
        return Err(AccountingError::invalid(
            "payment",
            "amount",
            "a payment of zero is not a payment — record nothing, or record the amount received",
        ));
    }

    let paid_on = resolve_paid_on(new.paid_on.as_deref())?;
    let reference = normalize_text(new.reference.as_deref(), MAX_REFERENCE_LENGTH, "reference")?;
    let note = normalize_text(new.note.as_deref(), MAX_NOTE_LENGTH, "note")?;
    let currency = normalize_currency(new.currency.as_deref())?;

    if new.allocations.len() > MAX_ALLOCATIONS {
        return Err(AccountingError::invalid(
            "payment",
            "allocations",
            format!("a payment carries at most {MAX_ALLOCATIONS} allocations"),
        ));
    }

    // Rows that name no invoice or no amount are dropped rather than refused: the recorder's
    // grid leaves one behind every time a picker is cleared, and a person should not have to
    // delete an empty row to save a real one.
    let mut requested: Vec<(Uuid, Amount)> = Vec::new();
    for line in &new.allocations {
        // `amount` is a `String`, not an `Option<String>`: serde already let a missing field go as
        // the empty string, so there is nothing to `as_deref()` here. A row whose amount is blank
        // is **dropped**, not refused — the recorder's grid leaves one behind every time a picker
        // is cleared, and an empty row should not block the real one next to it.
        //
        // Written with an `if`, not `bool::then_some`: the question "is this row blank?" and the
        // `continue` that follows it are two separate statements, and a one-liner combining them
        // inverts silently — which is exactly what the first run of this walk caught, as 12
        // failures all carrying the same "no allocations" message.
        let raw = line.amount.trim();
        if raw.is_empty() {
            continue;
        }
        let value = parse_required_amount(Some(raw), "payment_allocation", "amount")?;
        if value.is_zero() {
            continue;
        }
        if requested.iter().any(|(invoice, _)| *invoice == line.invoice_id) {
            // The database would refuse this too, but as a unique violation on a table the
            // operator cannot see. Naming it here is the difference between a form error and a
            // 500.
            return Err(AccountingError::invalid(
                "payment_allocation",
                "invoice_id",
                "the same invoice is named twice — combine the two amounts into one row",
            ));
        }
        requested.push((line.invoice_id, value));
    }

    let auto_allocate = new.auto_allocate.unwrap_or(false);
    if auto_allocate && !requested.is_empty() {
        return Err(AccountingError::invalid(
            "payment",
            "auto_allocate",
            "the payment names the invoices to apply to, or asks for the oldest-first sweep — \
             not both",
        ));
    }
    if requested.is_empty() && !auto_allocate {
        return Err(AccountingError::invalid(
            "payment",
            "allocations",
            "name the invoices this payment settles, or ask for auto-allocate; recording money \
             with nothing to apply it to is an unidentified receipt",
        ));
    }

    // The allocations can never exceed what was received. This one is arithmetic, not business,
    // so the override flag does not reach it.
    let mut allocated_total = Amount::ZERO;
    for (_, value) in &requested {
        allocated_total = allocated_total.plus(*value);
    }
    if allocated_total.cents() > amount.cents() {
        return Err(AccountingError::invalid(
            "payment",
            "allocations",
            format!(
                "the allocations add up to {} but the payment is for {} — the difference would \
                 be money nobody received",
                allocated_total.to_text(),
                amount.to_text()
            ),
        ));
    }

    // -- 2. the sweep, before the transaction, against the same read the manual path uses ----
    if auto_allocate {
        let open = open_invoices_for_allocation(pool, organization_id).await?;
        let mut left = amount;
        for candidate in open {
            if left.is_zero() {
                break;
            }
            let take = if candidate.outstanding.cents() <= left.cents() {
                candidate.outstanding
            } else {
                left
            };
            requested.push((candidate.invoice_id, take));
            allocated_total = allocated_total.plus(take);
            left = left.minus(take);
        }
    }

    let currency = resolve_currency(pool, organization_id, currency).await?;
    let customer_name = resolve_customer_name(pool, organization_id, new).await?;

    // -- 3..5 the write, in one transaction ---------------------------------------------------
    let mut tx = pool.begin().await?;

    let mut locked: Vec<LockedInvoice> = Vec::with_capacity(requested.len());
    // Sorted, so the lock order is the same for every payment that touches the same invoices.
    let mut ordered = requested.clone();
    ordered.sort_by_key(|(invoice, _)| *invoice);
    for (invoice_id, _) in &ordered {
        let row = sqlx::query(
            "select i.id, i.number, i.invoice_status, i.grand_total::text as grand_total, \
                    i.paid_total::text as paid_total, i.customer_name, i.due_date \
             from accounting_invoices i \
             where i.id = $1 and i.organization_id = $2 \
             for update",
        )
        .bind(invoice_id)
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(AccountingError::NotFound("invoice"));
        };
        let status_text: String = row.get("invoice_status");
        let status = InvoiceStatus::parse(&status_text).ok_or_else(|| {
            AccountingError::not_allowed(format!(
                "invoice {} carries the unknown status {status_text:?}",
                row.get::<Uuid, _>("id")
            ))
        })?;
        // A draft has never been given to anybody, so there is nothing to collect against. A
        // void one has been withdrawn. Both are refusals rather than silent skips, because a
        // recorder who aimed at them wants to know the row will not move.
        match status {
            InvoiceStatus::Draft => {
                return Err(AccountingError::not_allowed(format!(
                    "invoice {} is a draft — send it before recording a payment against it",
                    row.get::<String, _>("number")
                )));
            }
            InvoiceStatus::Void => {
                return Err(AccountingError::not_allowed(format!(
                    "invoice {} was voided — record a new invoice instead",
                    row.get::<String, _>("number")
                )));
            }
            InvoiceStatus::Paid => {
                return Err(AccountingError::not_allowed(format!(
                    "invoice {} is already paid in full",
                    row.get::<String, _>("number")
                )));
            }
            InvoiceStatus::Sent | InvoiceStatus::Partial | InvoiceStatus::Overdue => {}
        }

        let grand_total = Amount::parse(&row.get::<String, _>("grand_total"))
            .map_err(|error| AccountingError::InvalidAmount {
                message: format!("invoice {} has an unreadable total: {error}", row.get::<String, _>("number")),
            })?;
        let paid_total = Amount::parse(&row.get::<String, _>("paid_total"))
            .map_err(|error| AccountingError::InvalidAmount {
                message: format!("invoice {} has an unreadable paid total: {error}", row.get::<String, _>("number")),
            })?;
        let outstanding = grand_total.minus(paid_total);

        locked.push(LockedInvoice {
            id: row.get("id"),
            number: row.get("number"),
            customer_name: row.get("customer_name"),
            status,
            grand_total,
            paid_total,
            outstanding,
            due_date: row.get("due_date"),
        });
    }

    // -- 4. the rule: no allocation above what is owed ---------------------------------------
    // A payment that names no customer takes it from the single invoice it settles, resolved
    // under the lock below — which is why this happens after the lock and not before it. The
    // "Unidentified" default survives only for a payment applied to several invoices whose
    // customers disagree, where naming one of them would be a guess.
    let mut customer_name = customer_name;
    if customer_name == "Unidentified" && locked.len() == 1 {
        let only = locked[0].customer_name.trim();
        if !only.is_empty() {
            customer_name = only.to_string();
        }
    }

    let allow_overpayment = new.allow_overpayment.unwrap_or(false);
    for (invoice_id, value) in &requested {
        let Some(invoice) = locked.iter().find(|entry| entry.id == *invoice_id) else {
            return Err(AccountingError::NotFound("invoice"));
        };
        if value.cents() <= invoice.outstanding.cents() {
            continue;
        }
        if allow_overpayment {
            continue;
        }
        return Err(AccountingError::OverAllocation {
            invoice_number: invoice.number.clone(),
            attempted: value.to_text(),
            outstanding: invoice.outstanding.to_text(),
        });
    }

    let payment_number: i64 =
        sqlx::query_scalar(
            "select coalesce(max(payment_number), 0) + 1 from accounting_payments \
             where organization_id = $1",
        )
        .bind(organization_id)
        .fetch_one(&mut *tx)
        .await?;
    let number = format!("PAY-{payment_number:06}");

    let payment_id: Uuid = sqlx::query_scalar(
        "insert into accounting_payments \
             (organization_id, invoice_id, company_id, customer_id, customer_name, payment_number, \
              number, paid_on, method, amount, currency, reference, note, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::numeric, $11, $12, $13, $14) \
         returning id",
    )
    .bind(organization_id)
    // The convenience column carries the first allocation's invoice, when there is exactly one.
    // A payment split across three invoices leaves it null, because any of the three would be a
    // lie about the other two.
    .bind(requested.first().map(|(invoice, _)| *invoice))
    .bind(new.company_id)
    .bind(new.customer_id)
    .bind(&customer_name)
    .bind(payment_number)
    .bind(&number)
    .bind(paid_on)
    .bind(method.as_str())
    .bind(amount.to_text())
    .bind(&currency)
    .bind(&reference)
    .bind(&note)
    .bind(recorded_by)
    .fetch_one(&mut *tx)
    .await?;

    let mut settled: Vec<SettledInvoice> = Vec::with_capacity(requested.len());
    for (invoice_id, value) in &requested {
        let Some(invoice) = locked.iter().find(|entry| entry.id == *invoice_id) else {
            return Err(AccountingError::NotFound("invoice"));
        };
        let status_before = invoice.status;

        // `position` is the loop's own index, so the read-back can restore the order the sweep
        // walked the invoices in. Sorting by `created_at` cannot: `now()` is transaction-stable,
        // so every row this payment writes carries the identical timestamp and the tiebreak falls
        // to a random `id`.
        sqlx::query(
            "insert into accounting_payment_allocations \
                 (payment_id, organization_id, invoice_id, amount, position) \
             values ($1, $2, $3, $4::numeric, $5)",
        )
        .bind(payment_id)
        .bind(organization_id)
        .bind(invoice_id)
        .bind(value.to_text())
        .bind(i16::try_from(settled.len()).unwrap_or(i16::MAX))
        .execute(&mut *tx)
        .await?;

        let new_paid = invoice.paid_total.plus(*value);
        let outstanding_after = invoice.grand_total.minus(new_paid);
        let status_after = if outstanding_after.is_zero() {
            InvoiceStatus::Paid
        } else {
            InvoiceStatus::Partial
        };

        // One statement, so a status and a paid total can never disagree. `paid_total` is
        // recomputed from the column rather than incremented, which makes the write idempotent
        // against a replay: the value does not depend on what the row said before.
        sqlx::query(
            "update accounting_invoices set \
                 paid_total = $3::numeric, \
                 invoice_status = $4, \
                 last_payment_at = now(), \
                 paid_at = case when $4 = 'paid' then now() else paid_at end, \
                 overdue_at = case when $4 <> 'overdue' then null else overdue_at end, \
                 updated_at = now() \
             where id = $1 and organization_id = $2",
        )
        .bind(invoice_id)
        .bind(organization_id)
        .bind(new_paid.to_text())
        .bind(status_after.as_str())
        .execute(&mut *tx)
        .await?;

        settled.push(SettledInvoice {
            invoice_id: invoice.id,
            invoice_number: invoice.number.clone(),
            status_before,
            status_after,
            outstanding: outstanding_after.to_text(),
        });
    }

    // -- the journal entry -------------------------------------------------------------------
    // Debit cash for the whole amount (that is what arrived), credit the receivables for what
    // was applied, and credit customer advances for the surplus. The entry is written whether or
    // not it was applied to anything, because the money arriving IS the event — a receipt that
    // only ever exists in the payments table is invisible to every report and to the trial
    // balance.
    let (cash_id, receivable_id, advances_id) = accounts_for_payment(&mut tx, organization_id).await?;
    let advance_amount = amount.minus(allocated_total);
    let entry_id = write_payment_entry(
        &mut tx,
        organization_id,
        payment_id,
        &number,
        paid_on,
        method,
        amount,
        allocated_total,
        advance_amount,
        cash_id,
        receivable_id,
        advances_id,
        recorded_by,
    )
    .await?;

    sqlx::query("update accounting_payments set journal_entry_id = $2 where id = $1")
        .bind(payment_id)
        .bind(entry_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    // -- 6. read it back ---------------------------------------------------------------------
    let mut view = get_payment(pool, organization_id, payment_id).await?;
    view.settled_invoices = settled;
    Ok(view)
}

// ---------------------------------------------------------------------------------------------
// Reversal
// ---------------------------------------------------------------------------------------------

/// Reverse a payment: the row stays, the allocations are released and a counter entry is written.
///
/// Three things happen, in this order, and each one is a separate fact:
///
/// 1. the payment is stamped `reversed_at` — the `where reversed_at is null` is the whole
///    idempotence guard, the same shape the overdue sweep uses, so two concurrent reversals
///    cannot both win;
/// 2. the allocations go, and the invoices it had moved are recomputed **from what is still
///    allocated** rather than by subtracting the payment again. Subtracting is the bug: a
///    reversal after a later payment would take the invoice below zero, and an invoice cannot be
///    less than paid.
/// 3. a counter journal entry is written — the same lines, sides swapped — so the ledger still
///    balances and the original entry is never edited. Posted is immutable; that is the whole
///    reason this is a new document and not an update.
pub async fn reverse_payment(
    pool: &PgPool,
    organization_id: Uuid,
    payment_id: Uuid,
    reason: &str,
    reversed_by: Option<Uuid>,
) -> Result<PaymentView> {
    let reason = normalize_text(Some(reason), MAX_REASON_LENGTH, "reason")?;
    if reason.is_empty() {
        return Err(AccountingError::invalid(
            "payment",
            "reason",
            "say why the payment is being reversed — a reversal without a reason is an edit with \
             extra steps",
        ));
    }

    let mut tx = pool.begin().await?;

    // The row, locked, and the reversal claimed in the same statement: `reversing an update
    // returning zero rows` is the answer to "this one is already reversed", and it cannot be a
    // stale read because the lock is held to the end of the transaction.
    let row = sqlx::query(
        "select p.id, p.number, p.paid_on, p.method, p.amount::text as amount, \
                p.reference, p.journal_entry_id, p.reversed_at, p.currency, p.created_by \
         from accounting_payments p \
         where p.id = $1 and p.organization_id = $2 \
         for update",
    )
    .bind(payment_id)
    .bind(organization_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Err(AccountingError::NotFound("payment"));
    };
    if row.get::<Option<OffsetDateTime>, _>("reversed_at").is_some() {
        return Err(AccountingError::not_allowed(format!(
            "payment {} was already reversed on {}",
            row.get::<String, _>("number"),
            crate::dates::instant_to_wire(
                &row.get::<Option<OffsetDateTime>, _>("reversed_at").unwrap_or(OffsetDateTime::now_utc())
            )
        )));
    }

    let method_text: String = row.get("method");
    let method = PaymentMethod::parse(&method_text).ok_or_else(|| {
        AccountingError::not_allowed(format!("payment carries the unknown method {method_text:?}"))
    })?;
    let amount =
        Amount::parse(&row.get::<String, _>("amount")).map_err(|error| AccountingError::InvalidAmount {
            message: format!("payment {} has an unreadable amount: {error}", row.get::<String, _>("number")),
        })?;
    let number: String = row.get("number");
    // The original's date, which is the date its counter entry carries: a reversing entry
    // corrects the period the mistake was made in. `reversed_at` is when the undo happened,
    // so "when was this undone" and "which period does it correct" have two answers, and
    // conflating them moves the correction into the wrong month's numbers.
    let original_paid_on: Date = row.get("paid_on");

    // The invoices this payment touched, and what it put on each — read before the allocations go,
    // so the entries can be written per invoice.
    let allocations = sqlx::query(
        "select a.invoice_id, a.amount::text as amount, i.number as invoice_number \
         from accounting_payment_allocations a \
         join accounting_invoices i on i.id = a.invoice_id \
         where a.payment_id = $1 order by a.invoice_id",
    )
    .bind(payment_id)
    .fetch_all(&mut *tx)
    .await?;

    // The reversal is claimed here, guarded by the column the getter above checked. If two
    // requests race, the second one's UPDATE matches nothing and it is refused.
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "update accounting_payments set reversed_at = now(), reversed_by = $3, \
             reversal_reason = $4 where id = $1 and organization_id = $2 and reversed_at is null \
         returning id",
    )
    .bind(payment_id)
    .bind(organization_id)
    .bind(reversed_by)
    .bind(&reason)
    .fetch_optional(&mut *tx)
    .await?;
    if claimed.is_none() {
        return Err(AccountingError::not_allowed(format!(
            "payment {number} was reversed by another request while this one was reading it"
        )));
    }

    // Recompute each touched invoice from what is still allocated. Locking them in id order is
    // the same order `record_payment` takes, so the two cannot deadlock.
    let mut touched: Vec<Uuid> = allocations
        .iter()
        .map(|row| row.get::<Uuid, _>("invoice_id"))
        .collect();
    touched.sort_unstable();
    for invoice_id in &touched {
        sqlx::query("select id from accounting_invoices where id = $1 for update")
            .bind(invoice_id)
            .fetch_optional(&mut *tx)
            .await?;
    }

    // Release the allocations last, after the recompute below reads them, so the sum is of what
    // remains. Order matters: if the rows went first, the recompute would find nothing and every
    // invoice would be rewritten as unpaid.
    let mut applied = Amount::ZERO;
    for row in &allocations {
        let value = Amount::parse(&row.get::<String, _>("amount"))
            .map_err(|error| AccountingError::InvalidAmount {
                message: format!("payment {number} has an unreadable allocation: {error}"),
            })?;
        applied = applied.plus(value);
    }

    for invoice_id in &touched {
        let remaining: String = sqlx::query_scalar(
            "select coalesce(sum(amount), 0)::text from accounting_payment_allocations \
             where invoice_id = $1 and payment_id <> $2",
        )
        .bind(invoice_id)
        .bind(payment_id)
        .fetch_one(&mut *tx)
        .await?;
        let remaining = Amount::parse(&remaining).map_err(|error| AccountingError::InvalidAmount {
            message: format!("invoice {} has an unreadable allocation sum: {error}", invoice_id),
        })?;

        let totals = sqlx::query(
            "select grand_total::text as grand_total, invoice_status from accounting_invoices \
             where id = $1 and organization_id = $2",
        )
        .bind(invoice_id)
        .bind(organization_id)
        .fetch_one(&mut *tx)
        .await?;
        let grand_total = Amount::parse(&totals.get::<String, _>("grand_total"))
            .map_err(|error| AccountingError::InvalidAmount {
                message: format!("invoice {invoice_id} has an unreadable total: {error}"),
            })?;
        let status_text: String = totals.get("invoice_status");
        let current = InvoiceStatus::parse(&status_text).ok_or_else(|| {
            AccountingError::not_allowed(format!("invoice {invoice_id} carries the unknown status {status_text:?}"))
        })?;

        // Never more than the total, even if a row drifted: the column's own CHECK enforces the
        // ceiling and this keeps the recompute from ever walking into it.
        let settled = if remaining.cents() > grand_total.cents() {
            grand_total
        } else {
            remaining
        };
        let outstanding_after = grand_total.minus(settled);
        // A void invoice keeps its status whatever the recompute says, and a draft was never
        // collectable so the sweep is not what put it there.
        let status_after = if current == InvoiceStatus::Void || current == InvoiceStatus::Draft {
            current
        } else if outstanding_after.is_zero() {
            InvoiceStatus::Paid
        } else if settled.is_zero() {
            InvoiceStatus::Sent
        } else {
            InvoiceStatus::Partial
        };

        sqlx::query(
            "update accounting_invoices set \
                 paid_total = $3::numeric, \
                 invoice_status = $4, \
                 paid_at = case when $4 = 'paid' then coalesce(paid_at, now()) else null end, \
                 last_payment_at = case when $3::numeric = 0 then null else last_payment_at end, \
                 updated_at = now() \
             where id = $1 and organization_id = $2",
        )
        .bind(invoice_id)
        .bind(organization_id)
        .bind(settled.to_text())
        .bind(status_after.as_str())
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query("delete from accounting_payment_allocations where payment_id = $1")
        .bind(payment_id)
        .execute(&mut *tx)
        .await?;

    // The counter entry: the original's lines, sides swapped. Debit what was credited, credit
    // what was debited. Cash comes back down, the receivables (or the advances) go back up.
    let (cash_id, receivable_id, advances_id) = accounts_for_payment(&mut tx, organization_id).await?;
    let entry_id = write_reversal_entry(
        &mut tx,
        organization_id,
        payment_id,
        &number,
        method,
        original_paid_on,
        amount,
        applied,
        cash_id,
        receivable_id,
        advances_id,
        &reason,
        reversed_by,
    )
    .await?;

    sqlx::query("update accounting_payments set reversal_entry_id = $2 where id = $1")
        .bind(payment_id)
        .bind(entry_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    get_payment(pool, organization_id, payment_id).await
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// One payment with its allocations, or `404` — which is also the answer for another
/// organization's payment.
pub async fn get_payment(
    pool: &PgPool,
    organization_id: Uuid,
    payment_id: Uuid,
) -> Result<PaymentView> {
    let row = sqlx::query(
        "select p.id, p.number, p.customer_name, p.paid_on, p.method, p.amount::text as amount, \
                p.currency, p.reference, p.journal_entry_id, p.reversed_at, p.created_by, \
                p.created_by, p.created_at, \
                coalesce((select sum(a.amount)::text from accounting_payment_allocations a \
                          where a.payment_id = p.id), '0') as allocated, \
                p.note, p.reversal_reason, p.reversal_entry_id \
         from accounting_payments p \
         where p.id = $1 and p.organization_id = $2",
    )
    .bind(payment_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Err(AccountingError::NotFound("payment"));
    };

    // The customer name lives on the payment because the CRM owns the *current* name and a
    // receipt is a document issued on a day, like an invoice. Read from the payment, falling
    // back to the invoice it was recorded against for the rows slice 1 wrote before the column
    // existed.
    let customer_name: String = row
        .get::<Option<String>, _>("customer_name")
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| String::from("Unidentified"));

    // The state is derived here rather than filled in below: the struct is built before the
    // amounts are known, and a placeholder `Unallocated` on a fully applied payment is the kind
    // of field that ships because nothing reads it in the detail view.
    let mut summary = PaymentSummary {
        id: row.get("id"),
        number: row.get("number"),
        customer_name,
        paid_on: row.get("paid_on"),
        method: PaymentMethod::parse(&row.get::<String, _>("method")).unwrap_or(PaymentMethod::Other),
        amount: row.get("amount"),
        currency: row.get("currency"),
        reference: row.get("reference"),
        allocated: row.get("allocated"),
        unallocated: String::new(),
        allocation_state: AllocationState::Unallocated,
        journal_entry_id: row.get("journal_entry_id"),
        reversed: row.get::<Option<OffsetDateTime>, _>("reversed_at").is_some(),
        // Only `created_by` exists on this table: the REQ's data model names the field
        // `recorded_by` and migration `0167` wrote `created_by`, and I had hedged by reading both
        // "so a row written by either version names somebody". **A hedge over a column that was
        // never two columns is a panic, not a compatibility layer** — `row.get` on an alias the
        // SELECT does not list is `ColumnNotFound` at runtime, not `None`, and it took the whole
        // suite down with one line. The type annotation is here because the plain `row.get` has
        // nothing to infer from.
        recorded_by: row.get::<Option<Uuid>, _>("created_by"),
        created_at: row.get("created_at"),
    };
    summary.unallocated = remaining_text(&summary.amount, &summary.allocated)?;
    summary.allocation_state = AllocationState::of(&summary.amount, &summary.allocated);

    let allocations = load_allocations(pool, payment_id).await?;
    Ok(PaymentView {
        summary,
        note: row.get::<Option<String>, _>("note").unwrap_or_default(),
        reversal_reason: row.get::<Option<String>, _>("reversal_reason").unwrap_or_default(),
        reversed_at: row.get("reversed_at"),
        reversal_entry_id: row.get("reversal_entry_id"),
        allocations,
        settled_invoices: Vec::new(),
    })
}

/// The allocations of one payment, in the order they were written.
pub async fn load_allocations(pool: &PgPool, payment_id: Uuid) -> Result<Vec<AllocationView>> {
    let rows = sqlx::query(
        "select a.id, a.invoice_id, a.amount::text as amount, a.created_at, \
                i.number as invoice_number, i.customer_name, i.grand_total::text as grand_total, \
                i.paid_total::text as paid_total \
         from accounting_payment_allocations a \
         join accounting_invoices i on i.id = a.invoice_id \
         where a.payment_id = $1 order by a.position nulls last, a.id",
    )
    .bind(payment_id)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let grand_total = Amount::parse(&row.get::<String, _>("grand_total"))
            .map_err(|error| AccountingError::InvalidAmount {
                message: format!("invoice has an unreadable total: {error}"),
            })?;
        let paid_total = Amount::parse(&row.get::<String, _>("paid_total"))
            .map_err(|error| AccountingError::InvalidAmount {
                message: format!("invoice has an unreadable paid total: {error}"),
            })?;
        let amount = Amount::parse(&row.get::<String, _>("amount"))
            .map_err(|error| AccountingError::InvalidAmount {
                message: format!("allocation has an unreadable amount: {error}"),
            })?;
        out.push(AllocationView {
            id: row.get("id"),
            invoice_id: row.get("invoice_id"),
            invoice_number: row.get("invoice_number"),
            invoice_customer: row.get("customer_name"),
            amount: amount.to_text(),
            invoice_total: grand_total.to_text(),
            invoice_outstanding_before: grand_total.minus(paid_total).to_text(),
            created_at: row.get("created_at"),
        });
    }
    Ok(out)
}

/// The payments list.
#[allow(clippy::too_many_arguments)]
pub async fn list_payments(
    pool: &PgPool,
    organization_id: Uuid,
    method: Option<PaymentMethod>,
    from: Option<Date>,
    to: Option<Date>,
    search: Option<&str>,
    unreversed_only: bool,
    limit: i64,
) -> Result<Page<PaymentSummary>> {
    // QueryBuilder rather than a string with optional fragments: the predicates bind themselves
    // and are numbered by their own position, so a filter added here cannot shift a `$n` that is
    // already written further down.
    let mut query = sqlx::QueryBuilder::<Postgres>::new(
        // `p.created_by as recorded_by` — the alias is load-bearing. `from_row` reads
        // `recorded_by`, `row.get` is a *runtime* lookup, and the column is `created_by` because
        // migration 0167 wrote it that way. Without the alias the list 500s with
        // `ColumnNotFound("recorded_by")` on every call, which is why it is spelled here rather
        // than read under two names: reading both is a panic, not a compatibility layer.
        "select p.id, p.number, p.customer_name, p.paid_on, p.method, p.amount::text as amount, \
                p.currency, p.reference, p.journal_entry_id, p.reversed_at, \
                p.created_by as recorded_by, p.created_at, \
                coalesce((select sum(a.amount)::text from accounting_payment_allocations a \
                          where a.payment_id = p.id), '0') as allocated \
         from accounting_payments p where p.organization_id = ",
    );
    query.push_bind(organization_id);

    if let Some(method) = method {
        query.push(" and p.method = ").push_bind(method.as_str());
    }
    if let Some(from) = from {
        query.push(" and p.paid_on >= ").push_bind(from);
    }
    if let Some(to) = to {
        query.push(" and p.paid_on <= ").push_bind(to);
    }
    if unreversed_only {
        query.push(" and p.reversed_at is null");
    }
    if let Some(term) = search.map(str::trim).filter(|value| !value.is_empty()) {
        if term.chars().count() > crate::store::MAX_SEARCH_LENGTH {
            return Err(AccountingError::InvalidQuery(format!(
                "a search term is at most {} characters",
                crate::store::MAX_SEARCH_LENGTH
            )));
        }
        // LIKE on a substring of the two things a person knows: the number they were given and
        // who paid. Parameters on both sides, so a term containing `%` is a literal rather than
        // a wildcard that matches the whole table.
        let pattern = format!("%{}%", escape_like(term));
        query
            .push(" and (p.number ilike ")
            .push_bind(pattern.clone())
            .push(" or coalesce(p.customer_name, '') ilike ")
            .push_bind(pattern)
            .push(")");
    }

    query.push(" order by p.paid_on desc, p.id desc limit ").push_bind(limit.saturating_add(1));
    let rows = query.build().fetch_all(pool).await?;

    let truncated = rows.len() > limit as usize;
    let items: Vec<PaymentSummary> = rows
        .into_iter()
        .take(limit.max(0) as usize)
        .map(|row| PaymentSummary::from_row(&row))
        .collect::<Result<Vec<_>>>()?;
    // A page is "there may be more", not "here is the total": counting a payments table on
    // every keystroke of the search box is a query the list did not need.
    let next_cursor = if truncated {
        items.last().map(|item| item.id.to_string())
    } else {
        None
    };
    Ok(Page::new(items, next_cursor, i64::from(truncated)))
}

// ---------------------------------------------------------------------------------------------
// The sweep and its helpers
// ---------------------------------------------------------------------------------------------

/// One invoice the auto-allocation may apply to.
#[derive(Debug, Clone)]
struct Candidate {
    /// The invoice's id.
    invoice_id: Uuid,
    /// What is still owed on it.
    outstanding: Amount,
}

/// The organization's open invoices, oldest due first.
///
/// "Oldest first" is the rule the REQ names and it is not arbitrary: a customer who pays
/// without saying what for means the earliest thing they owe, and applying the sweep the other
/// way round would leave the oldest invoice overdue while closing a newer one. Ties break on the
/// issue date and then the id, so the sweep is deterministic — a client that calls it twice gets
/// the same answer, which is what makes the split testable.
async fn open_invoices_for_allocation(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<Candidate>> {
    let rows = sqlx::query(
        "select id, (grand_total - paid_total)::text as outstanding from accounting_invoices \
         where organization_id = $1 and invoice_status in ('sent', 'partial', 'overdue') \
         order by due_date nulls last, issue_date, id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let text: String = row.get("outstanding");
        // A zero or negative outstanding is skipped rather than clamped: a row that is already
        // settled but still says `sent` is a defect worth leaving visible to the sweep's caller
        // rather than papering over with a 0.00 allocation that the CHECK would refuse anyway.
        let outstanding = match Amount::parse(&text) {
            Ok(value) if !value.is_zero() => value,
            _ => continue,
        };
        out.push(Candidate { invoice_id: row.get("id"), outstanding });
    }
    Ok(out)
}

/// An invoice read under a row lock, with the arithmetic the rule needs.
struct LockedInvoice {
    /// The invoice's id.
    id: Uuid,
    /// Its number.
    number: String,
    /// The customer on the invoice.
    ///
    /// Read for more than the struct's own sake: when the caller names no customer, a payment
    /// that settles a single invoice takes that invoice's name. A receipt for a payment that
    /// says "Unidentified" while the invoice it settles names the customer is a receipt nobody
    /// can match to the row that created it.
    customer_name: String,
    /// Its status before this payment.
    status: InvoiceStatus,
    /// Its total.
    grand_total: Amount,
    /// What had been paid before this payment.
    paid_total: Amount,
    /// `grand_total - paid_total` at the moment of the lock.
    outstanding: Amount,
    /// When it is due — carried for the sort, not printed.
    #[allow(dead_code)]
    due_date: Option<Date>,
}

/// The three accounts a payment entry needs, by their seeded codes.
async fn accounts_for_payment(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
) -> Result<(Uuid, Uuid, Uuid)> {
    let codes = ["1100", "1200", "2300"];
    let mut found: Vec<Option<Uuid>> = Vec::with_capacity(codes.len());
    for code in codes {
        let id: Option<Uuid> = sqlx::query_scalar(
            "select id from accounting_accounts where organization_id = $1 and code = $2",
        )
        .bind(organization_id)
        .bind(code)
        .fetch_optional(&mut **tx)
        .await?;
        found.push(id);
    }

    let describe = |index: usize| -> String {
        match found.get(index).copied().flatten() {
            Some(id) => id.to_string(),
            None => format!("account {} is missing from this organization's chart", codes[index]),
        }
    };
    let (cash, receivable, advances) = (found[0], found[1], found[2]);
    if cash.is_none() || receivable.is_none() || advances.is_none() {
        return Err(AccountingError::not_allowed(format!(
            "a payment needs accounts {}, {} and {} — {}",
            codes[0],
            codes[1],
            codes[2],
            if cash.is_none() {
                describe(0)
            } else if receivable.is_none() {
                describe(1)
            } else {
                describe(2)
            }
        )));
    }
    Ok((cash.unwrap_or_default(), receivable.unwrap_or_default(), advances.unwrap_or_default()))
}

/// Write the entry a payment makes: debit cash, credit receivables, credit the surplus.
#[allow(clippy::too_many_arguments)]
async fn write_payment_entry(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    payment_id: Uuid,
    number: &str,
    paid_on: Date,
    method: PaymentMethod,
    amount: Amount,
    applied: Amount,
    advance: Amount,
    cash_id: Uuid,
    receivable_id: Uuid,
    advances_id: Uuid,
    created_by: Option<Uuid>,
) -> Result<Uuid> {
    let entry_id = insert_entry(
        tx,
        organization_id,
        paid_on,
        &format!("Payment {number} ({})", method.label()),
        payment_id,
        created_by,
        amount,
    )
    .await?;

    // One debit for the whole amount: the money arrived, and the entry says so whether or not
    // it was applied. A payment that arrives unapplied is still cash in the bank today.
    insert_line(tx, organization_id, entry_id, 0, cash_id, "Cash received", amount, Amount::ZERO)
        .await?;

    let mut position = 1;
    if !applied.is_zero() {
        insert_line(
            tx,
            organization_id,
            entry_id,
            position,
            receivable_id,
            "Applied to invoices",
            Amount::ZERO,
            applied,
        )
        .await?;
        position += 1;
    }
    if !advance.is_zero() {
        insert_line(
            tx,
            organization_id,
            entry_id,
            position,
            advances_id,
            "Held as customer credit",
            Amount::ZERO,
            advance,
        )
        .await?;
    }
    Ok(entry_id)
}

/// Write the counter entry a reversal makes: the original's lines, sides swapped.
///
/// `entry_date` is the ORIGINAL payment's date, not today. A reversing entry corrects the
/// period the mistake was made in, and a counter entry dated today against a March receipt
/// leaves March wrong and silently moves the correction into another period's numbers — which
/// is the whole thing a reversal exists to prevent. The `reversed_at` column on the payment
/// still carries when the undo happened, so "when was this undone" and "which period does it
/// correct" are two different facts with two different answers.
#[allow(clippy::too_many_arguments)]
async fn write_reversal_entry(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    payment_id: Uuid,
    number: &str,
    method: PaymentMethod,
    entry_date: Date,
    amount: Amount,
    applied: Amount,
    cash_id: Uuid,
    receivable_id: Uuid,
    advances_id: Uuid,
    reason: &str,
    created_by: Option<Uuid>,
) -> Result<Uuid> {
    let entry_id = insert_entry(
        tx,
        organization_id,
        entry_date,
        &format!("Reversal of {number} ({}) — {reason}", method.label()),
        payment_id,
        created_by,
        amount,
    )
    .await?;

    let advance = amount.minus(applied);
    let mut position = 0;
    // The cash leaves first, then the advances come back, then the receivables — the mirror of
    // the original's order, so an auditor reading the two entries side by side reads them in the
    // same direction.
    insert_line(
        tx,
        organization_id,
        entry_id,
        position,
        cash_id,
        "Cash returned",
        Amount::ZERO,
        amount,
    )
    .await?;
    position += 1;
    if !advance.is_zero() {
        insert_line(
            tx,
            organization_id,
            entry_id,
            position,
            advances_id,
            "Customer credit taken back",
            advance,
            Amount::ZERO,
        )
        .await?;
        position += 1;
    }
    if !applied.is_zero() {
        insert_line(
            tx,
            organization_id,
            entry_id,
            position,
            receivable_id,
            "Invoice payments reversed",
            applied,
            Amount::ZERO,
        )
        .await?;
    }
    Ok(entry_id)
}

/// Insert a journal entry header with both totals already balanced.
async fn insert_entry(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    entry_date: Date,
    memo: &str,
    source_id: Uuid,
    created_by: Option<Uuid>,
    total: Amount,
) -> Result<Uuid> {
    let entry_number: i64 = sqlx::query_scalar(
        "select coalesce(max(entry_number), 0) + 1 from accounting_journal_entries \
         where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(&mut **tx)
    .await?;

    let id: Uuid = sqlx::query_scalar(
        "insert into accounting_journal_entries \
             (organization_id, entry_number, entry_date, memo, source_kind, source_id, \
              debit_total, credit_total, balanced, posted_at, created_by) \
         values ($1, $2, $3, $4, 'payment', $5, $6::numeric, $6::numeric, true, now(), $7) \
         returning id",
    )
    .bind(organization_id)
    .bind(entry_number)
    .bind(entry_date)
    .bind(memo)
    .bind(source_id)
    .bind(total.to_text())
    .bind(created_by)
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

/// Insert one journal line.
#[allow(clippy::too_many_arguments)]
async fn insert_line(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    entry_id: Uuid,
    position: i32,
    account_id: Uuid,
    description: &str,
    debit: Amount,
    credit: Amount,
) -> Result<()> {
    sqlx::query(
        "insert into accounting_journal_lines \
             (entry_id, organization_id, account_id, position, description, debit, credit) \
         values ($1, $2, $3, $4, $5, $6::numeric, $7::numeric)",
    )
    .bind(entry_id)
    .bind(organization_id)
    .bind(account_id)
    .bind(position)
    .bind(description)
    .bind(debit.to_text())
    .bind(credit.to_text())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------------------------------------

/// The customer's name: the caller's, the payment column's, the CRM's, or a refusal.
async fn resolve_customer_name(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewPayment,
) -> Result<String> {
    if let Some(name) = new
        .customer_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if name.chars().count() > MAX_NOTE_LENGTH {
            return Err(AccountingError::invalid(
                "payment",
                "customer_name",
                format!("a customer name is at most {MAX_NOTE_LENGTH} characters"),
            ));
        }
        return Ok(name.to_string());
    }
    let Some(customer_id) = new.customer_id else {
        return Ok(String::from("Unidentified"));
    };

    // The CRM owns the current name. Both tables are searched and the refusal is the same one
    // either way, so a caller cannot learn from the message that the id is a company rather than
    // a contact.
    let found: Option<String> = sqlx::query_scalar(
        "select name from crm_companies where id = $1 and organization_id = $2 \
         union all \
         select full_name from crm_contacts where id = $1 and organization_id = $2 \
         limit 1",
    )
    .bind(customer_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    match found {
        Some(name) => Ok(name),
        None => Err(AccountingError::ForeignKey { kind: "customer", id: customer_id }),
    }
}

/// The currency: the caller's, or the organization's most common invoice currency.
async fn resolve_currency(
    pool: &PgPool,
    organization_id: Uuid,
    requested: Option<String>,
) -> Result<String> {
    if let Some(value) = requested {
        return Ok(value);
    }
    let from_invoices: Option<String> = sqlx::query_scalar(
        "select currency from accounting_invoices where organization_id = $1 \
         order by created_at desc limit 1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    Ok(from_invoices.unwrap_or_else(|| String::from("USD")))
}

/// `amount - allocated`, as text.
fn remaining_text(amount: &str, allocated: &str) -> Result<String> {
    let amount = Amount::parse(amount)
        .map_err(|error| AccountingError::InvalidAmount { message: error.to_string() })?;
    let allocated = Amount::parse(allocated)
        .map_err(|error| AccountingError::InvalidAmount { message: error.to_string() })?;
    Ok(amount.minus(allocated).to_text())
}

fn allocated_text_is_zero(text: &str) -> bool {
    Amount::parse(text).map(|value| value.is_zero()).unwrap_or(false)
}

fn unallocated_text_is_zero(text: &str) -> bool {
    allocated_text_is_zero(text)
}

/// The date the payment was received, defaulting to today.
fn resolve_paid_on(raw: Option<&str>) -> Result<Date> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(OffsetDateTime::now_utc().date()),
        Some(value) => crate::dates::parse(value).map_err(|_| {
            AccountingError::invalid(
                "payment",
                "paid_on",
                format!("a date such as 2026-12-01, not {value:?} — the format is YYYY-MM-DD"),
            )
        }),
    }
}

/// Trim, collapse, cap and default a piece of free text.
fn normalize_text(
    raw: Option<&str>,
    max: usize,
    field: &'static str,
) -> Result<String> {
    let value = raw.unwrap_or("").trim();
    if value.chars().count() > max {
        return Err(AccountingError::invalid(
            "payment",
            field,
            format!("at most {max} characters"),
        ));
    }
    Ok(value.to_string())
}

/// A three-letter currency code, upper-cased.
fn normalize_currency(raw: Option<&str>) -> Result<Option<String>> {
    let Some(value) = raw.map(str::trim).filter(|code| !code.is_empty()) else {
        return Ok(None);
    };
    if value.len() != 3 || !value.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(AccountingError::invalid(
            "payment",
            "currency",
            format!("{value:?} is not a currency code — three letters, like USD or EUR"),
        ));
    }
    Ok(Some(value.to_ascii_uppercase()))
}

/// A required amount, refusing an empty field with a message that names the field.
fn parse_required_amount(
    raw: Option<&str>,
    entity: &'static str,
    field: &'static str,
) -> Result<Amount> {
    let text = raw.map(str::trim).filter(|value| !value.is_empty()).ok_or_else(|| {
        AccountingError::invalid(entity, field, "the amount is required")
    })?;
    Amount::parse(text).map_err(|error| AccountingError::invalid(entity, field, error.to_string()))
}

/// Escape the two characters `LIKE` treats as wildcards, so a search for `100%` finds `100%`
/// rather than every number starting with `100`.
fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_method_this_build_cannot_read_is_none_rather_than_a_default() {
        // A cash payment is a fact about a till. Showing an unknown method as cash invents one.
        assert_eq!(PaymentMethod::parse("cheque"), None);
        assert_eq!(PaymentMethod::parse("cash"), Some(PaymentMethod::Cash));
        assert_eq!(PaymentMethod::Cash.as_str(), "cash");
    }

    #[test]
    fn the_allocation_state_follows_the_two_amounts_and_not_the_other_way_round() {
        let cases = [
            ("100.00", "0.00", AllocationState::Unallocated),
            ("100.00", "40.00", AllocationState::Partial),
            ("100.00", "100.00", AllocationState::Applied),
        ];
        for (amount, allocated, expected) in cases {
            let unallocated = remaining_text(amount, allocated).expect("amounts parse");
            let state = if unallocated_text_is_zero(&unallocated) {
                AllocationState::Applied
            } else if allocated_text_is_zero(allocated) {
                AllocationState::Unallocated
            } else {
                AllocationState::Partial
            };
            assert_eq!(state, expected, "{amount} with {allocated} allocated");
        }
    }

    #[test]
    fn the_payment_amount_may_not_be_missed_silently() {
        let error = parse_required_amount(None, "payment", "amount").expect_err("required");
        assert!(error.to_string().contains("required"), "{error}");
        let error = parse_required_amount(Some("  "), "payment", "amount").expect_err("blank");
        assert!(error.to_string().contains("required"), "{error}");
    }

    #[test]
    fn a_currency_is_three_letters_or_it_is_a_refusal() {
        assert_eq!(normalize_currency(Some("usd")).expect("lowercase is fine"), Some("USD".into()));
        assert_eq!(normalize_currency(None).expect("absent is fine"), None);
        let error = normalize_currency(Some("DOLLAR")).expect_err("too long");
        assert!(error.to_string().contains("three letters"), "{error}");
    }

    #[test]
    fn a_search_term_cannot_smuggle_a_wildcard_into_the_query() {
        // `%` alone would otherwise match every payment in the organization.
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn a_payment_with_a_third_decimal_is_refused_rather_than_rounded() {
        // The module stores hundredths; a payment of 12.345 is a question about rounding the
        // schema cannot answer, and rounding it silently records a different number.
        let error = Amount::parse("12.345").expect_err("three decimals");
        assert!(matches!(error, crate::money::DecimalError::TooPrecise), "{error:?}");
    }
}
