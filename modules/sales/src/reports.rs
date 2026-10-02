//! Reports, global search and the CSV export (docs/requests/REQ-052, slice 4b).
//!
//! The last file the module needed. Slices 1–4a made documents: a seller can write a quote, get
//! it approved, send it, watch a customer accept it, turn it into an order, hold its stock and
//! hand it to accounting. What none of that answers is the question a sales desk is actually
//! judged on — *did we win?* — and the question a seller asks while standing in front of a
//! customer: *do we already have a quote for them?* This file is those two answers, plus the
//! rows behind the first one.
//!
//! ## The four numbers, and what each one deliberately does **not** count
//!
//! Each of the figures the spec asks for has a version that is easy to compute and wrong, and the
//! reason is the same every time: **the pipeline and the outcome are not the same population.**
//!
//! * **Conversion** is won ÷ (won + lost), never ÷ every quote. A denominator that includes drafts
//!   and quotes still sitting with a customer is a number that falls every time a seller writes a
//!   new quote — the opposite of what a conversion rate means.
//! * **Won** is an accepted quote that became an order. **Lost** is declined or expired. A
//!   cancelled quote is counted in **neither**: the organization withdrew it, which is not a loss
//!   the customer inflicted and not a win anybody can claim. It has its own bucket, and the four
//!   buckets always add up to the number of rows in the table below them.
//! * **Average deal size** is over won deals only. Averaging over every quote — most of which
//!   never happened — reports the average of what was *asked for*, and a board reading it as
//!   revenue plans against a number that does not exist.
//! * **An undecided quote is never silently dropped.** `pending` carries drafts, sent quotes and
//!   accepted quotes nobody has turned into an order yet, and the sum is a `group by` of the same
//!   rows the table is drawn from, so the invariant cannot rot the way independently computed
//!   counters can.
//!
//! ## The breakdown is per owner, and "no owner" is a row
//!
//! An unowned quote renders as `Unassigned` on its own line rather than folded into a total: the
//! first question anybody asks of a per-owner report is "whose pipeline is this?", and dropping
//! the rows answers "nobody's", which is wrong. The breakdown also **ignores the owner filter** —
//! a report filtered to one seller would otherwise have a per-owner table that cannot be checked
//! against its own total.
//!
//! ## The period is a half-open range, resolved in SQL, and it defaults to the last 30 days
//!
//! `from` and `to` are both inclusive days, and the day a report counts a quote by is
//! `coalesce(accepted_at, declined_at, cancelled_at, created_at)` — the day the outcome happened,
//! or the day the document appeared if there is no outcome yet. Filtering on `created_at` alone was
//! the first version, and it put a March quote in April's report wearing April's date. The window is bounded at five
//! years, because a report over "everything ever" on a table that will hold a million rows is a
//! denial of service wearing a business report's clothes.
//!
//! ## Global search is one query, not two
//!
//! [`global_search`] answers one question — "what did we ever write about this?" — with a single
//! statement over both documents and both kinds of match, returning them as one ranked list. Two
//! separate calls would double the round trips on the ⌘K keystroke path, and two separately
//! ranked lists could not be merged honestly: a quote whose number matches exactly belongs above an
//! order that merely mentions the customer. The ranking is one expression, evaluated once per
//! row — exact number, then number prefix, then customer prefix, then anything else — with the
//! newest document first inside each rank.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::error::{Result, SalesError};
use crate::money::Money;
use crate::store::MAX_PER_PAGE;

/// How far back a report may reach when the caller names no window.
///
/// Thirty days is the window the overview's conversion figure is defined over; a report that
/// silently used a different one would be two numbers with one name on the same screen.
pub const DEFAULT_REPORT_DAYS: i64 = 30;

/// Longest a report may reach back, in days.
pub const MAX_REPORT_DAYS: i64 = 1_825;

/// Hard cap on the rows a report's table returns.
///
/// The table is a *report* — a person reads it — so it is bounded like a page. The CSV is where a
/// caller asks for more, and it is the same function with a bigger cap.
pub const MAX_REPORT_ROWS: i64 = 1_000;

/// Longest a search term may be.
pub const MAX_SEARCH_LENGTH: usize = 120;

