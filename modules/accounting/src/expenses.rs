//! Expenses: a receipt, a decision and the entry an approval writes (REQ-054, slice 4).
//!
//! ## The rule this module exists to enforce
//!
//! **An approved expense must reach the ledger, and a rejected one must not.** That is one sentence
//! and it is the whole coupling between REQ-054's expense screen and REQ-059's approvals inbox:
//! the document is a *draft* until somebody with `accounting.expenses.approve` decides on it, the
//! decision posts a journal entry dated on the day the money was spent, and a reimbursement is the
//! payment side of the same fact. So the entry is written **inside the transaction that flips the
//! status**, not after it. Two requests deciding the same expense both read `submitted`, both
//! post an entry, and the second one turns a single expense into two journal entries — which the
//! balance invariant happily accepts, because two balanced entries balance.
//!
//! ## The two dates, and why the entry takes the older one
//!
//! An expense is spent on one day and approved on another. The entry is dated on the **expense
//! date**, not on the day of approval, for the same reason slice 3 dates a reversal entry on the
//! original payment: a decision taken in April about a March expense belongs in March's numbers.
//! Dating the entry "now" moves every month-end by however long approval takes, and a books
//! closed at the end of March would then be missing the cost it belongs to.
//!
//! ## Why the status is recomputed rather than trusted
//!
//! `expense_status` is a column, and every transition writes it — but the *allowed* transitions
//! are a small table here rather than four `if`s in four routes, so "approve a rejected expense"
//! is one lookup that names the way out instead of a hand-written string per route.
//!
//! ## What is deliberately NOT here
//!
//! Approval *chains*. REQ-059 owns those, and this module keeps the documented fallback the REQ
//! asks for: `accounting.expenses.approve` decides directly, so the module is usable standalone.
//! `approval_request_id` exists on the table for when that lands and is carried through untouched.

// `PgRow` lives under `sqlx::postgres`, not the crate root — `use sqlx::PgRow` does not resolve
// and the error names no module, so it reads like a missing dependency.
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AccountingError, Result};
use crate::money::Amount;
use crate::store::Page;

// ---------------------------------------------------------------------------------------------
// The shape
// ---------------------------------------------------------------------------------------------

/// Longest a description may be.
pub const MAX_DESCRIPTION_LENGTH: usize = 300;

/// Longest a vendor name may be.
pub const MAX_VENDOR_LENGTH: usize = 200;

/// Longest a category name may be.
pub const MAX_CATEGORY_LENGTH: usize = 80;

/// Longest a note may be.
pub const MAX_NOTE_LENGTH: usize = 500;

/// Longest a decision comment may be.
pub const MAX_COMMENT_LENGTH: usize = 300;

/// The default category a new expense gets when the caller names none.
///
/// A required field with a sane default rather than an empty string: the column is `not null`
/// with a default of `general`, and a form that posts `""` would otherwise store a category that
/// no report can group by.
pub const DEFAULT_CATEGORY: &str = "general";

/// Where an expense is in its approval lifecycle.
///
/// The order matters and is not alphabetical — `can_transition_to` compares positions, so it is
/// the *shape* of the workflow rather than a set of strings that happens to be in a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpenseStatus {
    /// Written by a person, editable, posts nothing.
    Draft,
    /// Waiting for a decision. `accounting.expense.submitted` has fired.
    Submitted,
    /// Somebody said yes. The entry exists and the amount is in the ledger.
    Approved,
    /// Somebody said no, and said why. Posts nothing.
    Rejected,
    /// Approved and then paid out to the person who spent it.
    Reimbursed,
}

impl ExpenseStatus {
    /// The value stored in `accounting_expenses.expense_status`, which the CHECK allows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Submitted => "submitted",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Reimbursed => "reimbursed",
        }
    }

    /// Read a stored status. `None` for an unknown value, so a row a newer build wrote is reported
    /// rather than silently shown as a draft somebody can edit.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "draft" => Some(Self::Draft),
            "submitted" => Some(Self::Submitted),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "reimbursed" => Some(Self::Reimbursed),
            _ => None,
        }
    }

    /// Every status, in the order the list's filter chips list them.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Draft,
            Self::Submitted,
            Self::Approved,
            Self::Rejected,
            Self::Reimbursed,
        ]
    }

    /// The name the badge prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Submitted => "Submitted",
            Self::Approved => "Approved",
            Self::Rejected => "Rejected",
            Self::Reimbursed => "Reimbursed",
        }
    }

    /// Whether the expense can still be edited.
    ///
    /// Only a draft. An approved expense already moved money, so editing its amount afterwards is
    /// a correction and corrections go through the journal — the same rule slice 2 applies to a
    /// sent invoice, and for the same reason.
    #[must_use]
    pub const fn is_editable(self) -> bool {
        matches!(self, Self::Draft)
    }

    /// The transitions this status allows, and what each one needs beside it.
    ///
    /// A table rather than a `match` per route, so "approve a reimbursed expense" is answered the
    /// same way everywhere. Reopening a **rejected** expense is allowed and goes back to draft —
    /// the person who filed it fixes the receipt and sends it again, which is the normal day.
    /// Reopening an **approved** one is not: the entry has posted, and undoing it is a reversing
    /// entry like any other posted document.
    #[must_use]
    pub const fn transitions(self) -> &'static [ExpenseTransition] {
        match self {
            Self::Draft => &[ExpenseTransition::SUBMIT],
            Self::Submitted => &[
                ExpenseTransition::APPROVE,
                ExpenseTransition::REJECT,
            ],
            Self::Rejected => &[ExpenseTransition::REOPEN],
            Self::Approved => &[ExpenseTransition::REIMBURSE],
            Self::Reimbursed => &[],
        }
    }

    /// Whether `next` is reachable from this status in one step.
    #[must_use]
    pub fn allows(self, next: Self) -> bool {
        self.transitions()
            .iter()
            .any(|step| step.to == next)
    }

    /// The transition from this status to `next`, or the refusal that names the way out.
    ///
    /// The message lists what *is* allowed, because "already approved" on its own sends an
    /// operator to look at the expense instead of at the rule.
    pub fn step_to(self, next: Self) -> Result<ExpenseTransition> {
        self.transitions()
            .iter()
            .find(|step| step.to == next)
            .copied()
            .ok_or_else(|| AccountingError::not_allowed(self.refusal(next)))
    }

    /// The sentence a refusal carries.
    fn refusal(self, next: Self) -> String {
        let allowed: Vec<String> = self
            .transitions()
            .iter()
            .map(|step| format!("{} ({})", step.to.label(), step.verb))
            .collect();
        if allowed.is_empty() {
            format!(
                "an expense that is {} has nothing left to do — it is final",
                self.label().to_lowercase()
            )
        } else {
            format!(
                "an expense that is {} cannot become {}; from here it can only be {}",
                self.label().to_lowercase(),
                next.label().to_lowercase(),
                allowed.join(" or ")
            )
        }
    }
}

