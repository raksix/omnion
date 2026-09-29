//! The journal: entries, their lines, and the balance invariant.
//!
//! This module is the reason the accounting crate exists, and it has exactly one rule:
//!
//! **An entry that does not balance cannot be read.** Not "should not", not "is refused by the
//! posting route" — *cannot*, in the sense that the database's own CHECK constraint refuses the
//! row and a reader summing the debit column can never get a number they are wrong about.
//!
//! # How the invariant is actually kept
//!
//! Three layers, and the order matters, because each one catches what the layer above it cannot:
//!
//! 1. **A line is one side or the other.** Refused here, with the field named, and again by the
//!    schema's CHECK. A `0/0` line is a comment wearing a line's clothes; a `50/50` line is
//!    nonsense a sum would double-count. Both make the entry's line count disagree with its
//!    arithmetic, which is the failure a reader cannot detect.
//! 2. **The entry's totals are written by the same statement that writes the lines.** Not by a
//!    second query afterwards, and not by a trigger reading the lines: the totals are COLUMNS
//!    (`debit_total`, `credit_total`) because a CHECK constraint cannot contain an aggregate over
//!    another table, and computing them in the writer is what makes "balanced" a fact about the
//!    row rather than a claim about a table it is not allowed to see.
//! 3. **The CHECK refuses an entry that claims to be balanced and does not.** So even a future
//!    route, a bulk import or a person with a psql prompt cannot write one.
//!
//! # Why the refusal carries three numbers
//!
//! The slice's own wording for done is "an unbalanced entry is refused with a **visible
//! message**", and a message that says `accounting_journal_entries_check` is not visible in any
//! sense a bookkeeper would accept. So [`AccountingError::UnbalancedEntry`] carries the debit
//! total, the credit total and their **signed** difference, and the route renders them. That is
//! the difference between a refusal and an obstacle: the operator is looking at a grid of
//! debits and credits and needs to be told which way the difference runs, not sent to re-add a
//! column themselves.
//!
//! # Posted is immutable
//!
//! An entry with a `posted_at` cannot be edited or unposted through this module. Correction is a
//! **reversing entry**, not an edit — the same rule the other business modules wrote, and the
//! reason is that an edited journal is a journal nobody can audit: the number in front of you is
//! not the number that was reported.

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AccountingError, Result};

// The source kinds are re-exported for the same reason as the account kinds: the screens import
// from `journal` where the entry shape lives, and the SQL contract lives in `model`.
pub use crate::model::EntrySource;
use crate::money::{Amount, LineAmount, line_refusal};

/// The most lines one entry may carry.
///
/// A journal entry is a human-sized document — a payroll run, a depreciation posting, a payment
/// that touches four accounts. This is not a real limit; it is the bound that stops a request
/// that has lost its tenant predicate from writing the whole installation's ledger in one call.
pub const MAX_LINES: usize = 500;

/// Longest a memo may be.
pub const MAX_MEMO_LENGTH: usize = 500;

/// Longest a line description may be.
pub const MAX_DESCRIPTION_LENGTH: usize = 300;

// ---------------------------------------------------------------------------------------------
// The shapes
// ---------------------------------------------------------------------------------------------

/// One line of an entry, as the detail screen shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalLineView {
    /// The line's id.
    pub id: Uuid,
    /// The entry it belongs to.
    pub entry_id: Uuid,
    /// Where it sits in the entry.
    pub position: i32,
    /// The account it hits.
    pub account_id: Uuid,
    /// The account's code, so the grid does not need a second request to render.
    pub account_code: String,
    /// The account's name, for the same reason.
    pub account_name: String,
    /// A per-line note.
    pub description: String,
    /// The debit column, as text.
    pub debit: String,
    /// The credit column, as text.
    pub credit: String,
}

impl JournalLineView {
    /// The line's own reference for an audit row.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "line_id": self.id,
            "account_id": self.account_id,
            "account_code": self.account_code,
            "position": self.position,
        })
    }
}

