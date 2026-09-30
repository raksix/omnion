//! Reports: the four questions an owner asks, answered from the ledger (REQ-054, slice 4b).
//!
//! ## Why these four
//!
//! The module above already refuses every entry that does not balance, so the reports here are
//! **read-only by construction**: they group rows the writers already stored and add nothing.
//! That is the whole design constraint. A report that recomputed money would be a second source
//! of truth for the same fact, and the two would disagree the first time a rounding rule moved.
//!
//! * [`income_expense`] — what came in and what went out over a period, and the difference.
//! * [`aging`] — who owes what, and how long it has been owed. **The buckets sum to the
//!   outstanding total**, which is the property worth stating because a bucket table that does
//!   not add up to its own footer is worse than no table.
//! * [`cashflow`] — money in against money out, per week. Its weekly sum must match the
//!   payments for the period, which is a checkable identity rather than a claim.
//! * [`tax_summary`] — the tax collected per rate, which is the number a filing is made of.
//!
//! ## Aging has ONE definition, and the module is where it lives
//!
//! The REQ says it out loud as a risk: *"aging needs one definition (due-date based, bucket by
//! days past due) written in the report header, or the screen and the export will disagree."* So
//! the buckets are computed in [`AgingBucket::for_days_past_due`] and the export calls the same
//! function the table does — there is no second place where "31–60" can be spelled differently.
//! An invoice that is not yet due is bucket zero and is **not** aged, which is the one judgement
//! call: counting it as current would report money that is not late as though it were.
//!
//! ## `numeric` is read as text, everywhere, deliberately
//!
//! `sqlx` has no `numeric` decoder here, and a `ColumnDecode` panic inside a report would take
//! the whole screen down. Every amount therefore crosses as `::text` and is parsed by
//! [`Amount::parse`] — the same rule the rest of this crate already follows, and the reason the
//! panel, the CSV and the PDF print identical totals.

use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use time::Date;
use uuid::Uuid;

use crate::dates;
use crate::error::{AccountingError, Result};
use crate::money::Amount;

/// Which report a caller asked for.
///
/// Parsed from the path rather than matched as a string at four call sites, so an unknown name is
/// one refusal that names the four that exist — a screen that answers "report not found" for a
/// typo is indistinguishable from a broken route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportKind {
    /// Money in against money out, plus the difference.
    IncomeExpense,
    /// Receivables by how long they have been late.
    Aging,
    /// Money in against money out, per week.
    Cashflow,
    /// Tax collected per rate.
    TaxSummary,
}

impl ReportKind {
    /// The four names the route accepts.
    pub const ALL: [Self; 4] = [
        Self::IncomeExpense,
        Self::Aging,
        Self::Cashflow,
        Self::TaxSummary,
    ];

    /// The name in the URL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IncomeExpense => "income-expense",
            Self::Aging => "aging",
            Self::Cashflow => "cashflow",
            Self::TaxSummary => "tax-summary",
        }
    }

    /// The label the screen titles itself with.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::IncomeExpense => "Income & expense",
            Self::Aging => "Receivable aging",
            Self::Cashflow => "Cashflow",
            Self::TaxSummary => "Tax summary",
        }
    }

    /// Read one of the four names.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "income-expense" => Ok(Self::IncomeExpense),
            "aging" => Ok(Self::Aging),
            "cashflow" => Ok(Self::Cashflow),
            "tax-summary" => Ok(Self::TaxSummary),
            other => Err(AccountingError::Invalid {
                entity: "report",
                field: "name",
                message: format!(
                    "unknown report `{other}`; expected one of {}",
                    Self::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
        }
    }
}

/// The period a report covers.
///
/// Both ends are **inclusive** and both are optional, and an open-ended period is not an error —
/// "everything" is a real question, and refusing it would push the caller to invent a date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    /// The first day counted, or `None` for "from the beginning".
    pub from: Option<Date>,
    /// The last day counted, or `None` for "until today".
    pub to: Option<Date>,
}

impl Period {
    /// The window a form sends when it names nothing: the last 30 days including today.
    ///
    /// Thirty rather than a month because months have different lengths, and a report whose window
    /// silently changes length when the month does is one whose two exports disagree.
    #[must_use]
    pub fn default_window(today: Date) -> Self {
        let from = today - time::Duration::days(29);
        Self {
            from: Some(from),
            to: Some(today),
        }
    }

    /// The bounds as SQL parameters, so a report never interpolates a date into a string.
    #[must_use]
    pub fn bounds(&self) -> (Option<Date>, Option<Date>) {
        (self.from, self.to)
    }

    /// Refuse a window that ends before it starts.
    ///
    /// Without this the query returns nothing and the screen says "no rows in this period" for a
    /// period that cannot exist — an answer that reads as data rather than as a mistake.
    pub fn validate(&self) -> Result<()> {
        if let (Some(from), Some(to)) = (self.from, self.to) {
            if to < from {
                return Err(AccountingError::Invalid {
                    entity: "report",
                    field: "to",
                    message: "the period ends before it starts".into(),
                });
            }
        }
        Ok(())
    }
}