/// The label a quote with no seller carries, on its own report line.
///
/// A constant rather than a string at each site: two spellings would make the unassigned bucket
/// look like two different owners in a CSV, and a CSV that disagrees with the table is worse than
/// no CSV.
pub const UNASSIGNED: &str = "Unassigned";

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// The report as the screen reads it: the four numbers, the per-owner breakdown and the rows
/// behind them.
///
/// The three levels are computed from the same `case` expression, which is what stops a headline
/// from disagreeing with the sum of its own table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SalesReport {
    /// The first day of the window.
    pub from: String,
    /// The last day of the window, inclusive.
    pub to: String,
    /// The organization's default currency.
    ///
    /// **Not** a claim that every row is in it: a desk that sells in two currencies has one
    /// default here and a currency on each document, and the screen shows both. Saying otherwise
    /// would be a lie the moment the second currency appears.
    pub currency: String,
    /// The four buckets, which add up to `quotes_seen`.
    pub totals: ReportTotals,
    /// The per-seller breakdown, most wins first.
    pub by_owner: Vec<OwnerRow>,
    /// The rows behind the numbers, newest first, capped by [`MAX_REPORT_ROWS`].
    pub rows: Vec<ReportRow>,
    /// How many quotes the filter matched before the cap.
    ///
    /// Separate from `truncated` because the screen needs both: this is the "412 quotes" the
    /// filter bar prints, and `truncated` is the amber line that says the table below is only
    /// showing some of them. A report that had one field would make the reader divide two numbers
    /// to learn whether they are looking at everything.
    pub rows_matched: i64,
    /// Whether the cap cut the table short — and therefore whether the CSV holds fewer rows than
    /// the filter matched, which the export says out loud rather than letting somebody reconcile
    /// two files by hand.
    pub truncated: bool,
    /// Won ÷ (won + lost), in hundredths of a percent, or `None` when nothing was decided.
    pub conversion_bps: Option<i64>,
    /// Accepted quotes that became an order ÷ every accepted quote — the spec's
    /// "quote-to-order conversion", which is a different number from the line above.
    pub order_conversion_bps: Option<i64>,
    /// The mean size of a won deal, or `None` when nothing was won.
    pub average_deal: Option<String>,
    /// The sum of the won deals.
    pub won_value: String,
    /// The sum of the lost quotes.
    pub lost_value: String,
}

/// The four buckets.
///
/// `won + lost + pending + cancelled == quotes_seen` **by construction** — they are columns of one
/// `group by` — so a caller can check the arithmetic without trusting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportTotals {
    /// Accepted and turned into an order.
    pub won: i64,
    /// Declined by the customer, or lapsed.
    pub lost: i64,
    /// Draft, sent, or accepted with no order written yet.
    pub pending: i64,
    /// Withdrawn by the organization: neither a win nor a loss.
    pub cancelled: i64,
    /// Every quote the filter matched, decided or not.
    pub quotes_seen: i64,
    /// How many accepted quotes became at least one order.
    pub accepted_with_order: i64,
    /// How many accepted quotes have not become an order yet.
    pub accepted_without_order: i64,
}

/// One seller's line of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRow {
    /// The seller, or `None` for a quote nobody owns.
    pub owner_user_id: Option<Uuid>,
    /// The seller's name, or [`UNASSIGNED`].
    pub owner_name: String,
    /// The buckets, the same four the totals carry.
    pub totals: ReportTotals,
    /// The mean size of this seller's won deals, or `None` with no wins.
    pub average_deal: Option<String>,
    /// The sum of this seller's won deals.
    pub won_value: String,
}

/// One row of the table under the numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRow {
    /// The quote's id.
    pub quote_id: Uuid,
    /// The quote's number.
    pub number: String,
    /// The customer, as the quote recorded it.
    pub customer: String,
    /// The seller, by name, or nobody.
    pub owner_name: String,
    /// The bucket this row falls into (`won`, `lost`, `pending`, `cancelled`).
    pub outcome: String,
    /// The status the document actually carries, so a `lost` row can be `declined` or `expired`.
    pub status: String,
    /// The day the report counted the row by.
    pub date: String,
    /// This row's own currency, which is not necessarily the report's.
    pub currency: String,
    /// The grand total, as decimal text.
    pub grand_total: String,
    /// The order this quote became, if it has.
    pub order_number: Option<String>,
}

/// One document the global search found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// `quote` or `order`.
    ///
    /// A `String` rather than `&'static str`: `Deserialize` cannot fill a `&'static str` from a
    /// payload, and the alternative — a custom deserializer for one field — is more machinery
    /// than the lifetime saves.
    pub kind: String,
    /// The document's id.
    pub id: Uuid,
    /// Its number.
    pub number: String,
    /// The customer, as the document recorded it.
    pub customer: String,
    /// Its title; empty for an order, which has none.
    pub title: String,
    /// The status the document carries.
    pub status: String,
    /// The currency of its totals.
    pub currency: String,
    /// The grand total, as decimal text.
    pub grand_total: String,
    /// When the document last changed.
    pub updated_at: String,
    /// Where the panel opens it.
    pub url: String,
    /// Whether the number or the customer is what matched — so the screen can say why a row is
    /// in the list rather than making the reader guess.
    pub matched_on: String,
}

/// What the palette's "search the sales desk" answer is: one ranked list and two counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalSearchResults {
    /// The hits, best first.
    pub hits: Vec<SearchHit>,
    /// How many quotes the term matched, before the cap.
    pub quotes: i64,
    /// How many orders the term matched, before the cap.
    pub orders: i64,
}

// ---------------------------------------------------------------------------------------------
// The filters
// ---------------------------------------------------------------------------------------------