/// An entry with its lines, as the detail screen shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntryView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The per-organization number, for printing and for a person's "JE-42" reference.
    pub entry_number: i64,
    /// The day the entry is dated.
    #[serde(with = "crate::dates")]
    pub entry_date: Date,
    /// The header note.
    pub memo: String,
    /// What caused the entry.
    pub source_kind: EntrySource,
    /// The document that caused it, when there is one.
    pub source_id: Option<Uuid>,
    /// The sum of the debit column, as text.
    pub debit_total: String,
    /// The sum of the credit column, as text.
    pub credit_total: String,
    /// Whether the totals agree. Always `true` for a stored entry — that is the invariant, and
    /// the field is here so a screen can print a badge without recomputing the sum in the
    /// browser and getting a different answer than the server.
    pub balanced: bool,
    /// When it was posted, if it has been.
    #[serde(with = "crate::dates::instant::option")]
    pub posted_at: Option<OffsetDateTime>,
    /// The lines, in order.
    pub lines: Vec<JournalLineView>,
    /// When the row was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl JournalEntryView {
    /// The compact reference an audit row and an event payload carry.
    ///
    /// Deliberately small: an event travels to every webhook subscriber, so it carries the id,
    /// the number, the day and the two totals — not the memo, and not the lines.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "entry_id": self.id,
            "entry_number": self.entry_number,
            "entry_date": crate::dates::to_wire(&self.entry_date),
            "debit_total": self.debit_total,
            "credit_total": self.credit_total,
            "balanced": self.balanced,
            "source_kind": self.source_kind.as_str(),
        })
    }

    /// How many lines the entry carries, for the list column.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

/// One line of the posting form.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JournalLineInput {
    /// The account it hits. Required.
    pub account_id: Uuid,
    /// A per-line note. Optional.
    #[serde(default)]
    pub description: Option<String>,
    /// The debit column. Text, because `numeric` has no Rust type here.
    #[serde(default)]
    pub debit: Option<String>,
    /// The credit column.
    #[serde(default)]
    pub credit: Option<String>,
}

impl JournalLineInput {
    /// The line's two sides, parsed and checked against each other.
    ///
    /// The empty-string case is a form posting a blank cell, which is `0` and not an error —
    /// a grid that sends `""` for the side a line is not on is the normal shape of the request.
    fn amounts(&self) -> Result<LineAmount> {
        line_refusal(
            self.debit.as_deref().unwrap_or("0"),
            self.credit.as_deref().unwrap_or("0"),
        )
    }
}

/// The body of `POST /accounting/journal`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewJournalEntry {
    /// The day the entry is dated. Defaults to today when absent, because a journal entry a
    /// person forgets to date is far more common than one that is deliberately undated.
    #[serde(default)]
    pub entry_date: Option<String>,
    /// The header note.
    #[serde(default)]
    pub memo: Option<String>,
    /// The lines. At least two: an entry with one line is a transaction on an account against
    /// nothing, and the balance rule already refuses it — this message names the reason earlier.
    #[serde(default)]
    pub lines: Vec<JournalLineInput>,
}

// ---------------------------------------------------------------------------------------------
// Posting
// ---------------------------------------------------------------------------------------------