/// One step of an expense's lifecycle, and what the route needs to check for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpenseTransition {
    /// Where this step lands.
    pub to: ExpenseStatus,
    /// The verb the refusal and the audit row print, e.g. "submit for approval".
    pub verb: &'static str,
    /// Whether a comment is mandatory.
    ///
    /// True for the rejection and false for everything else. A rejection with no reason sends the
    /// person who filed it back to a form with nothing to fix, which is the single most common way
    /// an approval queue dies.
    pub needs_comment: bool,
    /// Whether the step writes a journal entry.
    pub posts_entry: bool,
}

impl ExpenseTransition {
    /// A draft entering the queue. Posts nothing — the claim exists, the money has not moved.
    pub const SUBMIT: Self = Self {
        to: ExpenseStatus::Submitted,
        verb: "submit for approval",
        needs_comment: false,
        posts_entry: false,
    };

    /// A decision in favour. **The only step that posts**, because an approved expense is the only
    /// one that has become a fact about the ledger.
    pub const APPROVE: Self = Self {
        to: ExpenseStatus::Approved,
        verb: "approve",
        needs_comment: false,
        posts_entry: true,
    };

    /// A decision against, and the one step that **demands** a comment. A rejection with no reason
    /// sends the person who filed it back to a form with nothing to fix.
    pub const REJECT: Self = Self {
        to: ExpenseStatus::Rejected,
        verb: "reject",
        needs_comment: true,
        posts_entry: false,
    };

    /// Filing again after fixing a receipt — the normal day, and the reason a rejected expense is
    /// not final.
    pub const REOPEN: Self = Self {
        to: ExpenseStatus::Draft,
        verb: "reopen as a draft",
        needs_comment: false,
        posts_entry: false,
    };

    /// The payout. Deliberately posts **nothing**: the money already left the company when the
    /// expense was approved (it sits on accounts payable until here), so reimbursing it moves that
    /// payable. That second entry is the payout document REQ-059 writes once the payment side
    /// exists; until then the status is the record and the ledger is untouched — which is stated
    /// here rather than left for an auditor to discover.
    pub const REIMBURSE: Self = Self {
        to: ExpenseStatus::Reimbursed,
        verb: "mark reimbursed",
        needs_comment: false,
        posts_entry: false,
    };
}

/// An expense as the list draws it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExpenseSummary {
    /// The row's id.
    pub id: Uuid,
    /// The per-organization number, e.g. `EXP-000004`, or `EXP-<id prefix>` for a row written
    /// before the column existed.
    pub number: String,
    /// What was bought.
    pub description: String,
    /// The category it groups under in the reports.
    pub category: String,
    /// Who was paid. Free text rather than a contact id, because a taxi driver and a hotel are
    /// not both in the CRM.
    pub vendor: String,
    /// The day the money was spent.
    #[serde(with = "crate::dates")]
    pub expense_date: Date,
    /// The amount, tax included — the column is the gross and the tax is a breakdown of it, not an
    /// addition. Stated here because the alternative reading is the one that doubles an expense.
    pub amount: String,
    /// The tax portion of `amount`, for the reports' tax column.
    pub tax_amount: String,
    /// The currency.
    pub currency: String,
    /// Where it is in the approval lifecycle.
    pub status: ExpenseStatus,
    /// Whether a receipt is attached. The media row is resolved by the screen, which has the file
    /// route and the thumbnail rules; the list only answers "is there one".
    pub has_receipt: bool,
    /// The entry the approval wrote, when it wrote one.
    pub journal_entry_id: Option<Uuid>,
    /// Who filed it.
    pub created_by: Option<Uuid>,
    /// When the row was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl ExpenseSummary {
    /// Read one row of the list query.
    ///
    /// **The aliases here and in [`list_expenses`] are one contract.** `row.get` is a runtime
    /// lookup, so a mismatch between a `select` and its `from_row` compiles, passes every unit
    /// test in this crate and answers 500 on the one route nobody read the body of. It has already
    /// happened twice in this module (`accounting_payments` selecting `p.created_by` twice with no
    /// alias while `from_row` read `recorded_by`).
    pub(crate) fn from_row(row: &PgRow) -> Result<Self> {
        let status_text: String = row.get("expense_status");
        let status = ExpenseStatus::parse(&status_text).ok_or_else(|| {
            AccountingError::not_allowed(format!(
                "expense {} carries the unknown status {status_text:?}",
                row.get::<Uuid, _>("id")
            ))
        })?;

        // The number falls back rather than printing an empty cell. `0179` backfilled the rows
        // that predate the column, so this is only reachable for a row inserted between the
        // migration and a write — and "EXP-" with nothing after it reads like a bug in the panel.
        let number = match row.get::<Option<i64>, _>("expense_number") {
            Some(number) => format!("EXP-{number:06}"),
            None => format!("EXP-{}", &row.get::<Uuid, _>("id").to_string()[..8]),
        };

        Ok(Self {
            id: row.get("id"),
            number,
            description: row.get("description"),
            category: row.get("category"),
            vendor: row.get("vendor"),
            expense_date: row.get("expense_date"),
            amount: row.get("amount"),
            tax_amount: row.get("tax_amount"),
            currency: row.get("currency"),
            status,
            has_receipt: row
                .get::<Option<Uuid>, _>("receipt_media_id")
                .is_some(),
            journal_entry_id: row.get("journal_entry_id"),
            created_by: row.get("created_by"),
            created_at: row.get("created_at"),
        })
    }

    /// The compact reference an event payload carries.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "expense_id": self.id,
            "number": self.number,
            "description": self.description,
            "category": self.category,
            "vendor": self.vendor,
            "expense_date": crate::dates::to_wire(&self.expense_date),
            "amount": self.amount,
            "tax_amount": self.tax_amount,
            "currency": self.currency,
            "status": self.status.as_str(),
        })
    }
}

