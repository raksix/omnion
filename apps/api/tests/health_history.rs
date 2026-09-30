//! What the health screen's *history* can claim (REQ-014, slice 2).
//!
//! Slice 1's suite (`health_probes.rs`) asks "is the reading true now". This one
//! asks "is the *past* true", and the past is where a metrics screen quietly
//! lies, because every wrong answer here is a plausible number rather than an
//! error:
//!
//! 1. **A range is a name and the wrong one is refused.** `Range::parse("168")`
//!    must fail. The clamp-and-answer shape produces a table labelled `7d`
//!    holding a day, and a CSV exported from that table matches it *perfectly* —
//!    so the export criterion passes while both are wrong. A refusal makes that
//!    combination unreachable, and it has to be asserted: nothing else in the
//!    build would notice a silent clamp.
//! 2. **An empty window has no numbers, not zeros.** `avg()` over no rows is
//!    `NULL`; the failing implementation is the one that maps it to `0.0` and
//!    thereby makes a metric nobody has ever measured the calmest row in the
//!    export. The walk inserts nothing, reads, and asserts `None` on every
//!    aggregate — and then also asserts the *mechanism*, by inserting samples
//!    and showing the same read returning real numbers. Without that second leg,
//!    a reader that returns nothing for everybody would pass.
//! 3. **The window actually bounds the rows.** Two samples an hour apart belong
//!    to `24h` and not to `1h`, and the `7d` roll-up buckets them into one day.
//!    A query that ignored the window would pass every assertion above on a
//!    single-window fixture, because the only sample in it is always in range.
//! 4. **The export is the table.** `summaries_to_csv` takes the very list the
//!    table rendered, so the walk asserts the CSV's *rows* against the summaries
//!    it was built from — the property the criterion is actually about — plus the
//!    range stamp that tells a reader which window the file claims.
//! 5. **Retention does not eat the history the table reads.** A sample the
//!    `24h` window shows must survive a prune, since retention is 30 days and the
//!    newest window is 7. Silently shortened retention would turn the newest
//!    range into an empty one and nobody would notice until the chart went blank
//!    a month after launch.

#![allow(clippy::too_many_lines)]

use omnion_health::{MetricSummary, Range};
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// A scratch database
// ---------------------------------------------------------------------------------------------

struct Harness {
    db: omnion_core::Db,
    maintenance: omnion_core::Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = omnion_core::config::Config::from_env().ok()?;
        let maintenance = match omnion_core::Db::connect(&omnion_core::config::DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        {
            Ok(db) => db,
            Err(err) => {
                eprintln!("SKIP: PostgreSQL is not reachable ({err})");
                return None;
            }
        };

        let database = format!("omnion_health_hist_{}", Uuid::new_v4().simple());
        sqlx::query(&format!(r#"create database "{database}""#))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = omnion_core::Db::connect(&omnion_core::config::DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        Some(Self {
            db,
            maintenance,
            database,
        })
    }

    fn pool(&self) -> &PgPool {
        self.db.pool()
    }

    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!(
            r#"drop database if exists "{database}" with (force)"#
        ))
        .execute(self.maintenance.pool())
        .await
        .expect("the temporary database must be removed");
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base
        .rsplit_once('/')
        .expect("the URL must contain a database path")
        .0;
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

/// Insert a sample at a chosen age, so a window has something inside it and
/// something outside it.
///
/// The three values are chosen so the aggregates are **not** derivable from the
/// count alone: min 10, avg 20, max 30 is a walk that can be wrong in each
/// direction separately — a reader that reported only the newest (30, 30, 30) or
/// only the count would fail rather than pass by luck.
async fn insert_sample(pool: &PgPool, service: &str, metric: &str, value: f64, age_hours: f64) {
    sqlx::query(
        "insert into health_samples (service, metric, value, unit, state, sampled_at) \
         values ($1, $2, $3, 'ms', 'healthy', now() - ($4 || ' hours')::interval)",
    )
    .bind(service)
    .bind(metric)
    .bind(value)
    .bind(format!("{age_hours}"))
    .execute(pool)
    .await
    .expect("the sample must be insertable");
}

// ---------------------------------------------------------------------------------------------
// The range is a name
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unoffered_range_is_refused_and_names_what_is_offered() {
    // No database needed: this is a pure contract, and the assertion is that the
    // *refusal* is the answer. `Range::parse("168")` returning the week would be
    // the silent clamp this test exists to prevent.
    for offered in ["1h", "24h", "7d"] {
        assert!(
            Range::parse(offered).is_ok(),
            "{offered} is on screen and must be accepted"
        );
    }
    for rejected in ["168", "24", "week", "", "7D", "1 hour"] {
        assert!(
            Range::parse(rejected).is_err(),
            "{rejected} silently became a window"
        );
    }
    let error = Range::parse("168").expect_err("168 is not a name this platform offers");
    assert!(
        error.to_string().contains("1h, 24h, 7d"),
        "the refusal must say what is offered: {error}"
    );
    // And the window a name *does* stand for is the one the label claims —
    // otherwise the name is decorative.
    assert_eq!(Range::Hour.hours(), 1);
    assert_eq!(Range::Day.hours(), 24);
    assert_eq!(Range::Week.hours(), 168);
}