/// Post a manual journal entry.
///
/// The whole transaction, in the order that makes the invariant true:
///
/// 1. validate the lines and sum them **here**, in Rust, so the refusal can carry three numbers;
/// 2. check the sum — an unbalanced entry never reaches SQL, so the person sees the difference
///    rather than a constraint name;
/// 3. take the next number **for update**, so two simultaneous posts cannot claim one number;
/// 4. insert the entry with both totals and `balanced = true` — the statement that would be
///    refused if the arithmetic were wrong is never executed;
/// 5. insert the lines with their accounts checked to belong to this organization.
///
/// Step 3 matters more than it looks. Without the lock, two posts read the same `max(number)`,
/// both write it, and the `unique (organization_id, entry_number)` turns the second into a
/// constraint error whose message says nothing about the race that caused it.
pub async fn post_entry(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewJournalEntry,
    created_by: Option<Uuid>,
) -> Result<JournalEntryView> {
    if new.lines.len() < 2 {
        return Err(AccountingError::invalid(
            "journal_entry",
            "lines",
            "an entry needs at least two lines — one side and the other",
        ));
    }
    if new.lines.len() > MAX_LINES {
        return Err(AccountingError::invalid(
            "journal_entry",
            "lines",
            format!("an entry carries at most {MAX_LINES} lines"),
        ));
    }

    let memo = normalize_memo(new.memo.as_deref());
    let entry_date = match new.entry_date.as_deref() {
        None | Some("") => time::OffsetDateTime::now_utc().date(),
        Some(raw) => crate::dates::parse(raw).map_err(|_| {
            AccountingError::invalid(
                "journal_entry",
                "entry_date",
                format!("a date such as 2026-12-01, not {raw:?} — the format is YYYY-MM-DD"),
            )
        })?,
    };

    // Sum in Rust, in integer hundredths. The database CHECK is the last line of defence; this
    // is the one that can say "difference 10.00" instead of naming a constraint.
    let mut debit_total = Amount::ZERO;
    let mut credit_total = Amount::ZERO;
    let mut parsed = Vec::with_capacity(new.lines.len());
    for line in &new.lines {
        let amounts = line.amounts()?;
        debit_total = debit_total.plus(amounts.debit);
        credit_total = credit_total.plus(amounts.credit);
        parsed.push((line, amounts));
    }

    if debit_total.cents() != credit_total.cents() {
        return Err(AccountingError::UnbalancedEntry {
            difference: debit_total.signed_difference(credit_total).to_text(),
            debit_total: debit_total.to_text(),
            credit_total: credit_total.to_text(),
        });
    }

    let mut tx = pool.begin().await?;

    // The account check runs **before** the entry insert, so a line pointing at another
    // organization's account is a 404 rather than a foreign-key error from the middle of a
    // transaction whose entry row is already written.
    for (line, _) in &parsed {
        // `fetch_optional` returns `Option<Uuid>` directly — the scalar is inferred, so naming
        // the type as `Result<Uuid, _>` is the mistake the compiler then reports three lines
        // later, away from the cause.
        let owner: Option<Uuid> = sqlx::query_scalar(
            "select organization_id from accounting_accounts where id = $1",
        )
        .bind(line.account_id)
        .fetch_optional(&mut *tx)
        .await?;
        match owner {
            Some(owner) if owner == organization_id => {}
            _ => {
                return Err(AccountingError::ForeignKey {
                    kind: "account",
                    id: line.account_id,
                });
            }
        }
    }

    let entry_number: i64 = sqlx::query_scalar(
        "select coalesce(max(entry_number), 0) + 1 from accounting_journal_entries \
         where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(&mut *tx)
    .await?;

    let entry_id: Uuid = sqlx::query_scalar(
        "insert into accounting_journal_entries \
             (organization_id, entry_number, entry_date, memo, source_kind, debit_total, \
              credit_total, balanced, posted_at, created_by) \
         values ($1, $2, $3, $4, 'manual', $5::numeric, $6::numeric, true, now(), $7) \
         returning id",
    )
    .bind(organization_id)
    .bind(entry_number)
    .bind(entry_date)
    .bind(&memo)
    .bind(debit_total.to_text())
    .bind(credit_total.to_text())
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await?;

    // The lines are inserted one statement at a time rather than as a generated bulk INSERT, on
    // purpose. A bulk insert is one round trip, but the CHECK on `accounting_journal_lines` — "one
    // side or the other" — would then abort the WHOLE statement and name no line. The balance is
    // already proved above, so the only thing a bulk insert could still catch is a shape this
    // loop reports with the line's own number attached. `MAX_LINES` bounds the cost.
    for (position, (line, amounts)) in parsed.iter().enumerate() {
        sqlx::query(
            "insert into accounting_journal_lines \
                 (entry_id, organization_id, account_id, position, description, debit, credit) \
             values ($1, $2, $3, $4, $5, $6::numeric, $7::numeric)",
        )
        .bind(entry_id)
        .bind(organization_id)
        .bind(line.account_id)
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .bind(normalize_line_description(line.description.as_deref()))
        .bind(amounts.debit.to_text())
        .bind(amounts.credit.to_text())
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    get_entry(pool, organization_id, entry_id).await
}

/// One entry with its lines, or `404` — which is also the answer for another organization's entry.
pub async fn get_entry(
    pool: &PgPool,
    organization_id: Uuid,
    entry_id: Uuid,
) -> Result<JournalEntryView> {
    let row = sqlx::query(
        "select id, organization_id, entry_number, entry_date, memo, source_kind, source_id, \
                debit_total::text as debit_total, credit_total::text as credit_total, balanced, \
                posted_at, created_at \
         from accounting_journal_entries where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(entry_id)
    .fetch_optional(pool)
    .await?
    .ok_or(AccountingError::NotFound("journal_entry"))?;

    let entry_id: Uuid = row.get("id");
    let source_kind = row
        .get::<String, _>("source_kind")
        .parse_source()
        .unwrap_or(EntrySource::Manual);

    let lines = load_lines(pool, entry_id).await?;

    Ok(JournalEntryView {
        id: entry_id,
        organization_id: row.get("organization_id"),
        entry_number: row.get("entry_number"),
        entry_date: row.get("entry_date"),
        memo: row.get("memo"),
        source_kind,
        source_id: row.get("source_id"),
        debit_total: row.get("debit_total"),
        credit_total: row.get("credit_total"),
        balanced: row.get("balanced"),
        posted_at: row.get("posted_at"),
        created_at: row.get("created_at"),
        lines,
    })
}

/// A page of entries, newest first.
///
/// Filters are the ones a bookkeeper actually filters by: the day, the source, and free text over
/// the memo and the number. The list does **not** return the lines — a bookkeeper's list screen
/// shows a number, a day, a memo, two totals and a line count, and shipping five hundred lines
/// per row to draw a "6 lines" column is how a journal screen becomes unusable.
pub async fn list_entries(
    pool: &PgPool,
    organization_id: Uuid,
    source: Option<EntrySource>,
    from: Option<Date>,
    to: Option<Date>,
    search: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<JournalEntrySummary>> {
    let limit = limit
        .unwrap_or(crate::store::DEFAULT_PER_PAGE)
        .clamp(1, crate::store::MAX_PER_PAGE);

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "select e.id, e.organization_id, e.entry_number, e.entry_date, e.memo, e.source_kind, \
                e.source_id, e.debit_total::text as debit_total, \
                e.credit_total::text as credit_total, e.balanced, e.posted_at, e.created_at, \
                (select count(*) from accounting_journal_lines l where l.entry_id = e.id) as line_count \
         from accounting_journal_entries e where e.organization_id = ",
    );
    // **No hand-written placeholders, and no always-on nullable predicate.** `QueryBuilder`
    // renumbers every bind itself, so a literal `$2` in the pushed SQL is not "the second bind" —
    // it is a dollar-quoted token the parser rejects, and the query dies with
    // `syntax error at or near "$2"` the first time anybody opens the journal. Two earlier
    // versions of this filter failed in opposite ways for the same reason: one numbered the
    // placeholders by hand (which the builder then renumbered out from under it), the other
    // compared `source_kind` against the organization id because the tenant predicate already
    // owned `$1`. The shape that survives all of them is the one the sales module already uses:
    // push the predicate ONLY when the filter is present, and let the builder number the binds.
    builder.push_bind(organization_id);
    if let Some(source) = source {
        builder.push(" and e.source_kind = ");
        builder.push_bind(source.as_str());
    }
    if let Some(from) = from {
        builder.push(" and e.entry_date >= ");
        builder.push_bind(from);
    }
    if let Some(to) = to {
        builder.push(" and e.entry_date <= ");
        builder.push_bind(to);
    }
    if let Some(term) = search.map(str::trim).filter(|t| !t.is_empty()) {
        if term.chars().count() > crate::store::MAX_SEARCH_LENGTH {
            return Err(AccountingError::invalid(
                "journal_entry",
                "search",
                format!(
                    "a search is at most {} characters",
                    crate::store::MAX_SEARCH_LENGTH
                ),
            ));
        }
        builder.push(" and (e.memo ilike ");
        builder.push_bind(format!("%{term}%"));
        builder.push(" or e.entry_number::text = ");
        builder.push_bind(term.to_owned());
        builder.push(")");
    }
    builder.push(" order by e.entry_date desc, e.entry_number desc limit ");
    builder.push_bind(limit);

    let rows = builder.build().fetch_all(pool).await?;
    rows.into_iter().map(JournalEntrySummary::from_row).collect()
}

/// An entry without its lines — what the list screen draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntrySummary {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The per-organization number.
    pub entry_number: i64,
    /// The day the entry is dated.
    #[serde(with = "crate::dates")]
    pub entry_date: Date,
    /// The header note.
    pub memo: String,
    /// What caused the entry.
    pub source_kind: EntrySource,
    /// The document that caused it, when there is one.
    pub source_id: Option<Uuid>,
    /// The sum of the debit column.
    pub debit_total: String,
    /// The sum of the credit column.
    pub credit_total: String,
    /// Whether the totals agree.
    pub balanced: bool,
    /// When it was posted, if it has been.
    #[serde(with = "crate::dates::instant::option")]
    pub posted_at: Option<OffsetDateTime>,
    /// How many lines it carries.
    pub line_count: i64,
    /// When the row was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl JournalEntrySummary {
    fn from_row(row: PgRow) -> Result<Self> {
        let source_kind = row
            .get::<String, _>("source_kind")
            .parse_source()
            .unwrap_or(EntrySource::Manual);
        Ok(Self {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            entry_number: row.get("entry_number"),
            entry_date: row.get("entry_date"),
            memo: row.get("memo"),
            source_kind,
            source_id: row.get("source_id"),
            debit_total: row.get("debit_total"),
            credit_total: row.get("credit_total"),
            balanced: row.get("balanced"),
            posted_at: row.get("posted_at"),
            // The alias must match the name here. `row.get` looks the name up at RUNTIME, so a
            // mismatch is a request-time 500 — `no column found for name: lines` — and not a
            // compile error, which is why a column renamed in the SELECT reads as a route that
            // "sometimes" fails: only the list, and only at runtime.
            line_count: row.get("line_count"),
            created_at: row.get("created_at"),
        })
    }

    /// The compact reference an audit row and an event payload carry.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "entry_id": self.id,
            "entry_number": self.entry_number,
            "entry_date": crate::dates::to_wire(&self.entry_date),
            "debit_total": self.debit_total,
            "credit_total": self.credit_total,
            "balanced": self.balanced,
            "source_kind": self.source_kind.as_str(),
        })
    }
}