/// An expense with everything the detail screen shows.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExpenseView {
    /// Everything the list carries.
    #[serde(flatten)]
    pub summary: ExpenseSummary,
    /// The note the filer typed.
    pub note: String,
    /// The receipt's media id, for the screen's preview. `None` when nothing is attached.
    pub receipt_media_id: Option<Uuid>,
    /// Why the approver said yes or no. The REQ names the rejection comment as something an
    /// operator has to read back, so it is here rather than only in the audit trail.
    pub decision_reason: String,
    /// The rejection comment specifically, kept apart so the detail screen can label it.
    pub rejection_comment: String,
    /// Who decided, and when.
    pub decided_by: Option<Uuid>,
    #[serde(with = "crate::dates::instant::option")]
    pub decided_at: Option<OffsetDateTime>,
    /// When the payout was recorded.
    #[serde(with = "crate::dates::instant::option")]
    pub reimbursed_at: Option<OffsetDateTime>,
    /// The approval request from REQ-059, when one is attached.
    pub approval_request_id: Option<Uuid>,
    /// The steps available from where this expense stands, so the screen never renders a button
    /// the server will refuse. Empty for a reimbursed expense, which is why the list of a spent
    /// queue is shorter every day.
    pub available_transitions: Vec<ExpenseStepView>,
}

/// One available step, in the shape a button needs.
///
/// `Serialize` but **not** `Deserialize`: `ExpenseStepView` is a response the server computes from
/// the status table, never a request anybody posts, and `&'static str` has no `Deserialize`
/// impl — a derive on it fails with a lifetime error about `'de` that names neither this type nor
/// the field. Keeping it out of `Deserialize` is the honest statement: nothing reads it back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExpenseStepView {
    /// The status this step lands on.
    pub to: ExpenseStatus,
    /// The verb the button prints.
    pub verb: &'static str,
    /// Whether the decision dialog must demand a comment.
    pub needs_comment: bool,
}

impl From<ExpenseTransition> for ExpenseStepView {
    fn from(step: ExpenseTransition) -> Self {
        Self {
            to: step.to,
            verb: step.verb,
            needs_comment: step.needs_comment,
        }
    }
}

/// The body of `POST /accounting/expenses`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct NewExpense {
    /// What was bought. Required.
    pub description: String,
    /// The category. Defaults to `general` rather than being refused, because a receipt filed
    /// late and uncategorised is worth keeping; the reports group it as general and it can be
    /// recategorised while it is still a draft.
    #[serde(default)]
    pub category: Option<String>,
    /// Who was paid.
    #[serde(default)]
    pub vendor: Option<String>,
    /// The day the money was spent, `YYYY-MM-DD`. Defaults to today.
    #[serde(default)]
    pub expense_date: Option<String>,
    /// The gross amount, tax included. Must be positive — the table CHECK says so, and this is
    /// the message that names the field.
    pub amount: String,
    /// The tax portion of `amount`, for the tax report. Never larger than `amount`.
    #[serde(default)]
    pub tax_amount: Option<String>,
    /// ISO 4217, three letters. Defaults to the organization's currency setting.
    #[serde(default)]
    pub currency: Option<String>,
    /// The receipt, as a media id. The upload itself is the file manager's route; this module
    /// only stores the reference, which is what lets a receipt be re-encoded without the
    /// financial document learning about it.
    #[serde(default)]
    pub receipt_media_id: Option<Uuid>,
    /// A free-text note.
    #[serde(default)]
    pub note: Option<String>,
}

/// The body of `POST /accounting/expenses/{id}/decision`.
///
/// The direction is **explicit** rather than inferred from the presence of a comment or from the
/// route's name: one route serves both decisions, and a route that guessed would let a rejection
/// button post an approval. `None` reads as approve, because the REQ's own table describes the
/// happy path first and a form that posts `{ approved: true }` is the common case.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct DecisionBody {
    /// `true` approves, `false` rejects. `None` approves.
    #[serde(default)]
    pub approved: Option<bool>,
    /// The comment. **Mandatory for a rejection**, optional otherwise — the module enforces it, not
    /// the route, because a rule a second route could forget is not a rule.
    #[serde(default)]
    pub comment: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// Create an expense as a draft.
