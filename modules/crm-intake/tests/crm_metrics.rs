//! The organization's counters answer about *every* lead — and the median is absence, not zero.
//!
//! Run through `scripts/qa/run-crm-metrics.sh`, which creates the database these tests need.
//!
//! ## The gap this file exists to measure
//!
//! `GET /api/v1/crm/leads/metrics` has been in REQ-117's own API table since the module
//! shipped, described as *"inbox metrics (breached, unassigned, **median response**)"*, and no
//! handler and no store function answered it. `LeadMetrics` had six counters and none of them
//! was a response time, so the one number in that sentence that is not derivable from a status
//! count had nowhere to live.
//!
//! **The counters already worked and that is what hid this.** The inbox list returns a
//! `metrics` object with the same six fields, and every gate that touched the CRM read one. So
//! the surface looked covered, and the REQ line reads as satisfied by a neighbour. It is the
//! signature defect this module has now written down seven times, in its narrowest form: not a
//! missing caller but a missing *fact*, with a similar-looking fact sitting right beside it.
//!
//! ## Why the endpoint is not `list_leads` with an empty filter
//!
//! That is the shape of the bug this gate exists to prevent, so it is asserted rather than
//! assumed. `list_leads` returns one page (its limit clamps to 51) and counts *that page*,
//! deliberately: the inbox's counters must describe the rows on screen, or "3 new leads" sits
//! above a table of five. An endpoint that answered "every lead the organization holds" by
//! delegating to it would cap itself at fifty and call the result total. The test writes
//! **sixty** answered leads — more than one page — and the metric has to count all sixty.
//!
//! ## The median's two failure modes, both here
//!
//! 1. **Zero instead of `None`.** An organization that has answered nothing would render
//!    "0 min" — the fastest team in the installation, and a claim about a fact nobody has.
//! 2. **The mean.** One forgotten lead drags an average under a number nobody can act on,
//!    which is why the request names a median. The fixture is built so the mean and the
//!    median differ by more than an hour, so asserting one cannot pass for the other.

use omnion_module_crm_intake::model::LeadMetrics;
use omnion_module_crm_intake::store;
use omnion_module_crm_intake::NewIntakeSource;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (the shell script sets it)");
    PgPool::connect(&url).await.expect("the QA database")
}

async fn fresh_org(pool: &PgPool, label: &str) -> Uuid {
    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind(format!("{label} {}", org))
        .bind(format!("{label}-{}", org.simple()))
        .execute(pool)
        .await
        .expect("an organization for the test");
    org
}

async fn cleanup(pool: &PgPool, org: Uuid) {
    for sql in [
        "delete from crm_lead_events where lead_id in (select id from crm_leads where organization_id = $1)",
        "delete from crm_lead_submissions where source_id in (select id from crm_intake_sources where organization_id = $1)",
        "delete from crm_leads where organization_id = $1",
        "delete from crm_intake_sources where organization_id = $1",
        "delete from crm_assignment_rules where organization_id = $1",
        "delete from crm_sla_policies where organization_id = $1",
    ] {
        let _ = sqlx::query(sql).bind(org).execute(pool).await;
    }
    let _ = sqlx::query("delete from organizations where id = $1")
        .bind(org)
        .execute(pool)
        .await;
}

async fn endpoint_source(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    store::create_source(pool, &NewIntakeSource::endpoint(org, name, None))
        .await
        .expect("a keyed source")
        .0
        .id
}

/// A lead received `ago_minutes` ago and answered `answered_after_minutes` after that.
///
/// Both instants are bound parameters rather than interpolated — this branch has been bitten
/// three times by a timestamp shifted by the box's offset and read each time as a product bug.
async fn answered_lead(
    pool: &PgPool,
    org: Uuid,
    source: Uuid,
    ago_minutes: i64,
    answered_after_minutes: Option<i64>,
) -> Uuid {
    let received = OffsetDateTime::now_utc() - time::Duration::minutes(ago_minutes);
    let answered = answered_after_minutes.map(|m| received + time::Duration::minutes(m));
    sqlx::query_scalar(
        "insert into crm_leads \
         (organization_id, source_id, status, email, first_name, received_at, first_response_at) \
         values ($1, $2, 'contacted', $3, 'Gate', $4, $5) returning id",
    )
    .bind(org)
    .bind(source)
    .bind(format!("gate-{}@example.invalid", Uuid::new_v4()))
    .bind(received)
    .bind(answered)
    .fetch_one(pool)
    .await
    .expect("a lead")
}

