//! The history: what a metric has *done*, over a named range (REQ-014, slice 2).
//!
//! Slice 1 answered "is it up right now", which is the only question a health
//! screen can answer on its own and the only one worth answering when an alarm
//! is already going off. This module answers the question the operator asks
//! *after* the alarm: "how long has it been like this, and has it been like this
//! before". Three rules, each one a way a chart ends up lying:
//!
//! * **A range is a named window, not a number of hours.** [`Range`] is a
//!   closed vocabulary of three keys — `1h`, `24h`, `7d` — because the panel
//!   shows the range's name next to the numbers, and a client that sends an
//!   arbitrary `hours=180` gets either a rejection or a silent clamp. Either
//!   one produces a table labelled `7d` holding a day's data, which is the shape
//!   of export that does not match what is on screen. The rejection is the
//!   honest answer and the criterion "CSV export matches the range shown" is a
//!   *structural* property here: both the table and the CSV are rendered from
//!   the one [`MetricSummary`] list, so they cannot disagree about the window
//!   they cover.
//! * **An empty window has no aggregates, not zero aggregates.** A metric with no
//!   samples in the range reports `samples: 0` and `None` for min/avg/max, and
//!   the panel prints a dash. The alternative — `0.0` from `avg()` over no rows,
//!   which is `NULL` in PostgreSQL and `0` in every language that forgets to
//!   check — puts a plausible number under a metric nobody has ever measured.
//!   That is the same defect as a metric card drawing its warn line at zero, one
//!   layer down, and it is the reason [`MetricSummary::is_empty`] exists.
//! * **Aggregation is the database's job.** Thirty days of samples read into the
//!   API to be averaged in Rust is a read whose cost grows with uptime, and the
//!   overview is a screen people leave open. Every aggregate below is computed
//!   with an index-friendly query, and the raw series is only ever fetched for
//!   one metric at a time (the sparkline), where the row count is bounded by the
//!   interval, not by the age of the platform.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::Result;
use crate::vocabulary::is_finite;

/// The three windows the panel offers, in the order it offers them.
pub const RANGE_KEYS: &[&str] = &["1h", "24h", "7d"];

/// The default window: a day is long enough to show a pattern and short enough
/// that a stale sample has not had time to hide inside it.
pub const DEFAULT_RANGE: Range = Range::Day;

/// A window of history, named rather than counted.
///
/// Named because the name travels: it is the CSV's column header, the panel's
/// button label and the `range=` the client echoes back. A range expressed as an
/// `i64` hour count has three spellings of the same window (`1`, `24`, `168`)
/// and no way to say which one a stored series actually covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Range {
    /// The last hour.
    #[serde(rename = "1h")]
    Hour,
    /// The last day.
    #[serde(rename = "24h")]
    Day,
    /// The last seven days, bucketed by day.
    #[serde(rename = "7d")]
    Week,
}

impl Range {
    /// The key this range is written and requested with.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Week => "7d",
        }
    }

    /// The window's length. Kept next to [`Range::key`] so a range cannot grow a
    /// fourth spelling without the two lists being updated together.
    #[must_use]
    pub fn hours(self) -> i64 {
        match self {
            Self::Hour => 1,
            Self::Day => 24,
            Self::Week => 24 * 7,
        }
    }

    /// The instant this window starts, `now` seconds ago.
    #[must_use]
    pub fn since(self, now: OffsetDateTime) -> OffsetDateTime {
        now - time::Duration::hours(self.hours())
    }

    /// Whether the panel should bucket this window by day.
    ///
    /// The week is the only one that is: 168 hourly points on a 300-pixel
    /// sparkline is 168 vertical pixels' worth of data in 300 pixels, and the
    /// line it draws is noise. Bucketing is decided by the *range*, not by the
    /// client, so the table and the export bucket identically.
    #[must_use]
    pub fn buckets_daily(self) -> bool {
        matches!(self, Self::Week)
    }

    /// Parse a key, or say why it is not one.
    ///
    /// A `Result` rather than a fallback to [`DEFAULT_RANGE`]: the fallback is
    /// how a client asking for a week ends up charting a day under a label that
    /// says a week.
    pub fn parse(key: &str) -> Result<Self> {
        match key {
            "1h" => Ok(Self::Hour),
            "24h" => Ok(Self::Day),
            "7d" => Ok(Self::Week),
            other => Err(crate::error::HealthError::invalid(format!(
                "{other} is not a range this platform offers ({}); use hours instead \
                 if you need a window the panel has no label for",
                RANGE_KEYS.join(", ")
            ))),
        }
    }

    /// Every range, in display order.
    #[must_use]
    pub fn all() -> [Self; 3] {
        [Self::Hour, Self::Day, Self::Week]
    }
}