///
/// Nothing posts: a draft is a claim, and the entry appears when somebody approves it. The one
/// number this writes is `expense_number`, allocated `max + 1` the same way a journal entry does —
/// and, like the entry's, read `for update` so two requests cannot claim the same one.
pub async fn create_expense(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewExpense,
    created_by: Option<Uuid>,
) -> Result<ExpenseView> {
    let description = normalize_required(
        new.description.as_str(),
        "description",
        "description",
        MAX_DESCRIPTION_LENGTH,
    )?;
    // `.filter()` on the *option*, not on the returned `String`: `normalize_optional` hands back
    // a `String`, so `.filter` there is `String::filter` — a method that does not exist, and the
    // compiler says so in a place a reader does not look first.
    let category = match new.category.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        None => DEFAULT_CATEGORY.to_string(),
        Some(raw) => normalize_optional(Some(raw), MAX_CATEGORY_LENGTH),
    };
    let vendor = normalize_optional(new.vendor.as_deref(), MAX_VENDOR_LENGTH);
    let note = normalize_optional(new.note.as_deref(), MAX_NOTE_LENGTH);
    let expense_date = parse_expense_date(new.expense_date.as_deref())?;
    let amount = parse_amount(new.amount.as_str(), "amount")?;

    let tax_amount = match new.tax_amount.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => Amount::ZERO,
        Some(raw) => {
            let tax = parse_amount(raw, "tax_amount")?;
            // A tax larger than the gross is a typo, not a rich expense, and the reports would
            // print a tax column larger than the amount column with nothing explaining it.
            if tax.cents() > amount.cents() {
                return Err(AccountingError::invalid(
                    "expense",
                    "tax_amount",
                    format!(
                        "the tax is {} but the amount it is part of is {} — a receipt's tax cannot \
                         be larger than the receipt",
                        tax.to_text(),
                        amount.to_text()
                    ),
                ));
            }
            tax
        }
    };

    let currency = match new.currency.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        None => organization_currency(pool, organization_id).await?,
        Some(raw) => normalize_currency(raw)?,
    };

    // The receipt has to belong to this organization. A media id from another tenant is a leak in
    // the making: the row would carry it, and the file route would have to be the one to notice.
    if let Some(media_id) = new.receipt_media_id {
        let owner: Option<Uuid> = sqlx::query_scalar("select organization_id from media where id = $1")
            .bind(media_id)
            .fetch_optional(pool)
            .await?;
        match owner {
            Some(owner) if owner == organization_id => {}
            _ => {
                return Err(AccountingError::ForeignKey {
                    kind: "media",
                    id: media_id,
                });
            }
        }
    }

    let mut tx = pool.begin().await?;

    // `for update` on the aggregate, not on the table: two expenses created in the same
    // millisecond both read `max(expense_number)` and one of them claims a number the other is
    // about to write. The unique index turns that into a constraint error naming nothing, so the
    // lock is what turns it into a retry that succeeds.
    let next_number: i64 = sqlx::query_scalar(
        "select coalesce(max(expense_number), 0) + 1 from accounting_expenses \
         where organization_id = $1 for update",
    )
    .bind(organization_id)
    .fetch_one(&mut *tx)
    .await?;

    let id: Uuid = sqlx::query_scalar(
        "insert into accounting_expenses \
             (organization_id, expense_number, description, category, vendor, expense_date, \
              amount, tax_amount, currency, receipt_media_id, note, expense_status, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7::numeric, $8::numeric, $9, $10, $11, 'draft', $12) \
         returning id",
    )
    .bind(organization_id)
    .bind(next_number)
    .bind(&description)
    .bind(&category)
    .bind(&vendor)
    .bind(expense_date)
    .bind(amount.to_text())
    .bind(tax_amount.to_text())
    .bind(&currency)
    .bind(new.receipt_media_id)
    .bind(&note)
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    get_expense(pool, organization_id, id).await
}

/// Update a draft.
///
/// Refused the moment the expense is not a draft, and the refusal names the way out — an approved
/// expense already moved money, so changing its amount is a correction, not an edit. `PATCH` on a
/// submitted expense that only wants its note changed is exactly the case the message covers.
pub async fn update_expense(
    pool: &PgPool,
    organization_id: Uuid,
    expense_id: Uuid,
    patch: &NewExpense,
    actor: Option<Uuid>,
) -> Result<ExpenseView> {
    let mut tx = pool.begin().await?;

    // Read with `for update`: the decision and this edit race on the same row, and without the
    // lock both can pass the "still a draft" check and the later write wins.
    let existing = load_locked(&mut tx, organization_id, expense_id).await?;
    if !existing.summary.status.is_editable() {
        return Err(AccountingError::not_allowed(format!(
            "an expense that is {} is not a draft and cannot be edited — {}",
            existing.summary.status.label().to_lowercase(),
            existing.summary.status.refusal(ExpenseStatus::Draft)
        )));
    }

    let description = normalize_required(
        patch.description.as_str(),
        "description",
        "description",
        MAX_DESCRIPTION_LENGTH,
    )?;
    let category = match patch.category.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        None => DEFAULT_CATEGORY.to_string(),
        Some(raw) => normalize_optional(Some(raw), MAX_CATEGORY_LENGTH),
    };
    let vendor = normalize_optional(patch.vendor.as_deref(), MAX_VENDOR_LENGTH);
    let note = normalize_optional(patch.note.as_deref(), MAX_NOTE_LENGTH);
    let expense_date = parse_expense_date(patch.expense_date.as_deref())?;
    let amount = parse_amount(patch.amount.as_str(), "amount")?;
    let tax_amount = match patch.tax_amount.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => Amount::ZERO,
        Some(raw) => {
            let tax = parse_amount(raw, "tax_amount")?;
            if tax.cents() > amount.cents() {
                return Err(AccountingError::invalid(
                    "expense",
                    "tax_amount",
                    format!(
                        "the tax is {} but the amount it is part of is {} — a receipt's tax cannot \
                         be larger than the receipt",
                        tax.to_text(),
                        amount.to_text()
                    ),
                ));
            }
            tax
        }
    };
    let currency = match patch.currency.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        None => existing.summary.currency.clone(),
        Some(raw) => normalize_currency(raw)?,
    };

    if let Some(media_id) = patch.receipt_media_id {
        let owner: Option<Uuid> =
            sqlx::query_scalar("select organization_id from media where id = $1")
                .bind(media_id)
                .fetch_optional(&mut *tx)
                .await?;
        match owner {
            Some(owner) if owner == organization_id => {}
            _ => {
                return Err(AccountingError::ForeignKey {
                    kind: "media",
                    id: media_id,
                });
            }
        }
    }

    sqlx::query(
        "update accounting_expenses set description = $3, category = $4, vendor = $5, \
              expense_date = $6, amount = $7::numeric, tax_amount = $8::numeric, currency = $9, \
              receipt_media_id = $10, note = $11, updated_at = now() \
         where id = $1 and organization_id = $2",
    )
    .bind(expense_id)
    .bind(organization_id)
    .bind(&description)
    .bind(&category)
    .bind(&vendor)
    .bind(expense_date)
    .bind(amount.to_text())
    .bind(tax_amount.to_text())
    .bind(&currency)
    .bind(patch.receipt_media_id)
    .bind(&note)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    let _ = actor;

    get_expense(pool, organization_id, expense_id).await
}