/**
 * An organization with nothing answered reports **no** median, and every counter is zero.
 *
 * This is the assertion that a `coalesce(…, 0)` in the handler would fail, and it is the first
 * one to run because it is the state a fresh installation is actually in — the one a customer
 * sees on the first day and the one most likely to be reached by a shortcut that makes the
 * other queries simpler.
 */
#[tokio::test]
async fn an_organization_with_no_answered_lead_has_no_median() {
    let pool = pool().await;
    let org = fresh_org(&pool, "metrics-empty").await;
    let source = endpoint_source(&pool, org, "empty").await;
    // An open, unanswered lead — the one row that a naive `avg(first_response_at - received_at)`
    // would count as zero.
    answered_lead(&pool, org, source, 30, None).await;

    let metrics = store::organization_metrics(&pool, org).await.expect("the metrics");

    assert_eq!(
        metrics.median_response_minutes, None,
        "an unanswered lead has no response time, and zero is a different claim"
    );
    assert_eq!(metrics.open, 1, "the unanswered lead is open work");
    assert_eq!(metrics.converted, 0);
    cleanup(&pool, org).await;
}

/// An even number of answers takes the average of the two middle ones.
#[tokio::test]
async fn an_even_number_of_answers_averages_the_two_middle_ones() {
    let pool = pool().await;
    let org = fresh_org(&pool, "metrics-even").await;
    let source = endpoint_source(&pool, org, "even").await;
    // 10, 20, 30, 40 minutes. The median is 25. The mean is also 25, so this fixture alone
    // cannot tell a mean from a median — which is exactly why the next test does not use four
    // values.
    for answer in [10, 20, 30, 40] {
        answered_lead(&pool, org, source, 100, Some(answer)).await;
    }

    let metrics = store::organization_metrics(&pool, org).await.expect("the metrics");
    assert_eq!(metrics.median_response_minutes, Some(25));
    cleanup(&pool, org).await;
}

/**
 * The median is not the mean, and one forgotten lead is the difference.
 *
 * Four answers: 5, 10, 15 and **1440** minutes. The median is 12 (floored from 12.5); the mean
 * is 367. A dashboard that shows the mean here is reporting a quarter of an hour as a full
 * day, and the two are separated by enough that no rounding can hide the difference.
 *
 * The floor is asserted rather than the exact average because it is a decision: three minutes
 * is a truthful median of five and seven, and rounding *up* would let a service report itself
 * faster than it was.
 */
#[tokio::test]
async fn one_forgotten_lead_does_not_move_the_median_to_the_mean() {
    let pool = pool().await;
    let org = fresh_org(&pool, "metrics-skew").await;
    let source = endpoint_source(&pool, org, "skew").await;
    for answer in [5, 10, 15, 1440] {
        answered_lead(&pool, org, source, 2000, Some(answer)).await;
    }

    let metrics = store::organization_metrics(&pool, org).await.expect("the metrics");
    assert_eq!(
        metrics.median_response_minutes,
        Some(12),
        "the mean of these four is 367 minutes; the median is not allowed to be it"
    );
    cleanup(&pool, org).await;
}

/**
 * The endpoint counts every lead, not one page of them.
 *
 * Sixty-one answered leads — one more than `MAX_PAGE`, so an implementation that delegated to
 * the inbox list could not match this by accident. The mean and the median are equal here on
 * purpose (every lead answered after 30 minutes), so this fixture isolates **the count** and
 * leaves the arithmetic to the two tests above.
 */
#[tokio::test]
async fn the_counters_cover_every_lead_and_not_one_page() {
    let pool = pool().await;
    let org = fresh_org(&pool, "metrics-volume").await;
    let source = endpoint_source(&pool, org, "volume").await;
    for _ in 0..61 {
        answered_lead(&pool, org, source, 90, Some(30)).await;
    }

    let metrics = store::organization_metrics(&pool, org).await.expect("the metrics");
    assert_eq!(
        metrics.open, 61,
        "an inbox page is 51 rows; a metric that answered 51 was counting a page"
    );
    assert_eq!(metrics.median_response_minutes, Some(30));
    cleanup(&pool, org).await;
}

/** Another organization's leads are not in the answer — the tenancy the endpoint has to keep.
 *
 *  The endpoint is `crm.leads.read`, and a reader of one tenant is exactly the account that
 *  could be handed a number describing another tenant's business if the organization came from
 *  anywhere but the session. A store function takes the organization as an argument, so this
 *  can only prove the *filter*; the guard that puts the session's own organization there is
 *  `run-crm-tenancy-http.sh`'s subject and is not re-litigated here.
 */