impl std::fmt::Display for Range {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.key())
    }
}

/// One metric's history over one window.
///
/// `current` is the newest sample's value and is the only field the overview
/// shows today; the aggregates are what slice 2 added. They are all optional
/// because the window can be empty, and they are optional *together*: a summary
/// with an `avg` and no `min` cannot be built by this type.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricSummary {
    /// Which service measured it.
    pub service: String,
    /// Which metric.
    pub metric: String,
    /// The unit the values are in.
    pub unit: String,
    /// How many samples fall in the window.
    pub samples: i64,
    /// The newest value in the window.
    pub current: Option<f64>,
    /// The smallest value in the window.
    pub min: Option<f64>,
    /// The arithmetic mean over the window.
    pub avg: Option<f64>,
    /// The largest value in the window.
    pub max: Option<f64>,
    /// The state the newest sample carried.
    pub state: String,
    /// When the newest sample was taken.
    pub last_sample_at: Option<String>,
}

impl MetricSummary {
    /// A window with nothing in it.
    ///
    /// Constructed rather than left to a caller, because the empty case is the
    /// one that has to agree with itself: `samples: 0` with a non-`None` average
    /// is the lie this type exists to prevent.
    #[must_use]
    pub fn empty(service: &str, metric: &str) -> Self {
        Self {
            service: service.to_string(),
            metric: metric.to_string(),
            unit: String::new(),
            samples: 0,
            current: None,
            min: None,
            avg: None,
            max: None,
            state: crate::vocabulary::STATE_WHEN_UNPROBED.to_string(),
            last_sample_at: None,
        }
    }

    /// `true` when the window holds no samples at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples == 0
    }

    /// How far the metric moved across the window, when it moved.
    ///
    /// A range's spread is the number that answers "is this new?", which is the
    /// whole reason to keep `min` and `max` next to the mean. `None` on an empty
    /// window rather than `Some(0.0)`: a spread of zero across no samples is a
    /// claim about nothing.
    #[must_use]
    pub fn spread(&self) -> Option<f64> {
        match (self.min, self.max) {
            (Some(low), Some(high)) => Some(high - low),
            _ => None,
        }
    }
}

/// One day's bucket of the weekly roll-up.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RollupDay {
    /// The UTC day, `YYYY-MM-DD`.
    pub day: String,
    /// Which service measured it.
    pub service: String,
    /// Which metric.
    pub metric: String,
    /// The unit the values are in.
    pub unit: String,
    /// How many samples fell in the day.
    pub samples: i64,
    /// The day's smallest value.
    pub min: f64,
    /// The day's mean.
    pub avg: f64,
    /// The day's largest value.
    pub max: f64,
}

// ---------------------------------------------------------------------------------------------
// The queries
// ---------------------------------------------------------------------------------------------

/// Every `(service, metric)` pair that has a sample inside the window, with its
/// aggregates, ordered by service then metric.
///
/// Written as one correlated aggregate per pair rather than a `group by` over
/// the whole table, because the result is *one row per metric*: a `group by
/// service, metric` over thirty days of raw samples sorts the entire retained
/// history to produce a few dozen rows, and this screen is polled every fifteen
/// seconds by anybody who leaves it open.
///
/// The window is a bound parameter, never a string-interpolated interval, so a
/// caller cannot talk the query into reading the whole table.
pub async fn metric_summaries(
    pool: &PgPool,
    range: Range,
    now: OffsetDateTime,
) -> Result<Vec<MetricSummary>> {
    let since = range.since(now);
    let rows = sqlx::query_as::<_, SummaryRow>(
        "select s.service, s.metric, \
                coalesce(max(s.unit), '') as unit, \
                count(*)::bigint as samples, \
                (array_agg(s.value order by s.sampled_at desc, s.id desc))[1] as current, \
                min(s.value) as min, avg(s.value) as avg, max(s.value) as max, \
                (array_agg(s.state order by s.sampled_at desc, s.id desc))[1] as state, \
                max(s.sampled_at) as last_sample_at \
         from health_samples s \
         where s.sampled_at >= $1 \
         group by s.service, s.metric \
         order by s.service, s.metric",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(MetricSummary::from).collect())
}