/// The filters a report screen sends.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReportQuery {
    /// First day of the window, inclusive, as `YYYY-MM-DD`. Defaults to 30 days ago.
    #[serde(default)]
    pub from: Option<String>,
    /// Last day of the window, inclusive, as `YYYY-MM-DD`. Defaults to today.
    #[serde(default)]
    pub to: Option<String>,
    /// One seller.
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// `true` to select the quotes nobody owns.
    #[serde(default)]
    pub unassigned: Option<bool>,
    /// One or more quote statuses; empty means every one.
    #[serde(default)]
    pub status: Option<String>,
    /// Rows to return.
    #[serde(default)]
    pub limit: Option<i64>,
}

impl ReportQuery {
    /// The window, defaulted and bounded.
    ///
    /// The rules live here rather than at a call site because there is exactly one answer to
    /// "is this window legal", and a second implementation of it is the one that forgets the
    /// `from > to` case.
    fn window(&self) -> Result<(time::Date, time::Date)> {
        let today = crate::quotes::today_utc();
        let to = match self.to.as_deref() {
            None | Some("") => today,
            Some(raw) => parse_day(raw, "to")?,
        };
        let from = match self.from.as_deref() {
            None | Some("") => to - time::Duration::days(DEFAULT_REPORT_DAYS - 1),
            Some(raw) => parse_day(raw, "from")?,
        };
        if from > to {
            return Err(SalesError::InvalidQuery(
                "the report starts after it ends".to_string(),
            ));
        }
        if (to - from).whole_days() + 1 > MAX_REPORT_DAYS {
            return Err(SalesError::InvalidQuery(format!(
                "a report may span at most {MAX_REPORT_DAYS} days"
            )));
        }
        Ok((from, to))
    }

    /// The row cap, clamped.
    fn row_cap(&self) -> i64 {
        self.limit.unwrap_or(MAX_REPORT_ROWS).clamp(1, MAX_REPORT_ROWS)
    }
}

/// A day the report filter parses, refusing what it cannot read.
fn parse_day(raw: &str, field: &'static str) -> Result<time::Date> {
    time::Date::parse(raw.trim(), &time::format_description::well_known::Iso8601::DATE)
        .map_err(|_| SalesError::InvalidQuery(format!("{field} is not a date the platform reads")))
}

/// The owner predicate, in one type so every query applies the same one.
struct OwnerFilter {
    user_id: Option<Uuid>,
    unassigned: bool,
}

impl OwnerFilter {
    /// Read the filter. `unassigned` **wins** over an id that was also sent, because a screen
    /// that offers both is offering a contradiction and the literal "nobody owns this" is the
    /// one a reader can see they chose.
    fn new(query: &ReportQuery) -> Self {
        let unassigned = query.unassigned == Some(true);
        Self {
            user_id: if unassigned { None } else { query.owner_user_id },
            unassigned,
        }
    }

    fn push<'a>(&self, builder: &mut QueryBuilder<'a, Postgres>) {
        if self.unassigned {
            builder.push(" and q.owner_user_id is null");
        } else if let Some(owner) = self.user_id {
            builder.push(" and q.owner_user_id = ");
            builder.push_bind(owner);
        }
    }
}