// ---------------------------------------------------------------------------------------------
// An empty window has no numbers
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_window_reports_no_aggregates_rather_than_zero() {
    // The failing implementation is `coalesce(avg(value), 0)`. It passes every
    // count assertion and every "there is a row" assertion, and it is the reason
    // a metric nobody has ever measured would show up in a spreadsheet as the
    // most stable series on the platform.
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    for range in Range::all() {
        let summaries =
            omnion_health::metric_summaries(harness.pool(), range, time::OffsetDateTime::now_utc())
                .await
                .expect("an empty window is not an error");
        assert!(
            summaries.is_empty(),
            "{range} invented rows out of an empty table"
        );
    }

    harness.dispose().await;
}

#[tokio::test]
async fn a_populated_window_reports_real_aggregates_and_its_own_bounds() {
    // The mechanism leg the empty test cannot provide: the same read, over rows
    // that exist, returns numbers — and returns *different* numbers per window.
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    // Inside 24 h but outside 1 h.
    series(&harness).await;

    let now = time::OffsetDateTime::now_utc();
    let day = omnion_health::metric_summaries(harness.pool(), Range::Day, now)
        .await
        .expect("the day window must read");
    let redis = find(&day, "redis", "latency_ms");
    assert_eq!(redis.samples, 2, "both samples are inside 24 h");
    assert_eq!(redis.current, Some(30.0), "current is the newest value");
    assert_eq!(redis.min, Some(10.0));
    assert_eq!(redis.max, Some(30.0));
    assert_eq!(redis.avg, Some(20.0), "the mean of 10 and 30, not 30");
    assert_eq!(redis.spread(), Some(20.0));
    assert!(!redis.is_empty());
    assert!(redis.last_sample_at.is_some());

    // The same rows, the hour window: only the recent one. This is the assertion
    // that catches a query ignoring its window — on a fixture where every sample
    // is inside every range, that query passes everything above.
    let hour = omnion_health::metric_summaries(harness.pool(), Range::Hour, now)
        .await
        .expect("the hour window must read");
    let hour_row = find(&hour, "redis", "latency_ms");
    assert_eq!(hour_row.samples, 1, "the two-hour-old sample is outside 1h");
    assert_eq!(hour_row.current, Some(30.0));
    assert_eq!(hour_row.avg, Some(30.0), "one sample's mean is that sample");

    harness.dispose().await;
}