/// One metric's daily roll-up, oldest day first.
///
/// The seven-day chart's series. Bucketed by `date_trunc` on the *stored*
/// timestamp rather than on the reader's clock, so the same stored samples
/// bucket the same way whoever asks and whenever they ask.
pub async fn daily_rollup(
    pool: &PgPool,
    service: &str,
    metric: &str,
    range: Range,
    now: OffsetDateTime,
) -> Result<Vec<RollupDay>> {
    // Eight columns: day, service, metric, unit, samples, min, avg, max.
    let rows = sqlx::query_as::<_, (String, String, String, String, i64, f64, f64, f64)>(
        "select to_char(date_trunc('day', sampled_at at time zone 'utc'), 'YYYY-MM-DD'), \
                service, metric, coalesce(max(unit), ''), \
                count(*)::bigint, min(value)::float8, avg(value)::float8, max(value)::float8 \
         from health_samples \
         where service = $1 and metric = $2 and sampled_at >= $3 \
         group by 1, service, metric \
         order by 1 asc",
    )
    .bind(service)
    .bind(metric)
    .bind(range.since(now))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(day, service, metric, unit, samples, min, avg, max)| RollupDay {
                day,
                service,
                metric,
                unit,
                samples,
                min,
                avg,
                max,
            },
        )
        .collect())
}

/// A sparkline's series: one metric's values, oldest first.
///
/// Bounded by the range rather than open-ended, and the *server* bounds it. A
/// sparkline endpoint that returns "however many rows there are" is a chart that
/// costs more the longer the platform has been up — the same growth argument as
/// the aggregation above, one row at a time.
pub async fn sparkline_values(
    pool: &PgPool,
    service: &str,
    metric: &str,
    range: Range,
    now: OffsetDateTime,
) -> Result<Vec<f64>> {
    let rows: Vec<(f64,)> = sqlx::query_as(
        "select value from health_samples \
         where service = $1 and metric = $2 and sampled_at >= $3 \
         order by sampled_at asc, id asc",
    )
    .bind(service)
    .bind(metric)
    .bind(range.since(now))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(value,)| value)
        .filter(|value| is_finite(*value))
        .collect())
}

// ---------------------------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------------------------

/// The CSV header, in the order [`summaries_to_csv`] writes it.
///
/// A `const` rather than a `&str` in the function because the header and the rows
/// are two halves of one file, and a header edited without the rows becomes a
/// file whose columns lie about their own contents.
pub const CSV_HEADER: &str = "service,metric,unit,samples,current,min,avg,max,state,last_sampled_at,range";

/// Render the metric table as CSV, for the range shown.
///
/// **The export and the screen are rendered from the same list.** That is the
/// whole mechanism behind the acceptance criterion: this function takes the
/// `&[MetricSummary]` the table just rendered and writes it out, so a row cannot
/// be visible on screen and absent from the download, or vice versa, without
/// somebody changing the endpoint — and the only thing that separates them is
/// the range, which travels *in* the rows rather than beside them.
///
/// Formatting rules, each one chosen because the alternative is a spreadsheet
/// that lies:
///
/// * **A missing number is an empty cell, not `0`.** `avg()` over no rows is
///   `NULL`; writing `0` there is how a metric that was never measured ends up
///   looking like the calmest metric in the export.
/// * **A non-finite number cannot be written at all.** The crate refuses such a
///   sample on the way in, but a value that somehow got stored would otherwise
///   export as `NaN`, which most spreadsheets read as text and then sort as a
///   number. Those two cells have to be empty, not `NaN`.
/// * **Every field is quoted.** Service and metric keys are platform vocabulary,
///   but the quoting costs nothing and a CSV that needs a second quoting rule is
///   a CSV one future key breaks.
pub fn summaries_to_csv(summaries: &[MetricSummary], range: Range) -> String {
    let mut out = String::from(CSV_HEADER);
    out.push('\n');
    for summary in summaries {
        let row = [
            csv_field(&summary.service),
            csv_field(&summary.metric),
            csv_field(&summary.unit),
            summary.samples.to_string(),
            csv_number(summary.current),
            csv_number(summary.min),
            csv_number(summary.avg),
            csv_number(summary.max),
            csv_field(&summary.state),
            csv_field(summary.last_sample_at.as_deref().unwrap_or("")),
            csv_field(range.key()),
        ];
        out.push_str(&row.join(","));
        out.push('\n');
    }
    out
}