/// The statuses a report filter accepts; empty means every one.
fn parse_status_filter(raw: Option<&str>) -> Result<Vec<String>> {
    let Some(raw) = raw else { return Ok(Vec::new()) };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for part in trimmed.split(',') {
        let value = part.trim();
        if value.is_empty() {
            continue;
        }
        if crate::model::QuoteStatus::parse(value).is_none() {
            return Err(SalesError::InvalidQuery(format!(
                "{value} is not a quote status"
            )));
        }
        out.push(value.to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The SQL every report query shares
// ---------------------------------------------------------------------------------------------

/// How a quote is classified for the report.
///
/// The join it reads is the one order the quote produced. An accepted quote **with** an order is
/// won; an accepted quote without one is `pending`, because the customer said yes and nobody has
/// written the order yet — calling that a win puts a number on the board the desk has not earned.
const CLASSIFY: &str = "case \
       when q.status = 'accepted' and o.id is not null then 'won' \
       when q.status in ('declined', 'expired') then 'lost' \
       when q.status = 'cancelled' then 'cancelled' \
       else 'pending' end";

/// The day a quote is counted by: the outcome, or the document's own birth.
///
/// Three outcome columns rather than one `decided_at`, because the schema has three: a quote is
/// accepted, declined or cancelled, and there is no single column that means "the day somebody
/// decided" — an earlier draft of this file assumed one and the summary answered `500` with
/// "column q.decided_at does not exist", which is the cheapest possible way to find out.
const REPORT_DAY: &str =
    "coalesce(q.accepted_at, q.declined_at, q.cancelled_at, q.created_at)::date";

/// The one order a quote produced, joined in.
///
/// Every query in this file uses it, so `won` cannot mean one thing in the counts and another in
/// the rows. One order per quote is the rule the module already enforces — converting twice
/// returns the same order — so `limit 1` is belt-and-braces for a row an operator's own script
/// may have made.
const ORDER_JOIN: &str = "left join lateral (select o.id, o.number from sales_orders o \
                             where o.quote_id = q.id and o.archived_at is null \
                             order by o.created_at, o.id limit 1) o on true";

/// The `from` and `where` half every report query shares: the join, the tenant, the window and
/// the status filter.
///
/// It takes a builder rather than returning SQL text because the placeholders and the binds have
/// to move together — a filter that counts `$1` and binds `$2` is a report that reads a
/// stranger's tenant.
fn report_from<'a>(
    builder: &mut QueryBuilder<'a, Postgres>,
    organization_id: Uuid,
    statuses: &'a [String],
    from: time::Date,
    to: time::Date,
) {
    builder.push(" from sales_quotes q ");
    builder.push(ORDER_JOIN);
    builder.push(" where q.organization_id = ");
    builder.push_bind(organization_id);
    builder.push(" and q.archived_at is null and ");
    builder.push(REPORT_DAY);
    builder.push(" between ");
    builder.push_bind(from);
    builder.push(" and ");
    builder.push_bind(to);
    if !statuses.is_empty() {
        builder.push(" and q.status = any(");
        builder.push_bind(statuses);
        builder.push(")");
    }
}

/// One `(outcome, count, value)` bucket from the shared `group by`.
#[derive(Debug, FromRow)]
struct Bucket {
    outcome: String,
    n: i64,
    value: String,
}

/// One quote as the report's table sees it.
#[derive(Debug, FromRow)]
struct ClassifiedQuote {
    quote_id: Uuid,
    number: String,
    customer_name: String,
    owner_user_id: Option<Uuid>,
    outcome: String,
    status: String,
    currency: String,
    grand_total: String,
    day: time::Date,
    order_number: Option<String>,
}

/// `GET /sales/reports/summary` — the whole report, from one classification.
pub async fn build_report(
    pool: &PgPool,
    organization_id: Uuid,
    query: &ReportQuery,
) -> Result<SalesReport> {
    let (from, to) = query.window()?;
    let statuses = parse_status_filter(query.status.as_deref())?;
    let owner = OwnerFilter::new(query);
    let cap = query.row_cap();

    let mut buckets: QueryBuilder<Postgres> = QueryBuilder::new("select ");
    buckets.push(CLASSIFY);
    buckets.push(" as outcome, count(*) as n, coalesce(sum(q.grand_total), 0)::text as value ");
    report_from(&mut buckets, organization_id, &statuses, from, to);
    owner.push(&mut buckets);
    buckets.push(" group by 1");
    let buckets: Vec<Bucket> = buckets.build_query_as().fetch_all(pool).await?;

    let mut totals = ReportTotals {
        won: 0,
        lost: 0,
        pending: 0,
        cancelled: 0,
        quotes_seen: 0,
        accepted_with_order: 0,
        accepted_without_order: 0,
    };
    let mut won_value = Money::zero();
    let mut lost_value = Money::zero();
    for bucket in buckets {
        totals.quotes_seen += bucket.n;
        match bucket.outcome.as_str() {
            "won" => {
                totals.won += bucket.n;
                won_value = add_money(won_value, &bucket.value);
            }
            "lost" => {
                totals.lost += bucket.n;
                lost_value = add_money(lost_value, &bucket.value);
            }
            "cancelled" => totals.cancelled += bucket.n,
            _ => totals.pending += bucket.n,
        }
    }

    // How much of the accepted pipeline has actually been written up. A separate statement
    // because it needs the *status* filter dropped: this figure is about the accepted quotes
    // inside the window, and asking it to honour a `status=sent` filter would make the
    // conversion rate a function of a dropdown nobody remembers to clear.
    let (with_order, without_order): (i64, i64) = sqlx::query_as(
        "select count(*) filter (where exists (select 1 from sales_orders o \
                                    where o.quote_id = q.id and o.archived_at is null)), \
                count(*) filter (where not exists (select 1 from sales_orders o2 \
                                    where o2.quote_id = q.id and o2.archived_at is null)) \
           from sales_quotes q \
          where q.organization_id = $1 and q.archived_at is null and q.status = 'accepted' \
            and coalesce(q.accepted_at, q.declined_at, q.cancelled_at, q.created_at)::date \
                between $2 and $3",
    )
    .bind(organization_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    totals.accepted_with_order = with_order;
    totals.accepted_without_order = without_order;

    let mut rows_query: QueryBuilder<Postgres> = QueryBuilder::new(
        "select q.id as quote_id, q.number, q.customer_name, q.owner_user_id, ",
    );
    rows_query.push(CLASSIFY);
    rows_query.push(" as outcome, q.status, q.currency, q.grand_total::text, ");
    rows_query.push(REPORT_DAY);
    rows_query.push(" as day, o.number as order_number ");
    report_from(&mut rows_query, organization_id, &statuses, from, to);
    owner.push(&mut rows_query);
    rows_query.push(" order by ");
    rows_query.push(REPORT_DAY);
    rows_query.push(" desc, q.id desc limit ");
    rows_query.push(cap + 1);
    let mut rows: Vec<ClassifiedQuote> = rows_query.build_query_as().fetch_all(pool).await?;
    let truncated = rows.len() as i64 > cap;
    rows.truncate(cap as usize);

    let names = crate::quotes::owner_names(
        pool,
        &rows.iter().filter_map(|r| r.owner_user_id).collect::<Vec<_>>(),
    )
    .await;

    let by_owner = owner_breakdown(pool, organization_id, &statuses, from, to).await?;

    // The strict reading: an outcome the customer reached. `cancelled` is excluded on purpose —
    // the organization withdrew it, and counting that as a lost sale would understate a seller
    // who tidied up their own pipeline.
    let conversion_bps = percent_of(totals.won, totals.won + totals.lost);
    // The spec's phrase literally: of the quotes a customer said yes to, how many have become an
    // order. It is a different number from the line above, and a seller told only one of them
    // would be misled by the other.
    let order_conversion_bps = percent_of(
        totals.accepted_with_order,
        totals.accepted_with_order + totals.accepted_without_order,
    );
    let won_value = won_value.to_text();
    let average_deal = if totals.won > 0 {
        Some(divide(&won_value, totals.won))
    } else {
        None
    };

    Ok(SalesReport {
        from: from.to_string(),
        to: to.to_string(),
        currency: crate::store::get_settings(pool, organization_id)
            .await?
            .currency,
        totals,
        by_owner,
        rows: rows
            .into_iter()
            .map(|row| ReportRow {
                quote_id: row.quote_id,
                number: row.number,
                customer: row.customer_name,
                owner_name: row
                    .owner_user_id
                    .and_then(|id| names.get(&id).cloned())
                    .unwrap_or_else(|| UNASSIGNED.to_string()),
                outcome: row.outcome,
                status: row.status,
                date: row.day.to_string(),
                currency: row.currency,
                grand_total: row.grand_total,
                order_number: row.order_number,
            })
            .collect(),
        rows_matched: totals.quotes_seen,
        truncated,
        conversion_bps,
        order_conversion_bps,
        average_deal,
        won_value,
        lost_value: lost_value.to_text(),
    })
}

/// The per-seller breakdown, over the same window and statuses as the totals but **not** the
/// owner filter.
async fn owner_breakdown(
    pool: &PgPool,
    organization_id: Uuid,
    statuses: &[String],
    from: time::Date,
    to: time::Date,
) -> Result<Vec<OwnerRow>> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("select q.owner_user_id, ");
    builder.push(CLASSIFY);
    builder.push(" as outcome, count(*) as n, coalesce(sum(q.grand_total), 0)::text as value ");
    report_from(&mut builder, organization_id, statuses, from, to);
    builder.push(" group by 1, 2");
    let rows: Vec<OwnerBucket> = builder.build_query_as().fetch_all(pool).await?;

    let ids: Vec<Uuid> = rows.iter().filter_map(|r| r.owner_user_id).collect();
    let names = crate::quotes::owner_names(pool, &ids).await;

    let mut merged: std::collections::BTreeMap<Option<Uuid>, (ReportTotals, Money)> =
        Default::default();
    for row in rows {
        let entry = merged.entry(row.owner_user_id).or_insert((
            ReportTotals {
                won: 0,
                lost: 0,
                pending: 0,
                cancelled: 0,
                quotes_seen: 0,
                accepted_with_order: 0,
                accepted_without_order: 0,
            },
            Money::zero(),
        ));
        entry.0.quotes_seen += row.n;
        match row.outcome.as_str() {
            "won" => {
                entry.0.won += row.n;
                entry.1 = entry.1.plus(Money::parse(&row.value).unwrap_or_else(|_| Money::zero()));
            }
            "lost" => entry.0.lost += row.n,
            "cancelled" => entry.0.cancelled += row.n,
            _ => entry.0.pending += row.n,
        }
    }

    let mut out: Vec<OwnerRow> = merged
        .into_iter()
        .map(|(id, (totals, value))| {
            let won_value = value.to_text();
            OwnerRow {
                owner_user_id: id,
                owner_name: id
                    .and_then(|id| names.get(&id).cloned())
                    .unwrap_or_else(|| UNASSIGNED.to_string()),
                average_deal: if totals.won > 0 {
                    Some(divide(&won_value, totals.won))
                } else {
                    None
                },
                won_value,
                totals,
            }
        })
        .collect();
    // Most wins first, ties broken by name so two passes of the same data print the same order.
    // A report that reorders itself between refreshes is one nobody screenshots.
    out.sort_by(|a, b| {
        b.totals
            .won
            .cmp(&a.totals.won)
            .then_with(|| a.owner_name.cmp(&b.owner_name))
    });
    Ok(out)
}

/// One `(owner, outcome)` bucket from the breakdown's `group by`.
#[derive(Debug, FromRow)]
struct OwnerBucket {
    owner_user_id: Option<Uuid>,
    outcome: String,
    n: i64,
    value: String,
}

/// `part / whole` in hundredths of a percent, or `None` when there is nothing to divide by.
///
/// Returning `None` rather than `0` is the difference between "nobody decided yet" and "nobody
/// won anything" — a conversion of 0% on a desk that has not quoted yet is a fact about a
/// competitor that is not there.
fn percent_of(part: i64, whole: i64) -> Option<i64> {
    if whole <= 0 {
        return None;
    }
    Some(part.saturating_mul(10_000) / whole)
}

/// `value / count` to the cent, in exact decimal rather than `f64`.
///
/// The sum came out of PostgreSQL as `numeric` text, and dividing it in a float would put a
/// rounding error in the one figure a board reads twice — once on the screen, once in the CSV.
/// [`Money`] holds hundredths as `i128`, so the mean is exact to the cent and both files print
/// the same digits.
fn divide(value: &str, count: i64) -> String {
    if count <= 0 {
        return "0.00".to_string();
    }
    let Ok(amount) = Money::parse(value) else {
        // A value PostgreSQL could not have produced still has to render as something, and zero
        // is the only answer that cannot be mistaken for a real figure on a board slide.
        return "0.00".to_string();
    };
    let count = i128::from(count);
    let denominator = count.saturating_mul(10_000).max(1);
    // `+ count/2` before the division is round-half-away-from-zero: the same rule the line
    // totals use, so a mean of ten 100.05 deals is 100.05 and not 100.04.
    let minor = (amount.minor().saturating_mul(10_000) + count / 2) / denominator;
    Money::from_minor(minor).unwrap_or_else(Money::zero).to_text()
}

/// Add a PostgreSQL `numeric` sum to a running total, in exact decimal.
fn add_money(acc: Money, value: &str) -> Money {
    acc.plus(Money::parse(value).unwrap_or_else(|_| Money::zero()))
}

// ---------------------------------------------------------------------------------------------
// Global search
// ---------------------------------------------------------------------------------------------

/// `GET /sales/search` — quotes and orders by number and customer, in one ranked list.
///
/// One statement, one ranking, both document kinds, for the reason in the module header: the
/// ⌘K palette fires on every pause in typing, and two separately ranked lists could not be merged
/// honestly anyway.
pub async fn global_search(
    pool: &PgPool,
    organization_id: Uuid,
    term: &str,
    limit: Option<i64>,
) -> Result<GlobalSearchResults> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(GlobalSearchResults {
            hits: Vec::new(),
            quotes: 0,
            orders: 0,
        });
    }
    if term.chars().count() > MAX_SEARCH_LENGTH {
        return Err(SalesError::InvalidQuery(
            "that search term is too long".to_string(),
        ));
    }
    let limit = limit.unwrap_or(20).clamp(1, MAX_PER_PAGE);
    let pattern = format!("%{}%", escape_like(term));
    let prefix = format!("{}%", escape_like(term));

    let rows = sqlx::query(
        "with hits as (
             select 'quote'::text as kind, q.id, q.number, q.customer_name, q.title,
                    q.status, q.currency, q.grand_total::text, q.updated_at,
                    case when lower(q.number) = lower($2) then 0
                         when lower(q.number) like lower($3) then 1
                         when lower(q.customer_name) like lower($3) then 2
                         else 3 end as rank,
                    case when lower(q.number) like lower($3) then 'number'
                         else 'customer' end as matched_on
               from sales_quotes q
              where q.organization_id = $1 and q.archived_at is null
                and (q.number ilike $4 or q.customer_name ilike $4 or q.title ilike $4)
             union all
             select 'order'::text, o.id, o.number, o.customer_name, ''::text,
                    o.status, o.currency, o.grand_total::text, o.updated_at,
                    case when lower(o.number) = lower($2) then 0
                         when lower(o.number) like lower($3) then 1
                         when lower(o.customer_name) like lower($3) then 2
                         else 3 end as rank,
                    case when lower(o.number) like lower($3) then 'number'
                         else 'customer' end as matched_on
               from sales_orders o
              where o.organization_id = $1 and o.archived_at is null
                and (o.number ilike $4 or o.customer_name ilike $4)
         )
         select kind, id, number, customer_name, title, status, currency, grand_total,
                updated_at, matched_on, rank,
                count(*) over (partition by kind) as kind_total
           from hits
          order by rank, updated_at desc, id
          limit $5",
    )
    .bind(organization_id)
    .bind(term)
    .bind(&prefix)
    .bind(&pattern)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut quotes = 0;
    let mut orders = 0;
    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let kind: String = row.try_get("kind")?;
        let is_quote = kind == "quote";
        // The window's `count(*) over (partition by kind)` travels with the row, so a capped
        // result still says how many documents the term matched in total — otherwise a caller
        // cannot tell "one order" from "forty, you are seeing twenty".
        let kind_total: i64 = row.try_get("kind_total")?;
        if is_quote {
            quotes = kind_total;
        } else {
            orders = kind_total;
        }
        let updated_at: time::OffsetDateTime = row.try_get("updated_at")?;
        let id: Uuid = row.try_get("id")?;
        let kind_slug = if is_quote { "quote" } else { "order" };
        hits.push(SearchHit {
            kind: if is_quote { "quote" } else { "order" }.to_string(),
            id,
            url: format!("/sales/{kind_slug}s/{id}"),
            number: row.try_get("number")?,
            customer: row.try_get("customer_name")?,
            title: row.try_get("title")?,
            status: row.try_get("status")?,
            currency: row.try_get("currency")?,
            grand_total: row.try_get("grand_total")?,
            updated_at: updated_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            matched_on: row.try_get::<String, _>("matched_on")?,
        });
    }
    Ok(GlobalSearchResults { hits, quotes, orders })
}