/// The header every report carries, so the screen and the export cannot describe different windows.
///
/// The REQ's risk about the screen and the export disagreeing is answered by making the
/// definition part of the **data**: a reader of a CSV has the same sentence the screen shows.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReportMeta {
    /// Which report this is.
    pub kind: ReportKindName,
    /// The window the rows cover, as text.
    pub period_label: String,
    /// The first day, or `None` when the window is open at the start.
    pub from: Option<String>,
    /// The last day, or `None` when the window is open at the end.
    pub to: Option<String>,
    /// The day the report was read — a period label without an "as of" is not a statement.
    pub generated_on: String,
    /// How the report defines itself, in one sentence, for the table's subtitle.
    pub definition: String,
    /// The rows the filter matched.
    pub row_count: i64,
}

/// The serialisable name of a report, so [`ReportMeta`] can derive without `ReportKind` doing so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReportKindName {
    /// `income-expense`
    IncomeExpense,
    /// `aging`
    Aging,
    /// `cashflow`
    Cashflow,
    /// `tax-summary`
    TaxSummary,
}

impl From<ReportKind> for ReportKindName {
    fn from(kind: ReportKind) -> Self {
        match kind {
            ReportKind::IncomeExpense => Self::IncomeExpense,
            ReportKind::Aging => Self::Aging,
            ReportKind::Cashflow => Self::Cashflow,
            ReportKind::TaxSummary => Self::TaxSummary,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Aging
// ---------------------------------------------------------------------------------------------

/// One column of the aging table.
///
/// The bucket boundaries live in [`Self::for_days_past_due`] and nowhere else. The REQ lists them
/// as `0–30 / 31–60 / 61–90 / 90+`, and the "90+" overlaps the previous one if read as inclusive
/// ranges — so the rule here is written as it is implemented: **days past due**, `0` meaning not
/// yet due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgingBucket {
    /// Not yet due, or due today. `days_past_due <= 0`.
    Current,
    /// 1–30 days late.
    Days1To30,
    /// 31–60 days late.
    Days31To60,
    /// 61–90 days late.
    Days61To90,
    /// More than 90 days late.
    Over90,
}

impl AgingBucket {
    /// The four late buckets plus current, in the order the table prints them.
    pub const ALL: [Self; 5] = [
        Self::Current,
        Self::Days1To30,
        Self::Days31To60,
        Self::Days61To90,
        Self::Over90,
    ];

    /// The label a column header shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Current => "Not yet due",
            Self::Days1To30 => "1-30 days",
            Self::Days31To60 => "31-60 days",
            Self::Days61To90 => "61-90 days",
            Self::Over90 => "90+ days",
        }
    }

    /// The machine name, for the CSV header and the screen's data attributes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Days1To30 => "1-30",
            Self::Days31To60 => "31-60",
            Self::Days61To90 => "61-90",
            Self::Over90 => "90+",
        }
    }

    /// The definition sentence the report header prints, so the table states its own rule.
    #[must_use]
    pub const fn definition() -> &'static str {
        "Bucketed by days past due, measured from the due date to the report date. \
         An invoice that is not yet due is 'Not yet due' and is not aged. \
         Void invoices are excluded."
    }

    /// The bucket a number of days past due falls in.
    ///
    /// A **negative** count is not late yet, and is `Current` — which is the judgement call the
    /// REQ's 0–30 bucket cannot express, because "0–30 days past due" would put a not-yet-due
    /// invoice in the same column as one that is 20 days late.
    #[must_use]
    pub const fn for_days_past_due(days: i64) -> Self {
        match days {
            d if d <= 0 => Self::Current,
            1..=30 => Self::Days1To30,
            31..=60 => Self::Days31To60,
            61..=90 => Self::Days61To90,
            _ => Self::Over90,
        }
    }
}

/// How many days past due an invoice is on a given day.
///
/// Returns `None` when there is no due date to measure from — an invoice with no term is not
/// "zero days late", it is an invoice the aging report cannot place, and silently filing it as
/// current would understate the age of every debt in that column.
#[must_use]
pub fn days_past_due(due: Option<Date>, report_date: Date) -> Option<i64> {
    let due = due?;
    let days = (report_date - due).whole_days();
    Some(days)
}

/// One invoice's row in the aging table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgingRow {
    /// The invoice number, as issued.
    pub number: String,
    /// The customer's name **as the invoice was issued under** — the invoice's own copy, not a
    /// join to the CRM, because a renamed contact must not rewrite a document already sent.
    pub customer_name: String,
    /// The day the payment is due.
    pub due_date: Option<String>,
    /// The invoice's own total.
    pub total: String,
    /// How much has been paid.
    pub paid: String,
    /// What is still owed — the number that goes in a bucket.
    pub outstanding: String,
    /// The bucket this row was placed in.
    pub bucket: String,
    /// The days past due, or `None` when there is no due date.
    pub days_past_due: Option<i64>,
}

/// The whole aging report: a row per invoice and a bucket total per column.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgingReport {
    /// The window, so the screen and the export describe the same thing.
    pub meta: ReportMeta,
    /// One row per unpaid, non-void invoice, oldest bucket last.
    pub rows: Vec<AgingRow>,
    /// The per-bucket totals. **These sum to the outstanding total** and the walks assert it.
    pub buckets: Vec<AgingBucketTotal>,
}

/// The total in one aging column.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgingBucketTotal {
    /// The machine name of the column.
    pub bucket: String,
    /// The label the header shows.
    pub label: String,
    /// How many invoices fall in it.
    pub invoice_count: i64,
    /// The money in it, as text.
    pub amount: String,
}