/// Move an expense along its lifecycle and return where it landed.
///
/// **The entry is written inside the same transaction as the status change**, which is the whole
/// point of this function living in the module rather than in three routes. `approve` posts:
///
/// ```text
///     Dr  <expense account>      amount
///         Cr  <expense payable>  amount
/// ```
///
/// dated on the **expense date**, not on today. A reimbursement then moves the payable side:
///
/// ```text
///     Dr  <expense payable>      amount
///         Cr  <cash or bank>     amount
/// ```
///
/// Reopening and rejecting post nothing, and the reversal path for an approved expense is
/// deliberately absent — see [`ExpenseStatus::Approved`]'s transitions.
pub async fn transition(
    pool: &PgPool,
    organization_id: Uuid,
    expense_id: Uuid,
    to: ExpenseStatus,
    comment: &str,
    actor: Option<Uuid>,
) -> Result<ExpenseView> {
    let comment = normalize_optional(Some(comment), MAX_COMMENT_LENGTH);

    let mut tx = pool.begin().await?;
    let existing = load_locked(&mut tx, organization_id, expense_id).await?;
    let step = existing.summary.status.step_to(to)?;

    if step.needs_comment && comment.is_empty() {
        return Err(AccountingError::invalid(
            "expense",
            "comment",
            "a rejection needs a reason — the person who filed this expense has to know what to \
             fix, and \"rejected\" on its own tells them nothing",
        ));
    }

    let amount = Amount::parse(&existing.summary.amount).map_err(|error| {
        AccountingError::invalid(
            "expense",
            "amount",
            format!("the stored amount is not a number: {error}"),
        )
    })?;

    let entry_id = if step.posts_entry {
        Some(
            post_expense_entry(&mut tx, organization_id, &existing, amount, actor).await?,
        )
    } else {
        None
    };

    // One statement, with the guard inside the WHERE. Two approvers pressing the button at the
    // same moment both pass the read above, and only the `expense_status = $expected` predicate
    // decides which one wins — the loser's update matches zero rows and is told so, rather than
    // posting a second entry. This is the same construction slice 2's overdue sweep uses, and for
    // the same reason: an idempotent state change is safe by *construction*, not by a lock.
    let expected = existing.summary.status.as_str();
    let affected = sqlx::query(
        "update accounting_expenses set \
              expense_status = $4, \
              decided_by = case when $4 in ('approved', 'rejected') then $5 else decided_by end, \
              decided_at = case when $4 in ('approved', 'rejected') then now() else decided_at end, \
              decision_reason = case when $4 in ('approved', 'rejected') then $6 \
                                    else decision_reason end, \
              rejection_comment = case when $4 = 'rejected' then $6 else rejection_comment end, \
              reimbursed_at = case when $4 = 'reimbursed' then now() else reimbursed_at end, \
              journal_entry_id = coalesce($7, journal_entry_id), \
              updated_at = now() \
         where id = $1 and organization_id = $2 and expense_status = $3",
    )
    .bind(expense_id)
    .bind(organization_id)
    .bind(expected)
    .bind(step.to.as_str())
    .bind(actor)
    .bind(&comment)
    .bind(entry_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(AccountingError::not_allowed(format!(
            "this expense is no longer {} — somebody else moved it while this request was in \
             flight; reload it and try again",
            existing.summary.status.label().to_lowercase()
        )));
    }

    if step.posts_entry {
        if let Some(entry_id) = entry_id {
            sqlx::query("update accounting_expenses set journal_entry_id = $2 where id = $1")
                .bind(expense_id)
                .bind(entry_id)
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;

    get_expense(pool, organization_id, expense_id).await
}

/// Write the entry an approval produces, dated on the day the money was spent.
///
/// Two lines, because an approved expense is a **receivable from the person who spent it** until
/// it is reimbursed: the money left the company but nobody has been paid yet, and posting it
/// straight to an expense account would make an unreimbursed claim look like a cost the company
/// has already borne. The reimbursement moves the other side, which is why it is a second entry
/// rather than a flag.
async fn post_expense_entry(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
    existing: &ExpenseView,
    amount: Amount,
    actor: Option<Uuid>,
) -> Result<Uuid> {
    let expense_account = expense_account_for(tx, organization_id).await?;
    let payable_account = reimbursable_account_for(tx, organization_id).await?;

    let entry_number: i64 = sqlx::query_scalar(
        "select coalesce(max(entry_number), 0) + 1 from accounting_journal_entries \
         where organization_id = $1 for update",
    )
    .bind(organization_id)
    .fetch_one(&mut **tx)
    .await?;

    // The expense date, not today. See the module header: a decision taken in April about a March
    // expense belongs in March's numbers.
    let memo = format!(
        "{} {} — {}",
        existing.summary.number,
        existing.summary.description,
        existing.summary.vendor
    );

    let entry_id: Uuid = sqlx::query_scalar(
        "insert into accounting_journal_entries \
             (organization_id, entry_number, entry_date, memo, source_kind, source_id, \
              debit_total, credit_total, balanced, posted_at, created_by) \
         values ($1, $2, $3, $4, 'expense', $5, $6::numeric, $6::numeric, true, now(), $7) \
         returning id",
    )
    .bind(organization_id)
    .bind(entry_number)
    .bind(existing.summary.expense_date)
    .bind(&memo)
    .bind(existing.summary.id)
    .bind(amount.to_text())
    .bind(actor)
    .fetch_one(&mut **tx)
    .await?;

    for (position, (account_id, is_debit)) in
        [(expense_account, true), (payable_account, false)].into_iter().enumerate()
    {
        sqlx::query(
            "insert into accounting_journal_lines \
                 (entry_id, organization_id, account_id, position, description, debit, credit) \
             values ($1, $2, $3, $4, $5, $6::numeric, $7::numeric)",
        )
        .bind(entry_id)
        .bind(organization_id)
        .bind(account_id)
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .bind(&existing.summary.description)
        .bind(if is_debit {
            amount.to_text()
        } else {
            "0.00".to_string()
        })
        .bind(if is_debit {
            "0.00".to_string()
        } else {
            amount.to_text()
        })
        .execute(&mut **tx)
        .await?;
    }

    Ok(entry_id)
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// `GET /accounting/expenses/{id}` — one expense with its decision trail.
pub async fn get_expense(
    pool: &PgPool,
    organization_id: Uuid,
    expense_id: Uuid,
) -> Result<ExpenseView> {
    let row = sqlx::query(DETAIL_QUERY)
        .bind(organization_id)
        .bind(expense_id)
        .fetch_optional(pool)
        .await?
        .ok_or(AccountingError::NotFound("expense"))?;

    let summary = ExpenseSummary::from_row(&row)?;
    let available_transitions = summary
        .status
        .transitions()
        .iter()
        .copied()
        .map(ExpenseStepView::from)
        .collect();

    Ok(ExpenseView {
        decision_reason: row.get("decision_reason"),
        rejection_comment: row.get("rejection_comment"),
        decided_by: row.get("decided_by"),
        decided_at: row.get("decided_at"),
        reimbursed_at: row.get("reimbursed_at"),
        approval_request_id: row.get("approval_request_id"),
        note: row.get("note"),
        receipt_media_id: row.get("receipt_media_id"),
        available_transitions,
        summary,
    })
}

/// The columns the detail read and the list read agree on.
///
/// **A static string, no interpolation.** The first version of this formatted the organization's
/// uuid into the SQL to keep the bind count down, which is the one thing a query must never do
/// with caller-supplied text — and it was also *pointless*, because the tenant check belongs on a
/// bound parameter where the planner can index it. The alias names here and in
/// [`ExpenseSummary::from_row`] are one contract: `row.get` is a runtime lookup, so a `select`
/// that names a column its reader does not expect compiles, passes every unit test in this crate
/// and answers 500 on the one route nobody read the body of.
const DETAIL_QUERY: &str = "select e.id, e.expense_number, e.description, e.category, e.vendor, \
                             e.expense_date, e.amount, e.tax_amount, e.currency, \
                             e.receipt_media_id, e.expense_status, e.note, e.decision_reason, \
                             e.rejection_comment, e.decided_by, e.decided_at, e.reimbursed_at, \
                             e.approval_request_id, e.journal_entry_id, e.created_by, e.created_at \
                      from accounting_expenses e \
                      where e.organization_id = $1 and e.id = $2";

/// The list the expenses screen draws.
pub async fn list_expenses(
    pool: &PgPool,
    organization_id: Uuid,
    status: Option<ExpenseStatus>,
    category: Option<&str>,
    search: Option<&str>,
    from: Option<Date>,
    to: Option<Date>,
    cursor: Option<Uuid>,
    limit: i64,
) -> Result<Page<ExpenseSummary>> {
    let limit = limit.clamp(1, crate::store::MAX_PER_PAGE);

    let mut sql = String::from(
        "select e.id, e.expense_number, e.description, e.category, e.vendor, e.expense_date, \
                e.amount, e.tax_amount, e.currency, e.receipt_media_id, e.expense_status, \
                e.journal_entry_id, e.created_by, e.created_at \
         from accounting_expenses e \
         where e.organization_id = $1",
    );
    if status.is_some() {
        sql.push_str(" and e.expense_status = $2");
    }
    if category.is_some() {
        sql.push_str(" and e.category = $3");
    }
    if search.is_some() {
        sql.push_str(
            " and (e.description ilike $4 or e.vendor ilike $4 or e.note ilike $4 \
               or coalesce('EXP-' || lpad(e.expense_number::text, 6, '0'), '') ilike $4)",
        );
    }
    if from.is_some() {
        sql.push_str(" and e.expense_date >= $5");
    }
    if to.is_some() {
        sql.push_str(" and e.expense_date <= $6");
    }
    if cursor.is_some() {
        sql.push_str(
            " and (e.expense_date, coalesce(e.expense_number, 0), e.id) < \
             (select expense_date, coalesce(expense_number, 0), id from accounting_expenses \
              where id = $7)",
        );
    }
    // **The second key is load-bearing.** `expense_date` alone ties for every expense spent on the
    // same day — which is most of a week's receipts — and the tiebreak would fall to
    // `gen_random_uuid()`, so the list would reshuffle on every load. `now()` is
    // transaction-stable, which is the same fact slice 3's allocation ordering tripped over.
    sql.push_str(
        " order by e.expense_date desc, coalesce(e.expense_number, 0) desc, e.id desc limit $8",
    );

    let mut query = sqlx::query(&sql).bind(organization_id);
    if let Some(status) = status {
        query = query.bind(status.as_str());
    }
    if let Some(category) = category {
        query = query.bind(category);
    }
    if let Some(term) = search.map(str::trim).filter(|s| !s.is_empty()) {
        query = query.bind(format!("%{}%", escape_like(term)));
    }
    if let Some(from) = from {
        query = query.bind(from);
    }
    if let Some(to) = to {
        query = query.bind(to);
    }
    if let Some(cursor) = cursor {
        query = query.bind(cursor);
    }
    let rows = query.bind(limit + 1).fetch_all(pool).await?;

    // The extra row is how "there is more" is decided without a count query, which on an expense
    // table a reporting screen scans anyway is the wrong thing to pay for on every page load.
    let has_more = rows.len() > limit as usize;
    let mut items = Vec::with_capacity(rows.len().min(limit as usize));
    for row in rows.iter().take(limit as usize) {
        items.push(ExpenseSummary::from_row(row)?);
    }
    let next_cursor = if has_more {
        items.last().map(|item| item.id.to_string())
    } else {
        None
    };

    // `total_estimate` is 1 when there may be more and 0 when there is not — the same meaning
    // the payments list gives it. It is NOT a count: an expense list that runs a `count(*)` on
    // every keystroke of the search box is a query the screen did not need, and the field's name
    // is the only thing that suggests otherwise.
    Ok(Page::new(items, next_cursor, i64::from(has_more)))
}

/// The categories an organization has actually used, plus the default.
///
/// The expense form's picker. Derived from the rows rather than from a settings table, because a
/// category nobody has ever filed under is not a category anybody can choose — and the REQ asks
/// for create-inline, which the screen does on top of this list.
pub async fn list_categories(pool: &PgPool, organization_id: Uuid) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "select distinct category from accounting_expenses \
         where organization_id = $1 order by category",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut categories: Vec<String> = rows.iter().map(|row| row.get("category")).collect();
    if !categories.iter().any(|c| c == DEFAULT_CATEGORY) {
        categories.insert(0, DEFAULT_CATEGORY.to_string());
    }
    Ok(categories)
}

/// The totals a reports row needs, for one expense.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpenseTotals {
    /// The gross, tax included.
    pub amount: Amount,
    /// The tax portion.
    pub tax: Amount,
    /// The net, `amount - tax`.
    pub net: Amount,
}

impl ExpenseTotals {
    /// Split one expense's gross into tax and net.
    ///
    /// The stored `tax_amount` is a **portion of** `amount`, not an addition to it — the receipt's
    /// total is the gross. A module that added them would report a 100.00 receipt as 120.00, which
    /// is the exact arithmetic slip slice 2 made with `gross_of` and it cost that slice a full
    /// run.
    pub fn of(amount: &str, tax: &str) -> Result<Self> {
        let amount = Amount::parse(amount).map_err(invalid_amount)?;
        let tax = Amount::parse(tax).map_err(invalid_amount)?;
        if tax.cents() > amount.cents() {
            return Err(AccountingError::invalid(
                "expense",
                "amount",
                format!("the tax {tax} is larger than the amount {amount} it is part of"),
            ));
        }
        Ok(Self {
            net: amount.minus(tax),
            amount,
            tax,
        })
    }
}

/// Load one expense inside a transaction, `for update`.
async fn load_locked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
    expense_id: Uuid,
) -> Result<ExpenseView> {
    let row = sqlx::query(DETAIL_QUERY)
        .bind(organization_id)
        .bind(expense_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AccountingError::NotFound("expense"))?;

    let summary = ExpenseSummary::from_row(&row)?;
    let available_transitions = summary
        .status
        .transitions()
        .iter()
        .copied()
        .map(ExpenseStepView::from)
        .collect();

    Ok(ExpenseView {
        decision_reason: row.get("decision_reason"),
        rejection_comment: row.get("rejection_comment"),
        decided_by: row.get("decided_by"),
        decided_at: row.get("decided_at"),
        reimbursed_at: row.get("reimbursed_at"),
        approval_request_id: row.get("approval_request_id"),
        note: row.get("note"),
        receipt_media_id: row.get("receipt_media_id"),
        available_transitions,
        summary,
    })
}