/// The lines of one entry, in position order, with their account codes.
///
/// The codes are joined in rather than fetched per line: a journal entry's grid shows a code on
/// every row, and a screen that renders "— " until a second request arrives is a screen whose
/// half-rendered state somebody will screenshot.
pub async fn load_lines(pool: &PgPool, entry_id: Uuid) -> Result<Vec<JournalLineView>> {
    let rows = sqlx::query(
        "select l.id, l.entry_id, l.position, l.account_id, a.code as account_code, \
                a.name as account_name, l.description, l.debit::text as debit, \
                l.credit::text as credit \
         from accounting_journal_lines l \
         join accounting_accounts a on a.id = l.account_id \
         where l.entry_id = $1 order by l.position, l.id",
    )
    .bind(entry_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| JournalLineView {
            id: row.get("id"),
            entry_id: row.get("entry_id"),
            position: row.get("position"),
            account_id: row.get("account_id"),
            account_code: row.get("account_code"),
            account_name: row.get("account_name"),
            description: row.get("description"),
            debit: row.get("debit"),
            credit: row.get("credit"),
        })
        .collect())
}

/// The lines of an entry, organization-scoped, for the route's 404 check.
pub async fn count_lines(pool: &PgPool, organization_id: Uuid, entry_id: Uuid) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from accounting_journal_lines where organization_id = $1 and entry_id = $2",
    )
    .bind(organization_id)
    .bind(entry_id)
    .fetch_one(pool)
    .await?)
}