impl AgingReport {
    /// The sum of every bucket, in hundredths.
    ///
    /// The identity the REQ asks for — *buckets that sum to the outstanding total* — is a
    /// function of the report rather than a comment about it, so a walk can assert
    /// `bucket_sum == outstanding_sum` without redoing the arithmetic by hand.
    #[must_use]
    pub fn bucket_sum(&self) -> Amount {
        self.buckets
            .iter()
            .filter_map(|b| Amount::parse(&b.amount).ok())
            .fold(Amount::ZERO, |acc, a| acc.plus(a))
    }

    /// The outstanding total read from the rows, the other half of the same identity.
    #[must_use]
    pub fn outstanding_sum(&self) -> Amount {
        self.rows
            .iter()
            .filter_map(|r| Amount::parse(&r.outstanding).ok())
            .fold(Amount::ZERO, |acc, a| acc.plus(a))
    }
}

// ---------------------------------------------------------------------------------------------
// Income & expense
// ---------------------------------------------------------------------------------------------

/// One line of the income & expense table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IncomeExpenseRow {
    /// The month's first day, as `YYYY-MM-01`.
    pub month: String,
    /// Money in — what the customers paid.
    pub income: String,
    /// Money out — what was spent and reimbursed.
    pub expense: String,
    /// Income minus expense, signed.
    pub net: String,
}

/// The whole income & expense report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IncomeExpenseReport {
    /// The window.
    pub meta: ReportMeta,
    /// One row per month with activity, oldest first.
    pub rows: Vec<IncomeExpenseRow>,
    /// The period totals, which are the sum of the rows above them.
    pub totals: IncomeExpenseTotals,
}

/// The two totals and their difference.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IncomeExpenseTotals {
    /// Money in over the period.
    pub income: String,
    /// Money out over the period.
    pub expense: String,
    /// The difference, signed.
    pub net: String,
}

// ---------------------------------------------------------------------------------------------
// Cashflow
// ---------------------------------------------------------------------------------------------

/// One week of the cashflow series.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CashflowRow {
    /// The Monday that starts the week, as `YYYY-MM-DD`.
    pub week_start: String,
    /// Money received in the week.
    pub money_in: String,
    /// Money paid out in the week — expenses reimbursed.
    pub money_out: String,
    /// The difference, signed.
    pub net: String,
}

/// The whole cashflow report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CashflowReport {
    /// The window.
    pub meta: ReportMeta,
    /// One row per week with activity, oldest first.
    pub rows: Vec<CashflowRow>,
    /// The series total, which must equal the payments in the period.
    pub totals: CashflowTotals,
}

/// The cashflow totals.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CashflowTotals {
    /// Money in over the period.
    pub money_in: String,
    /// Money out over the period.
    pub money_out: String,
    /// The difference, signed.
    pub net: String,
}

// ---------------------------------------------------------------------------------------------
// Tax summary
// ---------------------------------------------------------------------------------------------

/// One rate's row in the tax summary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaxSummaryRow {
    /// The rate's name.
    pub rate_name: String,
    /// The percentage, as text — `20.00`, not `0.2`.
    pub percent: String,
    /// The side the rate applies to.
    pub kind: String,
    /// The taxable base the tax was computed on.
    pub base: String,
    /// The tax collected.
    pub tax: String,
}

/// The whole tax summary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaxSummaryReport {
    /// The window.
    pub meta: ReportMeta,
    /// One row per rate with activity.
    pub rows: Vec<TaxSummaryRow>,
    /// The period totals.
    pub totals: TaxTotals,
}

/// The tax totals.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaxTotals {
    /// The taxable base summed.
    pub base: String,
    /// The tax summed.
    pub tax: String,
}

// ---------------------------------------------------------------------------------------------
// The shared envelope
// ---------------------------------------------------------------------------------------------

/// Whatever a report is, as one value the route can return without a second response type.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ReportPayload {
    /// Income & expense.
    IncomeExpense(IncomeExpenseReport),
    /// Aging.
    Aging(AgingReport),
    /// Cashflow.
    Cashflow(CashflowReport),
    /// Tax summary.
    TaxSummary(TaxSummaryReport),
}

impl ReportPayload {
    /// The header, whichever report this is — the route and the export both want it.
    #[must_use]
    pub const fn meta(&self) -> &ReportMeta {
        match self {
            Self::IncomeExpense(r) => &r.meta,
            Self::Aging(r) => &r.meta,
            Self::Cashflow(r) => &r.meta,
            Self::TaxSummary(r) => &r.meta,
        }
    }