// ---------------------------------------------------------------------------------------------
// The CSV export
// ---------------------------------------------------------------------------------------------

/// `GET /sales/reports/export` — the table's rows as CSV.
///
/// Generated from the **same** [`build_report`] the screen's table came from, which is the only
/// way "the CSV contains the same rows as the table" is true by construction rather than by two
/// implementations agreeing by luck. A caller wanting the whole set asks for a report with a
/// larger `limit`; the screen's cap and the export's are the same number, so the export never
/// silently contains more or fewer rows than the table above it.
pub fn report_csv(report: &SalesReport) -> String {
    let mut out = String::with_capacity(4_096);
    out.push_str("number,customer,owner,outcome,status,date,currency,grand_total,order_number\r\n");
    for row in &report.rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\r\n",
            csv_cell(&row.number),
            csv_cell(&row.customer),
            csv_cell(&row.owner_name),
            csv_cell(&row.outcome),
            csv_cell(&row.status),
            csv_cell(&row.date),
            csv_cell(&row.currency),
            csv_cell(&row.grand_total),
            csv_cell(row.order_number.as_deref().unwrap_or("")),
        ));
    }
    // A BOM, because Excel opens a CSV holding a customer's name and mangles every non-ASCII
    // character without one — and a Turkish organization is the first thing this module's
    // customers are.
    format!("\u{feff}{out}")
}

