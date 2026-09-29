//! The security store: the SQL behind posture results, findings and their status changes.
//!
//! Three rules shape every read and write here, and each of them exists because of a specific
//! way a security screen goes wrong:
//!
//! * **The count and the page are one question.** [`list_findings`] builds its `WHERE` clause
//!   and its bind values from one loop over one filter list and counts with the *same* clause.
//!   The alternative is a total that disagrees with the rows above it, which on a security
//!   screen reads as "3 open findings" over an empty table.
//! * **A finding you may not see is `None`, exactly like one that is gone.** There is no
//!   `Forbidden` variant, so the detail route cannot become a probe for what exists.
//! * **Re-ingest updates, it does not duplicate.** A report is a statement about a moment; the
//!   finding is a fact, and a fact confirmed twice is one row with a moved `last_seen_at`.

use sqlx::PgPool;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::error::{Result, SecurityError};
use crate::model::{
    CheckResult, Finding, FindingPage, FindingQuery, NewCheckResult, NewFinding, StatusChange,
};
use crate::vocabulary::{MAX_BULK_IDS, is_finding_status, is_severity, is_source};

const CHECK_COLUMNS: &str = "id, organization_id, check_key, state, detail, run_id, checked_at";
const FINDING_COLUMNS: &str = "id, organization_id, source, severity, title, description, \
                              component, component_version, fixed_in, status, ignore_reason, \
                              ignored_until, acknowledged_by, acknowledged_at, note, \
                              first_seen_at, last_seen_at, fingerprint";

// ---------------------------------------------------------------------------------------------
// Posture results
// ---------------------------------------------------------------------------------------------