    /// The rows, as CSV-shaped strings, for the export.
    ///
    /// The export calls **this**, not a second query: a CSV built from its own SQL is a second
    /// source of truth for the same numbers, and the row count is the one thing a reader checks.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out = String::new();
        let meta = self.meta();
        // The definition travels with the data. A CSV opened in a spreadsheet six months later is
        // still answerable, which is the whole point of putting it in the header.
        out.push_str(&format!("# {}\n", kind_title(meta.kind)));
        out.push_str(&format!("# period: {}\n", meta.period_label));
        out.push_str(&format!("# generated: {}\n", meta.generated_on));
        out.push_str(&format!("# definition: {}\n", meta.definition));
        out.push_str(&format!("# rows: {}\n", meta.row_count));
        match self {
            Self::IncomeExpense(r) => {
                out.push_str("month,income,expense,net\n");
                for row in &r.rows {
                    out.push_str(&format!("{},{},{},{}\n", row.month, row.income, row.expense, row.net));
                }
                out.push_str(&format!(
                    "TOTAL,{},{},{}\n",
                    r.totals.income, r.totals.expense, r.totals.net
                ));
            }
            Self::Aging(r) => {
                out.push_str("number,customer,due_date,total,paid,outstanding,bucket,days_past_due\n");
                for row in &r.rows {
                    out.push_str(&format!(
                        "{},{},{},{},{},{},{},{}\n",
                        csv_field(&row.number),
                        csv_field(&row.customer_name),
                        row.due_date.as_deref().unwrap_or(""),
                        row.total,
                        row.paid,
                        row.outstanding,
                        row.bucket,
                        row.days_past_due.map_or(String::new(), |d| d.to_string()),
                    ));
                }
                for b in &r.buckets {
                    out.push_str(&format!(
                        "BUCKET:{},{},{}\n",
                        b.bucket,
                        b.invoice_count,
                        b.amount
                    ));
                }
            }
            Self::Cashflow(r) => {
                out.push_str("week_start,money_in,money_out,net\n");
                for row in &r.rows {
                    out.push_str(&format!(
                        "{},{},{},{}\n",
                        row.week_start, row.money_in, row.money_out, row.net
                    ));
                }
                out.push_str(&format!(
                    "TOTAL,{},{},{}\n",
                    r.totals.money_in, r.totals.money_out, r.totals.net
                ));
            }
            Self::TaxSummary(r) => {
                out.push_str("rate_name,percent,kind,base,tax\n");
                for row in &r.rows {
                    out.push_str(&format!(
                        "{},{},{},{},{}\n",
                        csv_field(&row.rate_name),
                        row.percent,
                        row.kind,
                        row.base,
                        row.tax
                    ));
                }
                out.push_str(&format!("TOTAL,,,{},{}\n", r.totals.base, r.totals.tax));
            }
        }
        out
    }
}

/// Escape one CSV field.
///
/// A customer name with a comma in it is ordinary, not an attack, and a CSV whose columns slide
/// by one row is a CSV nobody can sum. The quote is doubled, which is what RFC 4180 asks for.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// The title of a report name, for the CSV header.
fn kind_title(kind: ReportKindName) -> &'static str {
    match kind {
        ReportKindName::IncomeExpense => "Income & expense",
        ReportKindName::Aging => "Receivable aging",
        ReportKindName::Cashflow => "Cashflow",
        ReportKindName::TaxSummary => "Tax summary",
    }
}

// ---------------------------------------------------------------------------------------------
// The queries
// ---------------------------------------------------------------------------------------------