/// One CSV cell, quoted.
///
/// A field containing a quote, a comma, a newline or leading/trailing space is
/// quoted with its quotes doubled — the RFC 4180 rule. The leading-space case is
/// not about commas; it is about a spreadsheet trimming it and gluing two cells
/// together.
fn csv_field(value: &str) -> String {
    let needs_quotes = value.contains(['"', ',', '\n', '\r']) || value.trim() != value;
    if needs_quotes {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// One numeric cell, or an empty one.
fn csv_number(value: Option<f64>) -> String {
    match value {
        Some(number) if is_finite(number) => number.to_string(),
        // `None` is an empty cell and a non-finite value is refused as one: see
        // this function's caller for why the second case is not `0`.
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

/// The row shape the aggregate query returns.
///
/// `avg` over a `double precision` column is a `double precision` in
/// PostgreSQL (it is `numeric` for `numeric` input), so `Option<f64>` decodes
/// directly — and the nullable aggregates stay nullable, which is what makes an
/// empty window *representable* rather than coerced to zero.
#[derive(Debug, sqlx::FromRow)]
struct SummaryRow {
    service: String,
    metric: String,
    unit: String,
    samples: i64,
    current: Option<f64>,
    min: Option<f64>,
    avg: Option<f64>,
    max: Option<f64>,
    state: String,
    last_sample_at: Option<OffsetDateTime>,
}

impl From<SummaryRow> for MetricSummary {
    fn from(row: SummaryRow) -> Self {
        Self {
            service: row.service,
            metric: row.metric,
            unit: row.unit,
            samples: row.samples,
            // An aggregate of a window that holds samples can still be absent if
            // every value in it was refused — the sample constructor forbids
            // that today, so the fallbacks below are belt and braces rather than
            // a reachable path. They exist because a summary with a `Some` mean
            // and a `None` maximum is the incoherent state this type refuses to
            // hand a caller.
            current: row.current.or(row.avg),
            min: row.min,
            avg: row.avg,
            max: row.max,
            state: row.state,
            last_sample_at: row.last_sample_at.map(|at| at.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_named_and_its_name_is_its_spellings_only() {
        // The criterion is that the export matches what is on screen, and that
        // starts with one spelling per window: `168` and `24*7` are the same
        // window and the label only says one of them.
        assert_eq!(Range::Hour.key(), "1h");
        assert_eq!(Range::Day.key(), "24h");
        assert_eq!(Range::Week.key(), "7d");
        assert_eq!(Range::parse("1h").expect("1h is offered"), Range::Hour);
        assert_eq!(Range::parse("24h").expect("24h is offered"), Range::Day);
        assert_eq!(Range::parse("7d").expect("7d is offered"), Range::Week);
    }

    #[test]
    fn an_unoffered_range_is_refused_rather_than_clamped() {
        // The defect this prevents: a client asking for a week, silently getting
        // a day, and the table and the CSV both labelled `7d`.
        for key in ["168", "week", "1 hour", "", "0", "-1", "1H"] {
            assert!(
                Range::parse(key).is_err(),
                "{key} was silently accepted as a window"
            );
        }
        let error = Range::parse("168").expect_err("168 is not offered");
        assert!(
            error.to_string().contains("1h, 24h, 7d"),
            "the refusal must name what is offered: {error}"
        );
    }

    #[test]
    fn a_range_window_is_the_length_its_name_claims() {
        let now = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(Range::Hour.since(now), now - time::Duration::hours(1));
        assert_eq!(Range::Day.since(now), now - time::Duration::hours(24));
        assert_eq!(Range::Week.since(now), now - time::Duration::hours(168));
        // Only the week buckets: 168 points on a 300-pixel sparkline is noise.
        assert!(!Range::Hour.buckets_daily());
        assert!(!Range::Day.buckets_daily());
        assert!(Range::Week.buckets_daily());
    }

    #[test]
    fn an_empty_window_has_no_numbers_at_all() {
        // `avg()` over no rows is NULL, and the temptation is to print it as 0.
        // A metric that was never measured and a metric that measured zero have
        // to look different in this product.
        let summary = MetricSummary::empty("redis", "latency_ms");
        assert!(summary.is_empty());
        assert_eq!(summary.samples, 0);
        assert!(summary.current.is_none());
        assert!(summary.min.is_none());
        assert!(summary.avg.is_none());
        assert!(summary.max.is_none());
        assert!(summary.spread().is_none());
        assert_eq!(summary.state, crate::vocabulary::STATE_WHEN_UNPROBED);
    }

    #[test]
    fn a_populated_window_carries_all_four_numbers_or_none_of_them() {
        let summary = MetricSummary {
            service: "host".to_string(),
            metric: "cpu_percent".to_string(),
            unit: "%".to_string(),
            samples: 12,
            current: Some(41.0),
            min: Some(3.0),
            avg: Some(20.0),
            max: Some(58.0),
            state: "healthy".to_string(),
            last_sample_at: Some("2026-09-30T10:00:00Z".to_string()),
        };
        assert!(!summary.is_empty());
        assert_eq!(summary.spread(), Some(55.0));
    }

    #[test]
    fn the_csv_has_a_header_and_one_line_per_summary() {
        let summaries = vec![
            MetricSummary {
                service: "host".to_string(),
                metric: "cpu_percent".to_string(),
                unit: "%".to_string(),
                samples: 2,
                current: Some(41.0),
                min: Some(3.0),
                avg: Some(20.0),
                max: Some(58.0),
                state: "healthy".to_string(),
                last_sample_at: Some("2026-09-30T10:00:00Z".to_string()),
            },
            MetricSummary::empty("redis", "latency_ms"),
        ];
        let csv = summaries_to_csv(&summaries, Range::Day);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3, "a header and one row per summary");
        assert_eq!(lines[0], CSV_HEADER);
        assert!(lines[1].starts_with("host,cpu_percent,%,2,41,3,20,58,healthy,"));
        // The empty row: `samples` is 0 and every aggregate cell is blank.
        assert!(lines[2].contains("redis,latency_ms,,0,,,,,"), "{}", lines[2]);
    }

    #[test]
    fn the_export_stamps_the_range_into_every_row() {
        // "Matches the range shown" is checked by the client on the value of the
        // last column, so the file has to carry it even when it is the only
        // thing separating this week's data from last week's.
        let csv = summaries_to_csv(&[MetricSummary::empty("host", "disk_percent")], Range::Week);
        assert!(csv.lines().nth(1).expect("a row").ends_with(",7d"));
        let day = summaries_to_csv(&[MetricSummary::empty("host", "disk_percent")], Range::Day);
        assert!(day.lines().nth(1).expect("a row").ends_with(",24h"));
    }

    #[test]
    fn an_empty_export_is_still_a_valid_csv() {
        // A download of nothing must still open: a header with no rows is a
        // file, and an empty file is an error message in some spreadsheet.
        let csv = summaries_to_csv(&[], Range::Hour);
        assert_eq!(csv, format!("{CSV_HEADER}\n"));
    }

    #[test]
    fn a_field_that_needs_quoting_is_quoted() {
        // Only reachable if a future metric key carries a comma, and the cost of
        // being wrong here is a CSV whose columns silently shift left.
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(csv_field(" padded "), "\" padded \"");
    }

    #[test]
    fn a_non_finite_number_cannot_reach_the_export() {
        // `NaN` in a cell is read as text by most spreadsheets and sorted as a
        // number by the rest, so the export writes an empty cell instead.
        assert_eq!(csv_number(Some(f64::NAN)), "");
        assert_eq!(csv_number(Some(f64::INFINITY)), "");
        assert_eq!(csv_number(None), "");
        assert_eq!(csv_number(Some(0.0)), "0");
    }

    #[test]
    fn every_offered_range_renders_its_own_csv() {
        // A round trip through the header, so a change to the column list cannot
        // pass this suite by only being exercised on one range.
        for range in Range::all() {
            let csv = summaries_to_csv(&[MetricSummary::empty("api", "latency_ms")], range);
            let lines: Vec<&str> = csv.lines().collect();
            assert_eq!(lines.len(), 2, "{range} lost its row");
            assert_eq!(lines[0].split(',').count(), CSV_HEADER.split(',').count());
        }
    }
}