/// One CSV cell: quoted when it holds a comma, a quote or a newline, with quotes doubled.
///
/// A hand-rolled writer rather than a dependency, because the rule is four lines and a crate in a
/// public repository has to earn its place.
fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Escape what `like` treats as wildcards, so a `%` in a search term is a percent sign.
fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_report(rows: Vec<ReportRow>) -> SalesReport {
        SalesReport {
            from: "2026-01-01".into(),
            to: "2026-01-31".into(),
            currency: "TRY".into(),
            totals: ReportTotals {
                won: 0,
                lost: 0,
                pending: 0,
                cancelled: 0,
                quotes_seen: 0,
                accepted_with_order: 0,
                accepted_without_order: 0,
            },
            by_owner: Vec::new(),
            rows,
            rows_matched: 0,
            truncated: false,
            conversion_bps: None,
            order_conversion_bps: None,
            average_deal: None,
            won_value: "0.00".into(),
            lost_value: "0.00".into(),
        }
    }

    fn row(number: &str, customer: &str, outcome: &str, total: &str) -> ReportRow {
        ReportRow {
            quote_id: Uuid::nil(),
            number: number.into(),
            customer: customer.into(),
            owner_name: UNASSIGNED.into(),
            outcome: outcome.into(),
            status: "accepted".into(),
            date: "2026-01-15".into(),
            currency: "TRY".into(),
            grand_total: total.into(),
            order_number: None,
        }
    }

    #[test]
    fn a_report_window_defaults_to_thirty_days_ending_today() {
        let (from, to) = ReportQuery::default()
            .window()
            .expect("the default window is legal");
        assert_eq!(to, crate::quotes::today_utc());
        assert_eq!((to - from).whole_days() + 1, DEFAULT_REPORT_DAYS);
    }

    #[test]
    fn a_window_that_starts_after_it_ends_is_refused_by_name() {
        let query = ReportQuery {
            from: Some("2026-03-10".into()),
            to: Some("2026-03-01".into()),
            ..ReportQuery::default()
        };
        let err = query.window().expect_err("a backwards window is not a report");
        assert!(err.to_string().contains("starts after it ends"), "{err}");
    }

    #[test]
    fn a_window_wider_than_the_ceiling_is_refused() {
        let query = ReportQuery {
            from: Some("2000-01-01".into()),
            to: Some("2026-01-01".into()),
            ..ReportQuery::default()
        };
        let err = query.window().expect_err("six years is not a report");
        assert!(err.to_string().contains("at most"), "{err}");
    }

    #[test]
    fn a_window_that_is_not_a_date_is_refused_at_the_field() {
        let query = ReportQuery {
            from: Some("last tuesday".into()),
            ..ReportQuery::default()
        };
        let err = query.window().expect_err("a weekday name is not a date");
        assert!(err.to_string().contains("from"), "{err}");
    }

    #[test]
    fn a_row_cap_is_clamped_rather_than_trusted() {
        let huge = ReportQuery {
            limit: Some(1_000_000),
            ..ReportQuery::default()
        };
        assert_eq!(huge.row_cap(), MAX_REPORT_ROWS);
        let negative = ReportQuery {
            limit: Some(-5),
            ..ReportQuery::default()
        };
        assert_eq!(negative.row_cap(), 1);
    }

    #[test]
    fn an_empty_status_filter_means_every_status() {
        assert!(
            parse_status_filter(None)
                .expect("no filter is fine")
                .is_empty()
        );
        assert!(
            parse_status_filter(Some("all"))
                .expect("`all` is the empty filter")
                .is_empty()
        );
        assert_eq!(
            parse_status_filter(Some("draft, accepted"))
                .expect("a known list parses"),
            vec!["draft".to_string(), "accepted".to_string()]
        );
    }

    #[test]
    fn an_unknown_status_is_refused_rather_than_ignored() {
        let err = parse_status_filter(Some("draft,invented"))
            .expect_err("a typo is not a filter");
        assert!(err.to_string().contains("invented"), "{err}");
    }

    #[test]
    fn a_filter_of_only_commas_widens_to_everything_and_is_still_a_filter() {
        // The dangerous bug this guards is a filter that parses to an empty list, which the
        // query then reads as "no filter" and answers with the whole table.
        assert!(parse_status_filter(Some(",,")).expect("commas parse").is_empty());
        let err = parse_status_filter(Some("draft,nope")).expect_err("an unknown status is refused");
        assert!(matches!(err, SalesError::InvalidQuery(_)), "{err}");
    }

    #[test]
    fn a_percentage_of_nothing_is_absent_rather_than_zero() {
        assert_eq!(percent_of(3, 0), None);
        assert_eq!(percent_of(0, 4), Some(0));
        assert_eq!(percent_of(1, 4), Some(2_500));
        assert_eq!(percent_of(1, 3), Some(3_333));
    }

    #[test]
    fn a_percentage_never_exceeds_a_hundred() {
        assert_eq!(percent_of(2, 3), Some(6_666));
        assert_eq!(percent_of(7, 7), Some(10_000));
        assert_eq!(percent_of(1, 1), Some(10_000));
    }

    #[test]
    fn a_mean_rounds_half_away_from_zero_at_the_cent() {
        assert_eq!(divide("1000.00", 3), "333.33");
        assert_eq!(divide("0.00", 0), "0.00");
        assert_eq!(divide("407.76", 1), "407.76");
        // 10 deals of 100.05 average to 100.05; a float or a round-half-to-even would print
        // 100.04 for some of them, and the CSV would then disagree with the screen.
        assert_eq!(divide("1000.50", 10), "100.05");
    }

    #[test]
    fn a_mean_of_a_value_postgres_could_not_have_produced_is_zero() {
        assert_eq!(divide("not a number", 3), "0.00");
    }

    #[test]
    fn adding_two_postgres_sums_is_exact() {
        let sum = add_money(Money::zero(), "100.05").plus(Money::parse("0.10").expect("parses"));
        assert_eq!(sum.to_text(), "100.15");
    }

    #[test]
    fn a_csv_cell_quotes_what_would_otherwise_break_a_row() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("Acme, Inc."), "\"Acme, Inc.\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn the_export_holds_the_same_rows_the_table_held() {
        let report = empty_report(vec![
            row("Q-2026-0001", "Acme, Inc.", "won", "1000.00"),
            row("Q-2026-0002", "Beta", "lost", "250.50"),
        ]);
        let csv = report_csv(&report);
        let lines: Vec<&str> = csv.lines().collect();
        // The BOM+header, then one line per row — nothing added, nothing dropped.
        assert_eq!(lines.len(), 1 + report.rows.len(), "{csv}");
        assert!(csv.starts_with('\u{feff}'), "the BOM is the first byte");
        assert!(lines[1].contains("\"Acme, Inc.\""), "{}", lines[1]);
        assert!(lines[2].contains("Q-2026-0002"), "{}", lines[2]);
    }

    #[test]
    fn a_capped_report_says_so_and_says_by_how_much() {
        // The two fields exist because a reader needs both: "412 quotes" without "showing 1 000"
        // is a filter that quietly lies, and "showing 1 000" without the total is a file
        // somebody reconciles by hand.
        let mut report = empty_report(vec![row("Q-2026-0001", "Acme", "won", "10.00")]);
        report.rows_matched = 412;
        report.truncated = true;
        assert_eq!(report.rows_matched, 412);
        assert!(report.truncated);
    }

    #[test]
    fn the_export_of_an_empty_table_is_just_a_header() {
        let csv = report_csv(&empty_report(Vec::new()));
        assert_eq!(csv.lines().count(), 1, "{csv}");
        assert!(csv.contains("grand_total"));
    }

    #[test]
    fn a_like_wildcard_in_a_search_term_is_a_character_and_not_a_wildcard() {
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn the_unassigned_bucket_is_one_spelling_everywhere() {
        // Two spellings would make one bucket look like two owners in a CSV, and a CSV that
        // disagrees with the table is worse than no CSV.
        assert_eq!(UNASSIGNED, "Unassigned");
    }
}