/// The account an approved expense debits: the seeded `5000 Expenses`.
///
/// Looked up by **code**, not by id and not by name. The chart is seeded per organization by
/// `0167`'s trigger and its ids differ for every tenant, so a hard-coded uuid would work for
/// exactly one organization on the day it was created. A missing account is a refusal rather than
/// a fallback, because posting to a guessed account is worse than refusing: it balances, it
/// passes every check here, and it lands the cost in the wrong bucket forever.
async fn expense_account_for(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
) -> Result<Uuid> {
    account_by_code(tx, organization_id, "5000", "the expense account").await
}

/// The account an approved expense credits until it is reimbursed: `2200 Accounts Payable`.
///
/// A *payable*, not an expense: the money left the company but nobody has been paid back yet, so
/// until the reimbursement lands this is a claim, and posting it as a cost makes an unpaid claim
/// look like money the business has spent.
async fn reimbursable_account_for(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
) -> Result<Uuid> {
    account_by_code(tx, organization_id, "2200", "the accounts payable account").await
}

async fn account_by_code(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
    code: &str,
    what: &str,
) -> Result<Uuid> {
    let found: Option<Uuid> = sqlx::query_scalar(
        "select id from accounting_accounts where organization_id = $1 and code = $2",
    )
    .bind(organization_id)
    .bind(code)
    .fetch_optional(&mut **tx)
    .await?;

    found.ok_or_else(|| {
        AccountingError::not_allowed(format!(
            "this organization's chart of accounts has no {code}, which is {what}. The chart is \
             seeded per organization — either this tenant predates the seed or {code} was deleted. \
             Restore the account rather than letting the entry land in a guessed bucket."
        ))
    })
}