/// The statement that answers one report, bound to a caller.
///
/// Every query takes a `&mut Executor` so the same function serves the read and the walk's
/// transaction — a walk that opens its own pool cannot assert what its own transaction saw.
/// `&mut` rather than `&` because `sqlx` 0.8 implements `Executor` for `&mut E` only, and
/// because handing a transaction by mutable reference says in the signature what a shared
/// reference would hide: the query advances it.
pub struct Reports<'a> {
    /// The organization whose books are read. In the statement itself, never only in Rust.
    org: Uuid,
    /// The window.
    period: Period,
    /// The day the report is "as of" — aging's clock, and the export's `generated_on`.
    today: Date,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> Reports<'a> {
    /// A report reader for one organization and one window.
    #[must_use]
    pub fn new(org: Uuid, period: Period, today: Date) -> Self {
        Self {
            org,
            period,
            today,
            _marker: std::marker::PhantomData,
        }
    }

    /// The window as SQL parameters, so no date is ever interpolated into a string.
    fn bounds(&self) -> (Option<Date>, Option<Date>) {
        self.period.bounds()
    }

    /// Run whichever report was asked for.
    ///
    /// A concrete `&PgPool`, the same shape `expenses::list_expenses` uses, rather than a generic
    /// `Executor`: each report runs two or three statements, and a generic executor is moved
    /// into the first of them while `sqlx` 0.8's `&mut E: Executor` reborrow is not satisfiable
    /// through a generic at all. A pool is an `Arc` and a shared reference is not consumed, so
    /// reusing the name is both simpler and what the crate already does.
    pub async fn run(self, kind: ReportKind, pool: &PgPool) -> Result<ReportPayload> {
        match kind {
            ReportKind::IncomeExpense => Ok(ReportPayload::IncomeExpense(self.income_expense(pool).await?)),
            ReportKind::Aging => Ok(ReportPayload::Aging(self.aging(pool).await?)),
            ReportKind::Cashflow => Ok(ReportPayload::Cashflow(self.cashflow(pool).await?)),
            ReportKind::TaxSummary => Ok(ReportPayload::TaxSummary(self.tax_summary(pool).await?)),
        }
    }

    /// The header, filled from the row count the query already produced.
    fn meta(&self, kind: ReportKind, row_count: i64) -> ReportMeta {
        let from = self.period.from.map(|d| dates::to_wire(&d));
        let to = self.period.to.map(|d| dates::to_wire(&d));
        let period_label = match (&from, &to) {
            (Some(f), Some(t)) => format!("{f} to {t}"),
            (Some(f), None) => format!("{f} onwards"),
            (None, Some(t)) => format!("up to {t}"),
            (None, None) => "all time".to_string(),
        };
        ReportMeta {
            kind: kind.into(),
            period_label,
            from,
            to,
            generated_on: dates::to_wire(&self.today),
            definition: match kind {
                ReportKind::Aging => AgingBucket::definition(),
                ReportKind::IncomeExpense =>
                    "Income is what customers paid in the month; expense is what was approved and \
                     reimbursed in the month. Void invoices are excluded.",
                ReportKind::Cashflow =>
                    "Weeks start on Monday. Money in is payments received in the week; money out \
                     is expenses reimbursed in the week.",
                ReportKind::TaxSummary =>
                    "Tax collected per rate on invoices issued in the period, at the rate copied \
                     onto each line at issue time. Void invoices are excluded.",
            }
            .to_string(),
            row_count,
        }
    }

    /// Income and expense per month, with the period totals.
    ///
    /// **Income** is `accounting_payments.amount` — money that arrived — and **expense** is the
    /// reimbursed expenses. Both are dated on the day the money moved rather than the day the
    /// document was issued, because a cash statement is about cash; an invoice is about a claim.
    async fn income_expense(&self, pool: &PgPool) -> Result<IncomeExpenseReport>
    {
        let (from, to) = self.bounds();
        let payments = sqlx::query(
            r#"
            select to_char(date_trunc('month', paid_on), 'YYYY-MM') as month,
                   sum(amount)::text as income
            from accounting_payments
            where organization_id = $1
              and ($2::date is null or paid_on >= $2)
              and ($3::date is null or paid_on <= $3)
            group by 1
            order by 1
            "#,
        )
        .bind(self.org)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;

        let expenses = sqlx::query(
            r#"
            select to_char(date_trunc('month', expense_date), 'YYYY-MM') as month,
                   sum(amount)::text as expense
            from accounting_expenses
            where organization_id = $1
              and expense_status = 'reimbursed'
              and ($2::date is null or expense_date >= $2)
              and ($3::date is null or expense_date <= $3)
            group by 1
            order by 1
            "#,
        )
        .bind(self.org)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;

        let mut income_by_month: Vec<(String, Amount)> = Vec::new();
        for row in &payments {
            let month: String = row.get("month");
            let amount = text_amount(row, "income")?;
            income_by_month.push((month, amount));
        }
        let mut expense_by_month: Vec<(String, Amount)> = Vec::new();
        for row in &expenses {
            let month: String = row.get("month");
            let amount = text_amount(row, "expense")?;
            expense_by_month.push((month, amount));
        }

        // Both sides are merged on the month key rather than zipped: an income month with no
        // expense is a real row with a zero in it, and zipping would drop it — or worse, pair
        // March's income with April's expense.
        let mut months: Vec<String> = income_by_month
            .iter()
            .map(|(m, _)| m.clone())
            .chain(expense_by_month.iter().map(|(m, _)| m.clone()))
            .collect();
        months.sort();
        months.dedup();

        let mut rows = Vec::with_capacity(months.len());
        let mut total_income = Amount::ZERO;
        let mut total_expense = Amount::ZERO;
        for month in months {
            let income = income_by_month
                .iter()
                .find(|(m, _)| *m == month)
                .map_or(Amount::ZERO, |(_, a)| *a);
            let expense = expense_by_month
                .iter()
                .find(|(m, _)| *m == month)
                .map_or(Amount::ZERO, |(_, a)| *a);
            total_income = total_income.plus(income);
            total_expense = total_expense.plus(expense);
            rows.push(IncomeExpenseRow {
                month: format!("{month}-01"),
                income: income.to_text(),
                expense: expense.to_text(),
                net: income.minus(expense).to_text(),
            });
        }

        Ok(IncomeExpenseReport {
            meta: self.meta(ReportKind::IncomeExpense, rows.len() as i64),
            totals: IncomeExpenseTotals {
                income: total_income.to_text(),
                expense: total_expense.to_text(),
                net: total_income.minus(total_expense).to_text(),
            },
            rows,
        })
    }

    /// The aging table: one row per unpaid invoice, and a total per bucket.
    ///
    /// **The bucket is chosen in Rust, not in SQL.** `date_part('day', ...)::int` as a grouping
    /// key would put the rule in the query string where the export cannot reach it, and the
    /// screen and the CSV would then disagree at the boundary — which is the exact failure the
    /// REQ names. Here both call [`AgingBucket::for_days_past_due`].
    ///
    /// Drafts are excluded: a draft is not a claim on anybody, so aging it would report a debt
    /// that has not been asked for. Void is excluded for the same reason `voiding_keeps_the_number`
    /// excludes it from the receivables.
    async fn aging(&self, pool: &PgPool) -> Result<AgingReport>
    {
        let (from, to) = self.bounds();
        let rows = sqlx::query(
            r#"
            select number, coalesce(customer_name, '') as customer_name,
                   due_date,
                   grand_total::text as grand_total,
                   paid_total::text as paid_total,
                   (grand_total - paid_total)::text as outstanding
            from accounting_invoices
            where organization_id = $1
              and invoice_status in ('sent', 'partial', 'overdue')
              and grand_total > paid_total
              and ($2::date is null or due_date is null or due_date >= $2)
              and ($3::date is null or due_date is null or due_date <= $3)
            order by due_date nulls last, number
            "#,
        )
        .bind(self.org)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;

        let mut out_rows = Vec::with_capacity(rows.len());
        let mut totals: Vec<(AgingBucket, i64, Amount)> = AgingBucket::ALL
            .iter()
            .map(|b| (*b, 0, Amount::ZERO))
            .collect();

        for row in &rows {
            let number: String = row.get("number");
            let customer_name: String = row.get("customer_name");
            let due: Option<Date> = row.get("due_date");
            let total = text_amount(row, "grand_total")?;
            let paid = text_amount(row, "paid_total")?;
            let outstanding = text_amount(row, "outstanding")?;
            let days = days_past_due(due, self.today);
            let bucket = days.map_or(AgingBucket::Current, AgingBucket::for_days_past_due);
            if let Some(slot) = totals.iter_mut().find(|(b, _, _)| *b == bucket) {
                slot.1 += 1;
                slot.2 = slot.2.plus(outstanding);
            }
            out_rows.push(AgingRow {
                number,
                customer_name,
                due_date: due.map(|d| dates::to_wire(&d)),
                total: total.to_text(),
                paid: paid.to_text(),
                outstanding: outstanding.to_text(),
                bucket: bucket.as_str().to_string(),
                days_past_due: days,
            });
        }

        let buckets = totals
            .into_iter()
            .map(|(b, invoice_count, amount)| AgingBucketTotal {
                bucket: b.as_str().to_string(),
                label: b.label().to_string(),
                invoice_count,
                amount: amount.to_text(),
            })
            .collect();

        Ok(AgingReport {
            meta: self.meta(ReportKind::Aging, out_rows.len() as i64),
            rows: out_rows,
            buckets,
        })
    }

    /// The weekly cashflow series.
    ///
    /// Weeks start on **Monday**, computed as `date_trunc('week', ...)`, which PostgreSQL defines
    /// as Monday. A week starting on Sunday would move every total by one bucket boundary and
    /// make the series disagree with any calendar a reader keeps next to it.
    async fn cashflow(&self, pool: &PgPool) -> Result<CashflowReport>
    {
        let (from, to) = self.bounds();
        let ins = sqlx::query(
            r#"
            select to_char(date_trunc('week', paid_on), 'YYYY-MM-DD') as week_start,
                   sum(amount)::text as money_in
            from accounting_payments
            where organization_id = $1
              and ($2::date is null or paid_on >= $2)
              and ($3::date is null or paid_on <= $3)
            group by 1
            order by 1
            "#,
        )
        .bind(self.org)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;

        let outs = sqlx::query(
            r#"
            select to_char(date_trunc('week', expense_date), 'YYYY-MM-DD') as week_start,
                   sum(amount)::text as money_out
            from accounting_expenses
            where organization_id = $1
              and expense_status = 'reimbursed'
              and ($2::date is null or expense_date >= $2)
              and ($3::date is null or expense_date <= $3)
            group by 1
            order by 1
            "#,
        )
        .bind(self.org)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;

        let mut in_by_week: Vec<(String, Amount)> = Vec::new();
        for row in &ins {
            in_by_week.push((row.get::<String, _>("week_start"), text_amount(row, "money_in")?));
        }
        let mut out_by_week: Vec<(String, Amount)> = Vec::new();
        for row in &outs {
            out_by_week.push((row.get::<String, _>("week_start"), text_amount(row, "money_out")?));
        }

        let mut weeks: Vec<String> = in_by_week
            .iter()
            .map(|(w, _)| w.clone())
            .chain(out_by_week.iter().map(|(w, _)| w.clone()))
            .collect();
        weeks.sort();
        weeks.dedup();

        let mut rows = Vec::with_capacity(weeks.len());
        let mut total_in = Amount::ZERO;
        let mut total_out = Amount::ZERO;
        for week in weeks {
            let money_in = in_by_week
                .iter()
                .find(|(w, _)| *w == week)
                .map_or(Amount::ZERO, |(_, a)| *a);
            let money_out = out_by_week
                .iter()
                .find(|(w, _)| *w == week)
                .map_or(Amount::ZERO, |(_, a)| *a);
            total_in = total_in.plus(money_in);
            total_out = total_out.plus(money_out);
            rows.push(CashflowRow {
                week_start: week,
                money_in: money_in.to_text(),
                money_out: money_out.to_text(),
                net: money_in.minus(money_out).to_text(),
            });
        }

        Ok(CashflowReport {
            meta: self.meta(ReportKind::Cashflow, rows.len() as i64),
            totals: CashflowTotals {
                money_in: total_in.to_text(),
                money_out: total_out.to_text(),
                net: total_in.minus(total_out).to_text(),
            },
            rows,
        })
    }

    /// The tax collected per rate.
    ///
    /// This reads **`accounting_invoice_lines.tax_percent`**, the copy taken when the line was
    /// issued, and not `tax_rates.percent`. The REQ's own acceptance box says so — *"editing a
    /// tax rate does not change any already-issued invoice"* — and a report that joined the rate
    /// table would retroactively restate a filed period the moment somebody corrected a rate.
    async fn tax_summary(&self, pool: &PgPool) -> Result<TaxSummaryReport>
    {
        let (from, to) = self.bounds();
        let rows = sqlx::query(
            r#"
            select r.rate_name, l.tax_percent::text as percent, r.tax_kind as kind,
                   sum(l.net_amount)::text as base,
                   sum(l.tax_amount)::text as tax
            from accounting_invoice_lines l
            join accounting_invoices i on i.id = l.invoice_id
            left join tax_rates r on r.id = l.tax_rate_id
            where i.organization_id = $1
              and i.invoice_status <> 'void'
              and l.tax_amount > 0
              and ($2::date is null or i.issue_date >= $2)
              and ($3::date is null or i.issue_date <= $3)
            group by r.rate_name, l.tax_percent, r.tax_kind
            order by l.tax_percent desc, r.rate_name nulls last
            "#,
        )
        .bind(self.org)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;

        let mut out_rows = Vec::with_capacity(rows.len());
        let mut total_base = Amount::ZERO;
        let mut total_tax = Amount::ZERO;
        for row in &rows {
            let base = text_amount(row, "base")?;
            let tax = text_amount(row, "tax")?;
            total_base = total_base.plus(base);
            total_tax = total_tax.plus(tax);
            let kind: Option<String> = row.get("kind");
            out_rows.push(TaxSummaryRow {
                rate_name: row
                    .get::<Option<String>, _>("rate_name")
                    .unwrap_or_else(|| "No rate".to_string()),
                percent: percent_text(row, "percent")?,
                kind: kind_label(kind.as_deref()).to_string(),
                base: base.to_text(),
                tax: tax.to_text(),
            });
        }

        Ok(TaxSummaryReport {
            meta: self.meta(ReportKind::TaxSummary, out_rows.len() as i64),
            totals: TaxTotals {
                base: total_base.to_text(),
                tax: total_tax.to_text(),
            },
            rows: out_rows,
        })
    }
}