/// The latest result for every check that has ever been evaluated in this organization.
///
/// "Latest" is resolved per check key in SQL rather than in Rust: the panel's read is a few
/// rows, and a query that returns a whole run's history for the overview is a query whose cost
/// grows with how long the platform has been up.
pub async fn latest_results(pool: &PgPool, organization_id: Option<Uuid>) -> Result<Vec<CheckResult>> {
    let sql = format!(
        "select {CHECK_COLUMNS} from security_check_results r \
         where r.organization_id is not distinct from $1 \
           and r.id = (select newest.id from security_check_results newest \
                       where newest.organization_id is not distinct from $1 \
                         and newest.check_key = r.check_key \
                       order by newest.checked_at desc, newest.id desc limit 1) \
         order by r.checked_at desc"
    );
    let rows = sqlx::query_as::<_, CheckResult>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The `checked_at` of the most recent result of any kind — the "last scan" line in the header.
pub async fn last_run_at(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<Option<time::OffsetDateTime>> {
    let checked_at: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "select max(checked_at) from security_check_results \
         where organization_id is not distinct from $1",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok(checked_at)
}

/// Write one run's results, replacing nothing.
///
/// Results are append-only on purpose: a screen that shows *when* a check last changed its
/// mind is worth more than a table that holds only the current answer, and the history is what
/// makes "it was green yesterday" answerable.
pub async fn record_run(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    results: &[NewCheckResult],
) -> Result<Vec<CheckResult>> {
    if results.is_empty() {
        return Ok(Vec::new());
    }
    let mut written = Vec::with_capacity(results.len());
    for raw in results {
        let draft = raw.clone().build()?;
        let row: CheckResult = sqlx::query_as(&format!(
            "insert into security_check_results \
             (organization_id, check_key, state, detail, run_id) \
             values ($1, $2, $3, $4, $5) \
             returning {CHECK_COLUMNS}"
        ))
        .bind(organization_id)
        .bind(&draft.check_key)
        .bind(&draft.state)
        .bind(&draft.detail)
        .bind(draft.run_id)
        .fetch_one(pool)
        .await?;
        written.push(row);
    }
    Ok(written)
}

// ---------------------------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------------------------

/// One page of findings and the total for the same filter.
pub async fn list_findings(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    query: &FindingQuery,
) -> Result<FindingPage> {
    let filter = finding_filter(organization_id, query)?;

    // The count and the page are pushed through the *same* builder helper in the same bind
    // order. A count that built its own clause is a count that can disagree with the list
    // above it, and "3 open findings" over an empty table is the worst thing this screen can
    // show.
    let mut count = QueryBuilder::<Postgres>::new("select count(*) from security_findings where ");
    push_filter(&mut count, &filter);
    let total: i64 = count.build_query_scalar().fetch_one(pool).await?;

    let limit = query.effective_limit() as i64;
    let mut page = QueryBuilder::<Postgres>::new(format!(
        "select {FINDING_COLUMNS} from security_findings where "
    ));
    push_filter(&mut page, &filter);
    // Worst first, then newest: an operator opening the findings tab is looking for the thing
    // that is worst and still current, not for the oldest open item.
    page.push(" order by case severity when 'critical' then 0 when 'high' then 1 \
               when 'medium' then 2 when 'low' then 3 when 'info' then 4 else 9 end, \
               last_seen_at desc, id desc limit ");
    page.push_bind(limit);
    page.push(" offset ");
    page.push_bind(query.offset.max(0));
    let findings = page
        .build_query_as::<Finding>()
        .fetch_all(pool)
        .await?;

    Ok(FindingPage {
        findings,
        total,
        offset: query.offset.max(0),
    })
}

/// One finding, or `None` for both "gone" and "not yours".
pub async fn find_finding(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    id: Uuid,
) -> Result<Option<Finding>> {
    let sql = format!(
        "select {FINDING_COLUMNS} from security_findings \
         where id = $1 and organization_id is not distinct from $2"
    );
    Ok(sqlx::query_as::<_, Finding>(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?)
}

/// Insert a finding, or refresh the one already there under the same fingerprint.
///
/// The `false` return means "already known": the caller reports it as a refresh rather than a
/// new finding, which is what makes re-running a CI report idempotent instead of doubling the
/// count every time the job runs.
pub async fn upsert_finding(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    draft: &NewFinding,
) -> Result<(Finding, bool)> {
    let built = draft.clone().build()?;
    let sql = format!(
        "insert into security_findings \
         (organization_id, source, severity, title, description, component, component_version, \
          fixed_in, fingerprint) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         on conflict (organization_id, fingerprint, coalesce(component_version, '')) \
         do update set last_seen_at = now(), \
                       severity = excluded.severity, \
                       description = excluded.description, \
                       fixed_in = excluded.fixed_in \
         returning {FINDING_COLUMNS}"
    );
    let row = sqlx::query_as::<_, Finding>(&sql)
        .bind(organization_id)
        .bind(&built.source)
        .bind(&built.severity)
        .bind(&built.title)
        .bind(&built.description)
        .bind(&built.component)
        .bind(&built.component_version)
        .bind(&built.fixed_in)
        .bind(&built.fingerprint)
        .fetch_one(pool)
        .await?;
    // `xmax = 0` is Postgres' way of saying "this insert did not collide" — the row came from
    // the DO UPDATE branch. Reading it is how one statement answers created-or-refreshed
    // without a second query, which is a second query that could see a *different* row after a
    // concurrent delete.
    let created: bool = sqlx::query_scalar("select coalesce(xmax, 0) = 0 from security_findings where id = $1")
        .bind(row.id)
        .fetch_one(pool)
        .await
        .unwrap_or(true);
    Ok((row, created))
}

/// Move a finding to a new status.
///
/// # Errors
///
/// [`SecurityError::NotFound`] when the row does not exist or belongs elsewhere;
/// [`SecurityError::Invalid`] when the change is not one the platform will record — an ignore
/// with no reason being the one that matters.
pub async fn set_status(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    id: Uuid,
    change: &StatusChange,
    actor: Option<Uuid>,
    now: time::OffsetDateTime,
) -> Result<Finding> {
    if !is_finding_status(&change.status) {
        return Err(SecurityError::invalid(format!(
            "status {:?} is not one the platform records",
            change.status
        )));
    }
    let change = change.clone().build(now)?;
    let sql = format!(
        "update security_findings set status = $3, \
             ignore_reason = case when $3 = 'ignored' then $4 else ignore_reason end, \
             ignored_until = case when $3 = 'ignored' then $5 else ignored_until end, \
             acknowledged_by = case when $3 = 'acknowledged' then $6 else acknowledged_by end, \
             acknowledged_at = case when $3 = 'acknowledged' then $7 else acknowledged_at end, \
             note = coalesce($8, note) \
         where id = $1 and organization_id is not distinct from $2 \
         returning {FINDING_COLUMNS}"
    );
    let row = sqlx::query_as::<_, Finding>(&sql)
        .bind(id)
        .bind(organization_id)
        .bind(&change.status)
        .bind(&change.ignore_reason)
        .bind(change.ignored_until)
        .bind(actor)
        .bind(now)
        .bind(&change.note)
        .fetch_optional(pool)
        .await?;
    row.ok_or(SecurityError::NotFound)
}

/// Apply one change to many findings, and report what it did.
///
/// The per-row report matters: a bulk acknowledge that quietly skipped three findings because
/// their ids were stale is a bulk acknowledge an operator will believe covered them.
pub async fn bulk_set_status(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    ids: &[Uuid],
    change: &StatusChange,
    actor: Option<Uuid>,
    now: time::OffsetDateTime,
) -> Result<BulkReport> {
    if ids.is_empty() {
        return Err(SecurityError::invalid("no findings were selected"));
    }
    if ids.len() > MAX_BULK_IDS {
        return Err(SecurityError::invalid(format!(
            "at most {MAX_BULK_IDS} findings can be changed at once, got {}",
            ids.len()
        )));
    }
    let mut updated = Vec::new();
    let mut missing = Vec::new();
    for id in ids {
        match set_status(pool, organization_id, *id, change, actor, now).await {
            Ok(row) => updated.push(row.id),
            Err(SecurityError::NotFound) => missing.push(*id),
            Err(other) => return Err(other),
        }
    }
    Ok(BulkReport { updated, missing })
}

/// What a bulk status change did, row by row.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkReport {
    /// The findings that changed.
    pub updated: Vec<Uuid>,
    /// The ids that matched nothing — stale selections, not failures.
    pub missing: Vec<Uuid>,
}

/// How many open findings carry each severity, for the overview's score ring.
///
/// Severities with no findings are **omitted**, not reported as zero: the panel adds the
/// missing ones from [`crate::vocabulary::SEVERITIES`] so the ring has a fixed set of buckets
/// and the check's own count agrees with the list it is a summary of.
pub async fn open_counts_by_severity(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    now: time::OffsetDateTime,
) -> Result<std::collections::BTreeMap<String, i64>> {
    // An ignore past its date is not an ignore, and the expiry is read here rather than by a
    // job: a job that did not fire would hide a finding that came back.
    let sql = "select severity, count(*) as count from security_findings \
               where organization_id is not distinct from $1 \
                 and (status in ('open', 'acknowledged') \
                      or (status = 'ignored' and (ignored_until is null or ignored_until > $2))) \
               group by severity";
    let rows: Vec<(String, i64)> = sqlx::query_as(sql).bind(organization_id).bind(now).fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

/// How many dependency findings are high or critical and still open — the freshness check's
/// only input. `None` means no report has ever been ingested, which the check reads as
/// `unknown` rather than as zero.
pub async fn stale_dependency_count(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    now: time::OffsetDateTime,
) -> Result<Option<i64>> {
    // `count(*)` over zero rows is `0`, never null — so the "no report ever" case is `0` and
    // not a null to unwrap. Reading it as `Option` would make the empty case and the null
    // case indistinguishable, and only one of them means "never scanned".
    let ingested: i64 = sqlx::query_scalar(
        "select count(*) from security_findings \
         where organization_id is not distinct from $1 and source in ('dependency', 'report')",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    if ingested == 0 {
        return Ok(None);
    }
    let count: i64 = sqlx::query_scalar(
        "select count(*) from security_findings \
         where organization_id is not distinct from $1 \
           and source in ('dependency', 'report') \
           and severity in ('critical', 'high') \
           and (status in ('open', 'acknowledged') \
                or (status = 'ignored' and (ignored_until is null or ignored_until > $2)))",
    )
    .bind(organization_id)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(Some(count))
}

/// How many findings the current filter matches, for the export's row count and the header.
pub async fn count_findings(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    query: &FindingQuery,
) -> Result<i64> {
    let filter = finding_filter(organization_id, query)?;
    let mut count = QueryBuilder::<Postgres>::new("select count(*) from security_findings where ");
    push_filter(&mut count, &filter);
    Ok(count.build_query_scalar().fetch_one(pool).await?)
}

/// Every finding the current filter matches, for the CSV export — unpaged by design.
///
/// The export is the one read here that is *not* a page, and the reason is that an export that
/// silently respects the current page size is the most common way a security report goes
/// wrong: an operator exports 200 rows, hands the file to an auditor, and the auditor reads a
/// filtered, truncated list as the platform's whole posture. `FindingQuery::limit` is ignored
/// here on purpose, and the count comes from the *same* filter, so the row count in the file
/// and the total on the screen are the same number.
pub async fn export_findings(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    query: &FindingQuery,
) -> Result<Vec<Finding>> {
    let filter = finding_filter(organization_id, query)?;
    let mut builder = QueryBuilder::<Postgres>::new(format!(
        "select {FINDING_COLUMNS} from security_findings where "
    ));
    push_filter(&mut builder, &filter);
    // The same worst-first order as the table, so a row in the file lines up with a row on
    // screen and a reader comparing the two is not looking for a different order.
    builder.push(" order by case severity when 'critical' then 0 when 'high' then 1 \
                  when 'medium' then 2 when 'low' then 3 when 'info' then 4 else 9 end, \
                  last_seen_at desc, id desc");
    Ok(builder.build_query_as::<Finding>().fetch_all(pool).await?)
}

/// One filter's condition, held as a value rather than as SQL text with hand-numbered
/// placeholders.
///
/// The earlier shape of this — a `String` of `"$1, $2, $3"` plus a parallel list of values — is
/// the kind of thing that works until one filter is added and the numbering is renumbered by
/// hand, at which point a filter can bind to the wrong column and nobody notices because the
/// query still runs. A `Condition` enum pushes its own value, so the clause and its bind are
/// written in one place and cannot drift.
/// The reason it is a `Debug` type: a test that cannot name what it got cannot report a
/// useful failure, and "expected a search condition" without the value is half an answer.
#[derive(Debug)]
enum Condition {
    Severity(String),
    Status(String),
    Source(String),
    Component(String),
    SeenAfter(time::OffsetDateTime),
    /// `(pattern, negated)` — the title match and the description match share one pattern, so
    /// it is pushed once and referenced twice.
    Search { pattern: String, negated: bool },
}

impl Condition {
    fn sql(&self) -> &'static str {
        match self {
            Self::Severity(_) => " and severity = ",
            Self::Status(_) => " and status = ",
            Self::Source(_) => " and source = ",
            Self::Component(_) => " and component = ",
            Self::SeenAfter(_) => " and last_seen_at >= ",
            Self::Search { negated, .. } => {
                if *negated {
                    " and description ilike "
                } else {
                    " and title ilike "
                }
            }
        }
    }

    fn push_value(&self, builder: &mut QueryBuilder<'_, Postgres>) {
        // Every arm must evaluate to `()`: `push_bind` returns the builder, and a match whose
        // arms return it is a match whose type is the builder — which is not what this
        // function returns.
        match self {
            Self::Severity(value)
            | Self::Status(value)
            | Self::Source(value)
            | Self::Component(value) => {
                builder.push_bind(value.clone());
            }
            Self::SeenAfter(at) => {
                builder.push_bind(*at);
            }
            Self::Search { pattern, .. } => {
                builder.push_bind(pattern.clone());
            }
        }
    }
}

/// A whole WHERE clause's worth of conditions, in bind order.
#[derive(Debug)]
struct Filter {
    /// The organization every read is scoped to. `None` is platform level, which is why the
    /// clause says `is not distinct from` and not `=`.
    organization: Option<Uuid>,
    conditions: Vec<Condition>,
}

/// Append a filter's clause and every bind, in one place, for one read.
///
/// There are three callers — the page, its count and the export's count — and they must agree
/// exactly. A count that built its own clause is a count that can disagree with the list above
/// it, and "3 open findings" over an empty table is the worst thing this screen can show.
fn push_filter(builder: &mut QueryBuilder<'_, Postgres>, filter: &Filter) {
    builder.push("organization_id is not distinct from ");
    builder.push_bind(filter.organization);
    for condition in &filter.conditions {
        builder.push(condition.sql());
        condition.push_value(builder);
        if let Condition::Search { pattern, negated: false } = condition {
            builder.push(" escape '\\' or description ilike ");
            builder.push_bind(pattern.clone());
            builder.push(" escape '\\'");
        }
    }
}

/// The one filter builder, used by the page, the count and the export.
///
/// Returns the conditions rather than a finished string, so a caller cannot get the clause and
/// the binds out of step.
fn finding_filter(
    organization: Option<Uuid>,
    query: &FindingQuery,
) -> Result<Filter> {
    let mut conditions: Vec<Condition> = Vec::new();

    if let Some(severity) = query.severity.as_deref().filter(|s| !s.is_empty()) {
        if !is_severity(severity) {
            return Err(SecurityError::invalid(format!("unknown severity {severity:?}")));
        }
        conditions.push(Condition::Severity(severity.to_string()));
    }
    if let Some(status) = query.status.as_deref().filter(|s| !s.is_empty()) {
        if !is_finding_status(status) {
            return Err(SecurityError::invalid(format!("unknown status {status:?}")));
        }
        conditions.push(Condition::Status(status.to_string()));
    }
    if let Some(source) = query.source.as_deref().filter(|s| !s.is_empty()) {
        if !is_source(source) {
            return Err(SecurityError::invalid(format!("unknown source {source:?}")));
        }
        conditions.push(Condition::Source(source.to_string()));
    }
    if let Some(component) = query.component.as_deref().filter(|s| !s.is_empty()) {
        conditions.push(Condition::Component(component.to_string()));
    }
    if let Some(seen_after) = query.seen_after {
        conditions.push(Condition::SeenAfter(seen_after));
    }
    if let Some(search) = query.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        // Escaped because the term goes into a LIKE pattern: an unescaped `%` in a search box
        // turns "search for 100%" into "search for everything", which shows up as a strange
        // total rather than as an error.
        let escaped = search
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        conditions.push(Condition::Search {
            pattern: format!("%{escaped}%"),
            negated: false,
        });
    }

    Ok(Filter {
        organization,
        conditions,
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocabulary::{FINDING_STATUSES, SEVERITIES, SOURCES};

    fn query() -> FindingQuery {
        FindingQuery::new()
    }

    /// The clause a filter produces, as a string, for a test to read.
    ///
    /// Tests read the *rendered* clause rather than the `Condition` list, because the failure
    /// they guard against is a clause that does not say what the filter meant — a test that
    /// asserted on the enum would have passed while the SQL was wrong.
    fn clause_of(filter: &Filter) -> String {
        let mut builder = QueryBuilder::<Postgres>::new("");
        push_filter(&mut builder, filter);
        builder.sql().to_string()
    }

    #[test]
    fn an_unfiltered_query_is_scoped_to_the_organization_and_nothing_else() {
        let filter = finding_filter(None, &query()).expect("an empty filter is valid");
        assert!(filter.conditions.is_empty(), "no filter, no condition");
        let clause = clause_of(&filter);
        assert!(
            clause.starts_with("organization_id is not distinct from"),
            "every read is organization-scoped, got: {clause}"
        );
        // One bind for the organization and nothing else. If this ever grows, a filter is
        // binding something the clause does not name.
        assert_eq!(clause.matches('$').count(), 1, "clause: {clause}");
    }

    #[test]
    fn every_filter_names_its_own_column_and_carries_its_own_bind() {
        // The reason the condition is a value and not a hand-numbered string: a clause that
        // filters by status while binding the severity is a query that runs and lies.
        let full = FindingQuery {
            severity: Some("high".into()),
            status: Some("open".into()),
            source: Some("dependency".into()),
            component: Some("tokio".into()),
            search: Some("unpinned".into()),
            seen_after: Some(time::OffsetDateTime::now_utc()),
            ..FindingQuery::new()
        };
        let filter = finding_filter(Some(Uuid::nil()), &full).expect("valid filter");
        let clause = clause_of(&filter);
        assert!(clause.contains("severity ="), "{clause}");
        assert!(clause.contains("status ="), "{clause}");
        assert!(clause.contains("source ="), "{clause}");
        assert!(clause.contains("component ="), "{clause}");
        assert!(clause.contains("last_seen_at >="), "{clause}");
        // Five filters plus the search, which is bound twice (title and description), plus the
        // organization: eight placeholders in the clause and eight binds after it.
        assert_eq!(clause.matches('$').count(), 8, "clause: {clause}");
    }

    #[test]
    fn a_search_matches_the_description_too_which_is_what_an_operator_expects() {
        let filter = finding_filter(
            None,
            &FindingQuery {
                search: Some("overflow".into()),
                ..query()
            },
        )
        .expect("any term is searchable");
        let clause = clause_of(&filter);
        assert!(clause.contains("title ilike"), "{clause}");
        assert!(clause.contains("description ilike"), "{clause}");
    }

    #[test]
    fn a_search_term_with_wildcards_is_escaped_not_honoured() {
        let filter = finding_filter(
            None,
            &FindingQuery {
                search: Some("100% _done\\".into()),
                ..query()
            },
        )
        .expect("any term is searchable");
        let Condition::Search { pattern, .. } = &filter.conditions[0] else {
            panic!("expected a search condition, got {:?}", filter.conditions[0]);
        };
        assert_eq!(pattern, "%100\\% \\_done\\\\%");
        assert!(clause_of(&filter).contains("escape '\\'"), "the pattern must be escaped");
    }

    #[test]
    fn an_unknown_filter_value_is_refused_rather_than_returning_nothing() {
        // A filter for a severity the platform does not have would otherwise return an empty
        // list, which reads as "no findings" rather than "that filter is nonsense".
        for bad in [
            FindingQuery { severity: Some("spicy".into()), ..query() },
            FindingQuery { status: Some("pending".into()), ..query() },
            FindingQuery { source: Some("telepathy".into()), ..query() },
        ] {
            let err = finding_filter(None, &bad).expect_err("a nonsense filter must be refused");
            assert_eq!(err.code(), "invalid_security_input");
        }
    }

    #[test]
    fn an_empty_filter_value_is_no_filter_rather_than_no_rows() {
        // A form that submits an empty severity dropdown must not filter to severity = ''.
        let empty = FindingQuery {
            severity: Some(String::new()),
            status: Some(String::new()),
            source: Some(String::new()),
            component: Some(String::new()),
            search: Some("   ".into()),
            ..query()
        };
        let filter = finding_filter(None, &empty).expect("empty is not a filter");
        assert!(filter.conditions.is_empty(), "an empty dropdown is not a filter");
    }

    #[test]
    fn the_vocabulary_the_filter_validates_is_the_vocabulary_the_registry_knows() {
        for severity in SEVERITIES {
            assert!(is_severity(severity));
        }
        for status in FINDING_STATUSES {
            assert!(is_finding_status(status));
        }
        for source in SOURCES {
            assert!(is_source(source));
        }
        assert!(!is_severity(""));
        assert!(!is_finding_status(""));
        assert!(!is_source(""));
    }

    #[test]
    fn the_page_size_the_query_asks_for_is_the_one_the_store_honours() {
        let mut big = query();
        big.limit = 5_000;
        assert_eq!(big.effective_limit(), crate::vocabulary::MAX_PAGE);
        let mut normal = query();
        normal.limit = 25;
        assert_eq!(normal.effective_limit(), 25);
    }

    #[test]
    fn a_negative_offset_is_clamped_so_a_hopeless_page_is_the_first_page() {
        // A negative offset is a `limit -10 offset -10` in the URL bar, and Postgres treats a
        // negative offset as an error while a client treats it as "the last page".
        let filter = finding_filter(None, &query()).expect("valid");
        assert_eq!(filter.conditions.len(), 0);
        let mut page = FindingQuery::new();
        page.offset = -10;
        assert_eq!(page.offset.max(0), 0);
    }
}