#[tokio::test]
async fn another_organizations_answers_are_not_counted_in_this_one() {
    let pool = pool().await;
    let mine = fresh_org(&pool, "metrics-mine").await;
    let theirs = fresh_org(&pool, "metrics-theirs").await;
    let my_source = endpoint_source(&pool, mine, "mine").await;
    let their_source = endpoint_source(&pool, theirs, "theirs").await;

    for answer in [10, 20] {
        answered_lead(&pool, mine, my_source, 120, Some(answer)).await;
    }
    // A neighbour answering in one minute. Read as the fastest team in the world if tenancy
    // were not doing its job.
    answered_lead(&pool, theirs, their_source, 120, Some(1)).await;

    let metrics = store::organization_metrics(&pool, mine).await.expect("the metrics");
    assert_eq!(
        metrics.median_response_minutes,
        Some(15),
        "a neighbour's one-minute answer is not this organization's median"
    );
    cleanup(&pool, mine).await;
    cleanup(&pool, theirs).await;
}

/**
 * The SQL answer and the pure function agree.
 *
 * The store computes the median in SQL (`percentile_cont`, the continuous percentile, which
 * averages the middle two on an even count) while [`LeadMetrics::median_of`] does the same
 * arithmetic in Rust. Two spellings of one definition is a thing to hold on purpose and a
 * thing to *check*, because the day they disagree the panel starts reporting a number nobody
 * wrote. Both arms are asserted here — an odd count and an even one — which is the minimum
 * that can distinguish them.
 */
#[tokio::test]
async fn the_sql_median_and_the_rust_one_agree_on_both_parities() {
    let pool = pool().await;
    let org = fresh_org(&pool, "metrics-parity").await;
    let source = endpoint_source(&pool, org, "parity").await;

    // Odd: 11, 20, 33 → the middle element is 20, and a "percentile_disc" (which takes a
    // whole row rather than interpolating) would still say 20, so the even case is the one
    // that matters.
    let odd_samples = vec![11, 20, 33];
    for answer in odd_samples.clone() {
        answered_lead(&pool, org, source, 200, Some(answer)).await;
    }
    let metrics = store::organization_metrics(&pool, org).await.expect("the metrics");
    assert_eq!(
        metrics.median_response_minutes,
        LeadMetrics::median_of(odd_samples),
        "three samples must agree between SQL and Rust"
    );
    cleanup(&pool, org).await;

    // Even: 10 and 35 → 22 after flooring. `percentile_disc` would say 35, so this is the
    // assertion that names the difference between a median and "pick a row".
    let org2 = fresh_org(&pool, "metrics-parity-even").await;
    let source2 = endpoint_source(&pool, org2, "parity-even").await;
    let even_samples = vec![10, 35];
    for answer in even_samples.clone() {
        answered_lead(&pool, org2, source2, 200, Some(answer)).await;
    }
    let metrics2 = store::organization_metrics(&pool, org2).await.expect("the metrics");
    assert_eq!(
        metrics2.median_response_minutes,
        LeadMetrics::median_of(even_samples),
        "an even sample count must average the middle two in SQL as well as in Rust"
    );
    assert_eq!(metrics2.median_response_minutes, Some(22));
    cleanup(&pool, org2).await;
}

/**
 * A lead answered *before* it arrived is not in the sample.
 *
 * `first_response_at >= received_at` is the guard, and the row is legal in the database: the
 * columns are two nullable `timestamptz` with no check between them, so a clock that went
 * backwards — or an integration writing one row — can produce a negative interval. Excluded
 * rather than clamped to zero, because "answered in no time at all" and "answered before the
 * submission arrived" are different sentences and only one of them is true.
 */
#[tokio::test]
async fn an_answer_earlier_than_the_submission_is_excluded() {
    let pool = pool().await;
    let org = fresh_org(&pool, "metrics-negative").await;
    let source = endpoint_source(&pool, org, "negative").await;

    answered_lead(&pool, org, source, 300, Some(30)).await;
    // received_at = now, first_response_at = now - 10 minutes: a negative interval.
    sqlx::query(
        "insert into crm_leads \
         (organization_id, source_id, status, email, first_name, received_at, first_response_at) \
         values ($1, $2, 'contacted', $3, 'Gate', $4, $5)",
    )
    .bind(org)
    .bind(source)
    .bind(format!("gate-{}@example.invalid", Uuid::new_v4()))
    .bind(OffsetDateTime::now_utc())
    .bind(OffsetDateTime::now_utc() - time::Duration::minutes(10))
    .execute(&pool)
    .await
    .expect("the impossible row");

    let metrics = store::organization_metrics(&pool, org).await.expect("the metrics");
    assert_eq!(
        metrics.median_response_minutes,
        Some(30),
        "a negative interval must not pull the answer, and must not be counted as zero"
    );
    cleanup(&pool, org).await;
}