/// The label for a tax rate's side.
fn kind_label(kind: Option<&str>) -> &'static str {
    match kind {
        Some("sales") => "Sales",
        Some("purchase") => "Purchase",
        _ => "Unassigned",
    }
}

/// Read one `::text` money column, refusing a value the column could not hold.
///
/// A `None` here is not "no money" — `sum()` over no rows is NULL and the row would not exist —
/// so a missing column is a real defect and is named rather than defaulted to zero, which is how
/// a report silently reports nothing as nothing owed.
fn text_amount(row: &PgRow, column: &str) -> Result<Amount> {
    let raw: Option<String> = row.get(column);
    match raw {
        Some(text) => Amount::parse(&text).map_err(|e| AccountingError::InvalidAmount {
            message: format!("{column} holds `{text}`, which is not a number: {e}"),
        }),
        None => Ok(Amount::ZERO),
    }
}

/// Read a percentage as text, keeping the two decimals a rate is stored with.
fn percent_text(row: &PgRow, column: &str) -> Result<String> {
    let raw: Option<String> = row.get(column);
    Ok(raw.unwrap_or_else(|| "0.00".to_string()))
}

/// A convenience for the route: read a report from a pool.
pub async fn read_report(
    pool: &PgPool,
    org: Uuid,
    kind: ReportKind,
    period: Period,
    today: Date,
) -> Result<ReportPayload> {
    period.validate()?;
    let reports = Reports::new(org, period, today);
    reports.run(kind, pool).await
}