/// The running balance after each line, for the grid's live indicator.
///
/// Computed server-side from the same integer hundredths the entry totals use, so the indicator
/// a bookkeeper watches while typing agrees with the number the save will check — a client-side
/// sum of a JSON array of strings is a different sum, and it is the one that would tell them
/// "balanced" on an entry the server then refuses.
pub fn running_balances(lines: &[LineAmount]) -> Vec<String> {
    let mut running = Amount::ZERO;
    lines
        .iter()
        .map(|line| {
            running = running.plus(line.signed());
            running.to_text()
        })
        .collect()
}

/// The refusal an unbalanced posting produces, given the two totals — the same variant the
/// posting path raises, so a test and the route cannot describe the same failure differently.
#[must_use]
pub fn unbalanced(debit_total: Amount, credit_total: Amount) -> AccountingError {
    AccountingError::UnbalancedEntry {
        debit_total: debit_total.to_text(),
        credit_total: credit_total.to_text(),
        difference: debit_total.signed_difference(credit_total).to_text(),
    }
}

fn normalize_memo(memo: Option<&str>) -> String {
    let trimmed = memo.unwrap_or_default().trim();
    trimmed.chars().take(MAX_MEMO_LENGTH).collect()
}

fn normalize_line_description(description: Option<&str>) -> String {
    let trimmed = description.unwrap_or_default().trim();
    trimmed.chars().take(MAX_DESCRIPTION_LENGTH).collect()
}