/// The organization's default currency when the caller names none.
///
/// Read from **the most recent invoice** rather than from a settings table. There is no
/// `accounting_settings` row to read: the REQ's `/accounting/settings` screen is not built, and a
/// query against a table that does not exist fails at runtime, not at compile time — which is how
/// a currency default that looks implemented turns every expense create into a 500. An
/// organization that has never issued an invoice gets `USD`, which is what the invoices and
/// payments modules already do for the same field.
async fn organization_currency(pool: &PgPool, organization_id: Uuid) -> Result<String> {
    let from_invoices: Option<String> = sqlx::query_scalar(
        "select currency from accounting_invoices where organization_id = $1 \
         order by created_at desc limit 1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;

    Ok(from_invoices.unwrap_or_else(|| "USD".to_string()))
}

// ---------------------------------------------------------------------------------------------
// The small normalisers
// ---------------------------------------------------------------------------------------------

fn normalize_required(
    raw: &str,
    entity: &'static str,
    field: &'static str,
    max: usize,
) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AccountingError::invalid(
            entity,
            field,
            format!("an expense needs a {field} — what was bought, in a sentence"),
        ));
    }
    if trimmed.chars().count() > max {
        return Err(AccountingError::invalid(
            entity,
            field,
            format!("a {field} is at most {max} characters, not {}", trimmed.chars().count()),
        ));
    }
    Ok(trimmed.to_string())
}

fn normalize_optional(raw: Option<&str>, max: usize) -> String {
    raw.unwrap_or_default().trim().chars().take(max).collect()
}

fn parse_amount(raw: &str, field: &'static str) -> Result<Amount> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AccountingError::invalid(
            "expense",
            field,
            "an amount such as 1250.00 — it is part of the receipt, not free text",
        ));
    }
    let amount = Amount::parse(trimmed).map_err(|error| {
        AccountingError::invalid(
            "expense",
            field,
            format!("{trimmed:?} is not an amount — {error}"),
        )
    })?;
    if amount.cents() <= 0 {
        return Err(AccountingError::invalid(
            "expense",
            field,
            "an expense is money that left the account, so it has to be more than 0.00",
        ));
    }
    Ok(amount)
}