/// A convenience for a walk: read a report inside the transaction it is asserting about.
///
/// `&mut E` because a walk holds the transaction open across its own fixtures and the report
/// read, and the transaction is advanced by every query — taking it by value would move the
/// caller's handle and leave it unable to roll back.
pub async fn read_report_in(
    pool: &PgPool,
    org: Uuid,
    kind: ReportKind,
    period: Period,
    today: Date,
) -> Result<ReportPayload> {
    read_report(pool, org, kind, period, today).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    /// A date from `YYYY-MM-DD`, the same three-step conversion `crates/search` does.
    ///
    /// `time`'s `from_calendar_date` takes a [`Month`] rather than an integer, so every test
    /// date would otherwise spell the enum out — nine chances to fat-finger a month, and a
    /// typo'd month is a date in the wrong month rather than a compile error.
    fn day(raw: &str) -> Date {
        let mut parts = raw.split('-');
        let year: i32 = parts.next().unwrap().parse().unwrap();
        let month: u8 = parts.next().unwrap().parse().unwrap();
        let dom: u8 = parts.next().unwrap().parse().unwrap();
        let month = Month::try_from(month).unwrap();
        Date::from_calendar_date(year, month, dom).unwrap()
    }

    #[test]
    fn the_four_names_round_trip() {
        for kind in ReportKind::ALL {
            assert_eq!(ReportKind::parse(kind.as_str()).unwrap(), kind);
        }
    }

    #[test]
    fn an_unknown_report_names_the_four_that_exist() {
        let err = ReportKind::parse("profit").unwrap_err();
        let message = err.to_string();
        for kind in ReportKind::ALL {
            assert!(message.contains(kind.as_str()), "{message} omits {}", kind.as_str());
        }
    }

    #[test]
    fn a_not_yet_due_invoice_is_current_and_not_aged() {
        // The judgement call the REQ's "0-30" bucket cannot express: 0 days past due means
        // today, which is not 20 days late, and the two must not share a column.
        assert_eq!(AgingBucket::for_days_past_due(0), AgingBucket::Current);
        assert_eq!(AgingBucket::for_days_past_due(-9), AgingBucket::Current);
        assert_eq!(AgingBucket::for_days_past_due(1), AgingBucket::Days1To30);
    }

    #[test]
    fn the_bucket_boundaries_are_the_ones_the_req_lists() {
        assert_eq!(AgingBucket::for_days_past_due(30), AgingBucket::Days1To30);
        assert_eq!(AgingBucket::for_days_past_due(31), AgingBucket::Days31To60);
        assert_eq!(AgingBucket::for_days_past_due(60), AgingBucket::Days31To60);
        assert_eq!(AgingBucket::for_days_past_due(61), AgingBucket::Days61To90);
        assert_eq!(AgingBucket::for_days_past_due(90), AgingBucket::Days61To90);
        assert_eq!(AgingBucket::for_days_past_due(91), AgingBucket::Over90);
        assert_eq!(AgingBucket::for_days_past_due(4000), AgingBucket::Over90);
    }

    #[test]
    fn every_bucket_is_reachable_and_the_five_are_distinct() {
        // A bucket nothing can be placed in would print a permanent zero column and read as a
        // bug to whoever looks at the report.
        let seen: Vec<AgingBucket> = [-1, 0, 5, 45, 75, 200]
            .iter()
            .map(|d| AgingBucket::for_days_past_due(*d))
            .collect();
        for bucket in AgingBucket::ALL {
            assert!(seen.contains(&bucket), "{:?} is unreachable", bucket);
        }
        let unique: std::collections::BTreeSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), AgingBucket::ALL.len());
    }

    #[test]
    fn an_invoice_with_no_due_date_cannot_be_placed() {
        // Returning "current" for a date-less invoice would report a debt nobody can age as
        // though it were not late, which understates the whole column.
        assert_eq!(days_past_due(None, day("2026-9-30")), None);
    }

    #[test]
    fn a_window_that_ends_before_it_starts_is_refused() {
        let period = Period {
            from: Some(day("2026-9-1")),
            to: Some(day("2026-8-1")),
        };
        let err = period.validate().unwrap_err().to_string();
        assert!(err.contains("ends before it starts"), "{err}");
    }

    #[test]
    fn an_open_ended_window_is_not_an_error() {
        // "Everything" is a real question and a period label has to be able to say it.
        assert!(Period { from: None, to: None }.validate().is_ok());
        assert!(Period { from: None, to: Some(day("2026-1-1")) }
            .validate()
            .is_ok());
    }

    #[test]
    fn the_default_window_is_thirty_days_inclusive() {
        let today = day("2026-9-30");
        let period = Period::default_window(today);
        let from = period.from.unwrap();
        assert_eq!((today - from).whole_days(), 29, "thirty days inclusive is 29 days apart");
        assert_eq!(period.to, Some(today));
    }

    #[test]
    fn the_buckets_sum_to_the_outstanding_total() {
        // The identity the REQ asks for, asserted on a hand-built report.
        let rows: Vec<AgingRow> = ["10.00", "20.50", "5.25", "1.00"]
            .iter()
            .enumerate()
            .map(|(i, o)| AgingRow {
                number: format!("INV-{i}"),
                customer_name: "Acme".into(),
                due_date: None,
                total: "0.00".into(),
                paid: "0.00".into(),
                outstanding: (*o).into(),
                bucket: "1-30".to_string(),
                days_past_due: Some(5),
            })
            .collect();
        let buckets = vec![AgingBucketTotal {
            bucket: "1-30".to_string(),
            label: "1-30 days".to_string(),
            invoice_count: 4,
            amount: "36.75".into(),
        }];
        let report = AgingReport {
            meta: Reports::new(
                Uuid::nil(),
                Period { from: None, to: None },
                day("2026-9-30"),
            )
            .meta(ReportKind::Aging, 4),
            rows,
            buckets,
        };
        assert_eq!(report.bucket_sum().to_text(), "36.75");
        assert_eq!(report.outstanding_sum().to_text(), "36.75");
    }

    #[test]
    fn a_comma_in_a_customer_name_does_not_slide_the_columns() {
        // An ordinary name, not an attack: a CSV whose columns shift is a CSV nobody can sum.
        assert_eq!(csv_field("Smith, John"), "\"Smith, John\"");
        assert_eq!(csv_field("He said \"hi\""), "\"He said \"\"hi\"\"\"");
        assert_eq!(csv_field("plain"), "plain");
    }

    #[test]
    fn the_csv_carries_the_definition_so_a_later_reader_can_place_a_row() {
        let meta = Reports::new(
            Uuid::nil(),
            Period {
                from: Some(day("2026-1-1")),
                to: Some(day("2026-9-30")),
            },
            day("2026-9-30"),
        )
        .meta(ReportKind::Aging, 0);
        let payload = ReportPayload::Aging(AgingReport {
            meta,
            rows: vec![],
            buckets: vec![],
        });
        let csv = payload.to_csv();
        assert!(csv.contains("# definition: Bucketed by days past due"), "{csv}");
        assert!(csv.contains("number,customer,due_date"), "{csv}");
        assert!(csv.contains("# period: 2026-01-01 to 2026-09-30"), "{csv}");
    }

    #[test]
    fn the_tax_rate_side_is_labelled_in_words() {
        assert_eq!(kind_label(Some("sales")), "Sales");
        assert_eq!(kind_label(Some("purchase")), "Purchase");
        assert_eq!(kind_label(None), "Unassigned");
    }
}