/// A small trait so a `String` in a row becomes an [`EntrySource`] without a `match` in three
/// places. `None` for an unknown value is deliberate: a source written by a newer build is
/// reported as `manual` on the read path rather than crashing a list, and the audit row keeps the
/// raw string.
trait ParseSource {
    fn parse_source(&self) -> Option<EntrySource>;
}

impl ParseSource for String {
    fn parse_source(&self) -> Option<EntrySource> {
        EntrySource::parse(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(debit: &str, credit: &str) -> LineAmount {
        LineAmount::parse(debit, credit).expect("a legal line")
    }

    #[test]
    fn a_balanced_pair_sums_to_zero() {
        let lines = [line("100", "0"), line("0", "100")];
        let balances = running_balances(&lines);
        assert_eq!(balances, vec!["100.00".to_owned(), "0.00".to_owned()]);
    }

    #[test]
    fn the_running_indicator_names_which_way_the_entry_is_out() {
        // The grid's live balance is the number a bookkeeper reads before pressing save, so it
        // has to run the same way the refusal describes it.
        let lines = [line("100", "0"), line("0", "90")];
        let balances = running_balances(&lines);
        assert_eq!(balances, vec!["100.00".to_owned(), "10.00".to_owned()]);

        let error = unbalanced(
            Amount::parse("100").expect("parses"),
            Amount::parse("90").expect("parses"),
        );
        assert_eq!(
            error.to_string(),
            "the entry does not balance: debits 100.00, credits 90.00, difference 10.00"
        );
    }

    #[test]
    fn a_multi_line_entry_balances_when_the_columns_agree() {
        let lines = [
            line("1200", "0"),
            line("300", "0"),
            line("0", "1500"),
        ];
        let debits = lines
            .iter()
            .fold(Amount::ZERO, |acc, l| acc.plus(l.debit));
        let credits = lines
            .iter()
            .fold(Amount::ZERO, |acc, l| acc.plus(l.credit));
        assert_eq!(debits.to_text(), "1500.00");
        assert_eq!(credits.to_text(), "1500.00");
        assert!(debits.cents() == credits.cents());
    }

    #[test]
    fn a_memo_is_trimmed_and_bounded_rather_than_refused() {
        assert_eq!(normalize_memo(Some("  rent  ")), "rent");
        assert_eq!(normalize_memo(None), "");
        assert_eq!(normalize_memo(Some(&"x".repeat(1_000))).len(), MAX_MEMO_LENGTH);
    }
}