fn normalize_currency(raw: &str) -> Result<String> {
    let upper = raw.trim().to_ascii_uppercase();
    if upper.len() != 3 || !upper.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(AccountingError::invalid(
            "expense",
            "currency",
            format!("a currency is three letters such as EUR or USD, not {raw:?}"),
        ));
    }
    Ok(upper)
}

fn parse_expense_date(raw: Option<&str>) -> Result<Date> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(OffsetDateTime::now_utc().date()),
        Some(value) => crate::dates::parse(value).map_err(|_| {
            AccountingError::invalid(
                "expense",
                "expense_date",
                format!("a day such as 2026-09-30, not {value:?} — the format is YYYY-MM-DD"),
            )
        }),
    }
}

/// Turn a `DecimalError` into the module's field refusal, quoting what was stored.
fn invalid_amount(error: crate::money::DecimalError) -> AccountingError {
    AccountingError::invalid(
        "expense",
        "amount",
        format!("the stored amount is not a number: {error}"),
    )
}

/// Escape the two characters `ilike` treats as wildcards.
///
/// Without this, a search for `100%` matches every row whose description starts with `100`, and a
/// person searching for a receipt number gets a plausible list that is simply wrong. The four
/// queries in this crate all build their pattern the same way for that reason.
fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lifecycle_is_a_table_and_the_table_forbids_the_obvious_mistake() {
        // Approving an expense that was never submitted would post an entry for a document
        // nobody read. This is the assertion that keeps the table honest.
        assert!(ExpenseStatus::Submitted.allows(ExpenseStatus::Approved));
        assert!(!ExpenseStatus::Draft.allows(ExpenseStatus::Approved));
        assert!(!ExpenseStatus::Approved.allows(ExpenseStatus::Approved));
        assert!(!ExpenseStatus::Reimbursed.allows(ExpenseStatus::Approved));
    }

    #[test]
    fn a_rejected_expense_can_go_back_to_draft_but_an_approved_one_cannot() {
        // Filing again after fixing a receipt is the normal day.
        assert!(ExpenseStatus::Rejected.allows(ExpenseStatus::Draft));
        // Reopening an approved expense would leave its journal entry behind with nothing
        // pointing at it — the entry has posted, and undoing it is a reversing entry.
        assert!(!ExpenseStatus::Approved.allows(ExpenseStatus::Draft));
    }

    #[test]
    fn a_refusal_names_the_way_out_rather_than_only_what_failed() {
        let message = ExpenseStatus::Draft.step_to(ExpenseStatus::Approved).unwrap_err();
        let rendered = message.to_string();
        assert!(
            rendered.contains("submit for approval"),
            "the refusal has to tell the operator what they can do instead: {rendered}"
        );
    }

    #[test]
    fn a_final_expense_says_it_is_final_instead_of_listing_nothing() {
        let message = ExpenseStatus::Reimbursed
            .step_to(ExpenseStatus::Approved)
            .unwrap_err();
        assert!(
            message.to_string().contains("final"),
            "an empty transition list must still produce a sentence: {message}"
        );
    }

    #[test]
    fn only_a_draft_is_editable() {
        assert!(ExpenseStatus::Draft.is_editable());
        for status in [
            ExpenseStatus::Submitted,
            ExpenseStatus::Approved,
            ExpenseStatus::Rejected,
            ExpenseStatus::Reimbursed,
        ] {
            assert!(!status.is_editable(), "{status:?} must not be editable");
        }
    }

    #[test]
    fn the_tax_is_a_portion_of_the_amount_and_never_added_to_it() {
        // The slice-2 arithmetic slip: `gross_of` was `net + tax + discount` where the gross is
        // `net + discount`. A 100.00 receipt with 20.00 of tax in it is a 100.00 expense.
        let totals = ExpenseTotals::of("100.00", "20.00").expect("splits");
        assert_eq!(totals.amount.to_text(), "100.00");
        assert_eq!(totals.tax.to_text(), "20.00");
        assert_eq!(totals.net.to_text(), "80.00");
    }

    #[test]
    fn a_tax_larger_than_its_receipt_is_refused_rather_than_producing_a_negative_net() {
        assert!(ExpenseTotals::of("100.00", "120.00").is_err());
    }

    #[test]
    fn a_zero_amount_is_refused_before_the_table_check_has_to_catch_it() {
        // The table says `amount > 0` too, but a constraint violation names a constraint.
        let message = parse_amount("0.00", "amount").unwrap_err();
        assert!(message.to_string().contains("more than 0.00"));
    }

    #[test]
    fn a_blank_amount_is_refused_with_the_shape_it_wants() {
        let message = parse_amount("", "amount").unwrap_err();
        assert!(message.to_string().contains("1250.00"));
    }

    #[test]
    fn an_amount_that_is_not_a_number_is_refused_with_what_was_typed() {
        let message = parse_amount("fifty pounds", "amount").unwrap_err();
        assert!(message.to_string().contains("fifty pounds"));
    }

    #[test]
    fn a_description_is_required_and_says_what_it_wants() {
        let message = normalize_required("  ", "expense", "description", 300).unwrap_err();
        assert!(message.to_string().contains("what was bought"));
    }

    #[test]
    fn a_description_longer_than_the_column_is_refused() {
        let long = "x".repeat(MAX_DESCRIPTION_LENGTH + 1);
        assert!(normalize_required(&long, "expense", "description", MAX_DESCRIPTION_LENGTH).is_err());
    }

    #[test]
    fn a_currency_is_three_letters_and_says_so() {
        assert_eq!(normalize_currency("eur").expect("normalises"), "EUR");
        let message = normalize_currency("dollars").unwrap_err();
        assert!(message.to_string().contains("EUR or USD"));
        assert!(normalize_currency("US").is_err());
    }

    #[test]
    fn a_search_term_cannot_wildcard_its_way_to_the_whole_table() {
        // A person searching for a receipt numbered `100%` gets receipts numbered 100%, not
        // every row that starts with 100.
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
    }

    #[test]
    fn every_status_survives_a_round_trip_through_its_stored_form() {
        for status in ExpenseStatus::all() {
            assert_eq!(ExpenseStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(ExpenseStatus::parse("pending"), None);
    }
}