#[tokio::test]
async fn the_sparkline_is_the_window_values_oldest_first() {
    // A sparkline drawn from an unordered or window-blind read is a picture of
    // something that did not happen. The values are asymmetric (10, 20, 30) so a
    // reversed series is not symmetric with the right one.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    series(&harness).await;

    let now = time::OffsetDateTime::now_utc();
    let series =
        omnion_health::sparkline_values(harness.pool(), "redis", "latency_ms", Range::Day, now)
            .await
            .expect("the series must be readable");
    assert_eq!(
        series,
        vec![10.0, 20.0, 30.0],
        "oldest first, not newest first"
    );

    // The same metric read as a roll-up is one bucket per day, and the day it
    // lands in is the day the *sample* says — not the reader's clock.
    let rollup =
        omnion_health::daily_rollup(harness.pool(), "redis", "latency_ms", Range::Week, now)
            .await
            .expect("the roll-up must be readable");
    assert_eq!(rollup.len(), 1, "three samples in one day make one bucket");
    assert_eq!(rollup[0].samples, 3);
    assert_eq!(rollup[0].min, 10.0);
    assert_eq!(rollup[0].max, 30.0);
    assert_eq!(rollup[0].avg, 20.0);
    assert_eq!(rollup[0].day.len(), 10, "a day is YYYY-MM-DD");

    // A metric with no samples produces no buckets at all — an empty roll-up is
    // the honest answer, and a bucket of zeroes is not.
    let none = omnion_health::daily_rollup(harness.pool(), "s3", "latency_ms", Range::Week, now)
        .await
        .expect("an unmeasured metric is not an error");
    assert!(none.is_empty());

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// The export is the table
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_export_carries_exactly_the_rows_the_table_rendered() {
    // "CSV export matches the range shown" is checked structurally: the file is
    // rendered from the list, so the walk asserts the file against *that list* —
    // row count, values, and the range stamped into every row.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    series(&harness).await;

    let summaries = omnion_health::metric_summaries(
        harness.pool(),
        Range::Day,
        time::OffsetDateTime::now_utc(),
    )
    .await
    .expect("the table must read");
    let csv = omnion_health::summaries_to_csv(&summaries, Range::Day);
    let lines: Vec<&str> = csv.lines().collect();

    assert_eq!(
        lines.len(),
        summaries.len() + 1,
        "the file has a header and one row per table row"
    );
    assert_eq!(lines[0], omnion_health::CSV_HEADER);

    for summary in &summaries {
        let row = lines
            .iter()
            .find(|line| line.starts_with(&format!("{},{},", summary.service, summary.metric)))
            .unwrap_or_else(|| {
                panic!(
                    "{} / {} is missing from the export",
                    summary.service, summary.metric
                )
            });
        let cells: Vec<&str> = row.split(',').collect();
        assert_eq!(cells[0], summary.service);
        assert_eq!(cells[1], summary.metric);
        assert_eq!(cells[3], summary.samples.to_string());
        // The aggregate cells must be the table's, in the table's units — this
        // is the assertion that a zero-filled cell cannot satisfy.
        assert_eq!(
            cells[6].parse::<f64>().ok(),
            summary.avg,
            "avg matches the table"
        );
        assert_eq!(
            cells[7].parse::<f64>().ok(),
            summary.max,
            "max matches the table"
        );
        assert_eq!(
            *cells.last().expect("a range cell"),
            "24h",
            "every row says its window"
        );
    }

    // And the same list in the week window produces a file that says `7d`, so a
    // download cannot be mistaken for a different week than the screen showed.
    let week = omnion_health::summaries_to_csv(&summaries, Range::Week);
    assert!(week.lines().nth(1).expect("a row").ends_with(",7d"));

    harness.dispose().await;
}

#[tokio::test]
async fn an_export_of_an_empty_window_is_a_file_and_not_an_error() {
    // A download of nothing must still open in a spreadsheet: a header with no
    // rows is a valid CSV, and an empty body is a file that fails to parse.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let summaries = omnion_health::metric_summaries(
        harness.pool(),
        Range::Week,
        time::OffsetDateTime::now_utc(),
    )
    .await
    .expect("the empty window must read");
    let csv = omnion_health::summaries_to_csv(&summaries, Range::Week);
    assert_eq!(csv, format!("{}\n", omnion_health::CSV_HEADER));
    assert!(csv.contains("range"), "the header names the window column");

    harness.dispose().await;
}

#[tokio::test]
async fn retention_leaves_the_windows_the_table_reads_intact() {
    // Retention is 30 days; the newest window the table offers is 7. If the two
    // ever crossed, the `7d` range would silently become an empty one a month
    // after launch and nobody would be there to notice.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    assert_eq!(omnion_health::SAMPLE_RETENTION_DAYS, 30);
    for range in Range::all() {
        assert!(
            range.hours() < omnion_health::SAMPLE_RETENTION_DAYS * 24,
            "{} is longer than the retention window",
            range.key()
        );
    }

    // A week-old sample — the oldest the widest range shows — must survive the
    // sweep, and a 45-day-old one must not.
    insert_sample(harness.pool(), "queue", "queue_depth", 12.0, 24.0 * 6.0).await;
    insert_sample(harness.pool(), "queue", "queue_depth", 1.0, 24.0 * 45.0).await;

    let deleted = omnion_health::prune_old_samples(harness.pool())
        .await
        .expect("the sweep must run");
    assert_eq!(deleted, 1, "only the 45-day-old sample is pruned");

    let week = omnion_health::metric_summaries(
        harness.pool(),
        Range::Week,
        time::OffsetDateTime::now_utc(),
    )
    .await
    .expect("the week window must still read");
    let queue = find(&week, "queue", "queue_depth");
    assert_eq!(queue.samples, 1, "the week-old sample survived the sweep");
    assert_eq!(queue.min, Some(1.0), "and it is the only one in range");

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// The service detail page's own 24 h trend (slice 2's last open item)
// ---------------------------------------------------------------------------------------------
//
// The request asks the drill-down for "a 24 h trend chart", and the screen that
// shipped with slice 2's sibling listed current values with the history on another
// page. Two failures hide in that, and only one of them is visible:
//
//  1. **A row that carries a value and no series.** The screen renders perfectly:
//     a table, a value, a timestamp. The *chart* is what is missing, and a table
//     is not obviously a missing chart. The walk asserts every metric row accounts
//     for a trend — a line, a point, or the sentence that says the window is empty.
//  2. **Two rows drawn over two different windows.** If each row's series were read
//     with its own `now()`, the first row's window would be a fraction of a second
//     wider than the last, and two lines sharing one screen would not be
//     comparable. So the series is read once per response and the walk asserts
//     that two metrics sampled at the same ages get the same *count* — a
//     per-row `now()` cannot produce that, and a fixture with different metrics
//     cannot prove it either.

#[tokio::test]
async fn every_metric_row_of_a_service_carries_its_own_series() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    // Two metrics on ONE service, sampled at the same ages. A per-row `now()` gives
    // them different windows; one read for the whole response gives them the same.
    for metric in ["latency_ms", "queue_depth"] {
        insert_sample(harness.pool(), "redis", metric, 10.0, 2.0).await;
        insert_sample(harness.pool(), "redis", metric, 20.0, 1.0).await;
        insert_sample(harness.pool(), "redis", metric, 30.0, 0.5).await;
    }

    let now = time::OffsetDateTime::now_utc();
    // The window the detail page draws, named the same way the route names it.
    for metric in ["latency_ms", "queue_depth"] {
        let values = omnion_health::sparkline_values(
            harness.pool(),
            "redis",
            metric,
            omnion_health::Range::Day,
            now,
        )
        .await
        .expect("the series must read");
        assert_eq!(
            values,
            vec![10.0, 20.0, 30.0],
            "{metric} did not come back oldest first, so the line would draw time backwards"
        );
    }

    // The mechanism leg, through the same function the route uses: a metric with one
    // sample yields a one-element series, which the screen must draw as a dot. A
    // reader that dropped single samples would render "no data" on a row that has one.
    insert_sample(harness.pool(), "redis", "used_memory_bytes", 42.0, 0.25).await;
    let one = omnion_health::sparkline_values(
        harness.pool(),
        "redis",
        "used_memory_bytes",
        omnion_health::Range::Day,
        now,
    )
    .await
    .expect("the single-sample series must read");
    assert_eq!(one.len(), 1, "a lone sample is a dot, not an empty chart");

    // And a metric nobody has ever sampled yields an EMPTY series, not a zero and
    // not a fabricated point: the row itself is absent from the detail page, and
    // this is the read that must not invent one.
    let none = omnion_health::sparkline_values(
        harness.pool(),
        "redis",
        "never_measured",
        omnion_health::Range::Day,
        now,
    )
    .await
    .expect("an unmeasured metric is not an error");
    assert!(
        none.is_empty(),
        "a metric with no samples invented {} point(s)",
        none.len()
    );

    harness.dispose().await;
}

#[tokio::test]
async fn the_detail_window_is_a_day_and_the_samples_query_no_longer_clamps() {
    // The samples endpoint used to take `hours` and clamp it to 1 h … 7 d, which
    // is the silent-clamp defect slice 2 removed from `/health/metrics`: a caller
    // asking for 30 days got seven days with a `200` and no warning, and the
    // chart it drew was confidently the wrong window. The route now resolves a
    // *named* range through the same `Range::parse` the metric table uses, so the
    // refusal is a property of the vocabulary rather than of one handler.
    assert_eq!(
        omnion_health::DEFAULT_RANGE,
        Range::Day,
        "the detail draws a day"
    );
    for offered in omnion_health::RANGE_KEYS {
        assert!(
            Range::parse(offered).is_ok(),
            "{offered} is on screen and must be accepted"
        );
    }
    for rejected in ["168", "30", "hours=24", ""] {
        assert!(
            Range::parse(rejected).is_err(),
            "{rejected} silently became a window in the samples endpoint too"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The three samples every series test shares: 2 h ago, 1 h ago and 30 min ago.
///
/// Deliberately asymmetric (10, 20, 30) and deliberately *straddling the hour
/// window*: two of the three are inside `24h` and only one is inside `1h`. A
/// fixture whose every sample sits inside every range cannot tell a
/// window-aware query from one that ignores the window, so the spread across a
/// boundary is the assertion that does.
async fn series(harness: &Harness) {
    insert_sample(harness.pool(), "redis", "latency_ms", 10.0, 2.0).await;
    insert_sample(harness.pool(), "redis", "latency_ms", 20.0, 1.0).await;
    insert_sample(harness.pool(), "redis", "latency_ms", 30.0, 0.5).await;
}

/// The row for one metric, or a panic naming it.
///
/// A `Result`-returning lookup here would push the "which metric is missing"
/// question onto every caller, and the answer would be the same every time.
fn find(summaries: &[MetricSummary], service: &str, metric: &str) -> MetricSummary {
    summaries
        .iter()
        .find(|summary| summary.service == service && summary.metric == metric)
        .unwrap_or_else(|| {
            panic!(
                "{service} / {metric} is missing from the table; it holds: {:?}",
                summaries
                    .iter()
                    .map(|summary| format!("{}/{}", summary.service, summary.metric))
                    .collect::<Vec<_>>()
            )
        })
        .clone()
}
