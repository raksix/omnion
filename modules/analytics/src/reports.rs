//! Reports: reading the numbers back (REQ-007, slice 2).
//!
//! The read side of the engine, in one place, so the API layer stays thin and the numbers have
//! exactly one definition:
//!
//! * **The overview reads the raw rows while they are the truth** — a range inside the site's
//!   retention window is answered from `analytics_visits`, `analytics_pageviews`,
//!   `analytics_events` and the goal hits, which is where *distinct visitors across a period*
//!   is a real number rather than a sum of per-day counts. A range that reaches past retention
//!   (raw rows are purged there) falls back to `analytics_daily` and says so: the answer carries
//!   `exact = false`, and the screen states the difference instead of quietly inflating or
//!   shrinking a number. That fallback is the only read of a rollup here; every other report is
//!   a detail report and reads raw rows inside the retention window.
//! * **Every report is a URL.** Filters, sorting, paging and the date range are plain values
//!   this module takes in; nothing here remembers a choice.
//! * **A day is the UTC day.** Rollups and reports share the boundary (REQ-113 brings the
//!   per-site timezone), so a bucket the worker computed and a bucket shown here agree.
//!
//! Series buckets are addressed by an **offset from the range start** rather than by a truncated
//! timestamp: `date_trunc` would follow the connection's time zone, and a report that shifts by
//! an hour depending on who asks is worse than no report.
//!
//! The filters of a report are built by [`Narrowing`]: only the clauses in use are written, and
//! the placeholder numbers follow from how many values are already bound, so a report never
//! carries a filter it does not apply.

use serde::Serialize;
use sqlx::PgPool;
use time::{Date, Duration, Month, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AnalyticsError, Result};

/// Longest range a report answers, in days (a year plus the leap day).
pub const MAX_RANGE_DAYS: i64 = 366;

/// Rows one page of a report table holds when the caller names nothing.
pub const DEFAULT_PER_PAGE: i64 = 50;

/// Hard cap on a page of a report table.
pub const MAX_PER_PAGE: i64 = 200;

/// Rows a ranked report returns at most (top pages, sources, dimensions, countries).
pub const MAX_RANKED_ROWS: i64 = 100;

/// Longest value a filter may carry before it is refused.
pub const MAX_FILTER_LENGTH: usize = 200;

/// The device values the collector may have stored.
pub const DEVICES: [&str; 4] = ["desktop", "mobile", "tablet", "other"];

/// The group-by modes of the sources report; `combination` keeps the five UTM columns.
pub const SOURCE_GROUPS: [&str; 7] = [
    "combination",
    "source",
    "medium",
    "campaign",
    "referrer",
    "term",
    "content",
];

/// The sorts a page report accepts, as `(key, order by fragment)`.
///
/// The fragment is taken from this table and never from the caller: a sort key is parsed into a
/// known value, and only the known value reaches the query.
pub const PAGE_SORTS: [(&str, &str); 7] = [
    ("views", "views"),
    ("visitors", "visitors"),
    ("views_per_visitor", "views::float8 / greatest(visitors, 1)"),
    ("avg_time", "avg_time_ms"),
    (
        "bounce_rate",
        "bounced_visits::float8 / greatest(visits, 1)",
    ),
    ("entrances", "entrances"),
    ("exits", "exits"),
];

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

/// An inclusive range of UTC days, `from` first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DateRange {
    /// First day the report covers.
    pub from: Date,
    /// Last day the report covers.
    pub to: Date,
}

impl DateRange {
    /// Validate a range: `from` before (or on) `to`, and at most [`MAX_RANGE_DAYS`] days long.
    pub fn new(from: Date, to: Date) -> Result<Self> {
        if to < from {
            return Err(AnalyticsError::InvalidQuery(
                "the range ends before it starts".to_owned(),
            ));
        }
        if (to - from).whole_days() > MAX_RANGE_DAYS {
            return Err(AnalyticsError::InvalidQuery(format!(
                "a report covers at most {MAX_RANGE_DAYS} days"
            )));
        }

        Ok(Self { from, to })
    }

    /// The same number of days, ending the day before `from`.
    #[must_use]
    pub fn previous(self) -> Self {
        let days = self.days();
        Self {
            from: self.from - Duration::days(days),
            to: self.from - Duration::days(1),
        }
    }

    /// How many days the range covers (inclusive).
    #[must_use]
    pub fn days(self) -> i64 {
        (self.to - self.from).whole_days() + 1
    }

    /// `[start, end)` as instants: the first midnight to the midnight after the last day.
    #[must_use]
    pub fn bounds(self) -> (OffsetDateTime, OffsetDateTime) {
        let start = self.from.midnight().assume_utc();
        let end = (self.to + Duration::days(1)).midnight().assume_utc();
        (start, end)
    }

    /// A label for exports and screens: the one day, or `first..last`.
    #[must_use]
    pub fn label(self) -> String {
        if self.from == self.to {
            self.from.to_string()
        } else {
            format!("{}..{}", self.from, self.to)
        }
    }
}

/// The bucket size of a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// One bucket per UTC hour; only offered for short ranges.
    Hour,
    /// One bucket per UTC day.
    Day,
}

impl Granularity {
    /// Parse a caller's choice; `auto` and an absent value let the range decide.
    pub fn parse(value: Option<&str>, range: DateRange) -> Result<Self> {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("hour") => Ok(Self::Hour),
            Some(value) if value.eq_ignore_ascii_case("day") => Ok(Self::Day),
            Some(value) if value.eq_ignore_ascii_case("auto") => Ok(Self::auto(range)),
            Some("") | None => Ok(Self::auto(range)),
            Some(other) => Err(AnalyticsError::InvalidQuery(format!(
                "granularity \"{other}\" is not one of hour, day, auto"
            ))),
        }
    }

    /// The natural bucket size of a range: hours for the last two days, days beyond that.
    #[must_use]
    pub fn auto(range: DateRange) -> Self {
        if range.days() <= 2 {
            Self::Hour
        } else {
            Self::Day
        }
    }

    /// Seconds one bucket spans.
    #[must_use]
    fn seconds(self) -> f64 {
        match self {
            Self::Hour => 3_600.0,
            Self::Day => 86_400.0,
        }
    }

    /// The name the API answers with.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }
}

/// The filters a report table combines: each is optional and they narrow together (AND).
#[derive(Debug, Clone, Default)]
pub struct Filters {
    /// Substring of the path.
    pub path: Option<String>,
    /// Substring of the page title.
    pub title: Option<String>,
    /// One device type (`desktop`, `mobile`, `tablet`, `other`).
    pub device: Option<String>,
    /// One ISO-3166 alpha-2 country code.
    pub country: Option<String>,
    /// One source, exactly as the sources report names it.
    pub source: Option<String>,
}

impl Filters {
    /// Validate the raw filter values: a length cap, a closed device list, a country code shape.
    pub fn new(
        path: Option<String>,
        title: Option<String>,
        device: Option<String>,
        country: Option<String>,
        source: Option<String>,
    ) -> Result<Self> {
        let device = clean_filter("device", device)?.map(|value| value.to_ascii_lowercase());
        if let Some(value) = &device
            && !DEVICES.contains(&value.as_str())
        {
            return Err(AnalyticsError::InvalidQuery(format!(
                "device \"{value}\" is not one of {}",
                DEVICES.join(", ")
            )));
        }

        let country = clean_filter("country", country)?.map(|value| value.to_ascii_uppercase());
        if let Some(value) = &country
            && (value.len() != 2 || !value.chars().all(|ch| ch.is_ascii_alphabetic()))
        {
            return Err(AnalyticsError::InvalidQuery(format!(
                "country \"{value}\" is not a two-letter code"
            )));
        }

        Ok(Self {
            path: clean_filter("path", path)?,
            title: clean_filter("title", title)?,
            device,
            country,
            source: clean_filter("source", source)?,
        })
    }

    /// `true` when nothing is narrowed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.path.is_none()
            && self.title.is_none()
            && self.device.is_none()
            && self.country.is_none()
            && self.source.is_none()
    }
}

/// Trim a filter value, refuse an oversized one, and turn an empty one into `None`.
fn clean_filter(name: &str, value: Option<String>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().count() > MAX_FILTER_LENGTH {
        return Err(AnalyticsError::InvalidQuery(format!(
            "the {name} filter is longer than {MAX_FILTER_LENGTH} characters"
        )));
    }

    Ok(Some(value.to_owned()))
}

/// Parse a `YYYY-MM-DD` day; `None` when the text is not a day.
#[must_use]
pub fn day_from_str(text: &str) -> Option<Date> {
    let mut parts = text.trim().split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }

    Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()
}

// ---------------------------------------------------------------------------------------------
// Narrowing
// ---------------------------------------------------------------------------------------------

/// The `where` fragment a report's filters become, plus the values they bind.
///
/// A report binds `site_id`, `from` and `to` first, so the placeholders here start at `$4` and
/// follow how many values are already written: only the clauses in use appear, which is why a
/// filter that cannot narrow a report never shows up as a no-op in its SQL.
#[derive(Debug, Default)]
pub struct Narrowing {
    clause: String,
    values: Vec<String>,
}

impl Narrowing {
    /// Append one clause and its value.
    ///
    /// The fragment carries one `{}` where its placeholder goes; the number follows from how
    /// many values are already bound, which is why a report's SQL and its binds cannot drift.
    fn and(&mut self, fragment: &str, value: &str) {
        self.values.push(value.to_owned());
        let index = self.values.len() + 3;
        let clause = fragment.replace("{}", &format!("${index}"));
        self.clause.push_str(&format!(" and ({clause})"));
    }

    /// The clause as a `where` fragment (empty when nothing is narrowed).
    #[must_use]
    pub fn clause(&self) -> &str {
        &self.clause
    }

    /// How many values the clause binds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// `true` when nothing is narrowed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The placeholder number after the last filter value (where a `limit` goes).
    #[must_use]
    fn next_index(&self) -> usize {
        self.values.len() + 4
    }

    /// The filter values as bindable `Option<String>`s (never null: only clauses in use appear).
    fn binds(&self) -> Vec<Option<String>> {
        self.values.iter().cloned().map(Some).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Shapes the API answers with
// ---------------------------------------------------------------------------------------------

/// One headline number, beside the same number of the previous period when comparing.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Metric {
    /// The value of the requested range.
    pub value: i64,
    /// The value of the previous equal-length range; `None` when not comparing.
    pub previous: Option<i64>,
}

/// One point of a series.
#[derive(Debug, Clone, Serialize)]
pub struct SeriesPoint {
    /// Stable bucket key (`2026-09-26`, `2026-09-26T14:00Z`).
    pub bucket: String,
    /// What the axis shows (`Sep 26`, `14:00`).
    pub label: String,
    /// Distinct visitors in the bucket.
    pub visitors: i64,
    /// Pageviews in the bucket.
    pub pageviews: i64,
    /// The same bucket one period earlier; `None` when not comparing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_visitors: Option<i64>,
    /// The previous period's pageviews for the bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_pageviews: Option<i64>,
}

/// One ranked value with its counts.
#[derive(Debug, Clone, Serialize)]
pub struct DimensionRow {
    /// The dimension value (`/pricing`, `Google`, `desktop`).
    pub value: String,
    /// Distinct visitors, when the source counts them.
    pub visitors: Option<i64>,
    /// Views or visits, when the source counts them.
    pub views: Option<i64>,
}

/// The overview: the five headline numbers, the series, and the three side panels.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    /// The range the numbers cover.
    pub range: DateRange,
    /// The previous equal-length range.
    pub previous_range: DateRange,
    /// Whether a comparison was asked for.
    pub compare: bool,
    /// `false` when the numbers come from the daily rollups (the range reaches past retention).
    pub exact: bool,
    /// `false` when the previous period holds no traffic at all — a delta against nothing is not
    /// a number, so the screens say "no comparison" instead of showing zero.
    pub previous_has_data: bool,
    /// The bucket size the series uses (`hour`, `day`).
    pub granularity: String,
    /// Visitors, pageviews, conversions, forms and downloads.
    pub kpis: OverviewKpis,
    /// The requested period, one point per bucket.
    pub series: Vec<SeriesPoint>,
    /// The busiest pages, best first.
    pub top_pages: Vec<DimensionRow>,
    /// The busiest sources, best first.
    pub top_sources: Vec<DimensionRow>,
    /// Visitors per device type.
    pub devices: Vec<DimensionRow>,
}

/// The five headline numbers of the overview.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OverviewKpis {
    /// Distinct visitors.
    pub visitors: Metric,
    /// Pageviews.
    pub pageviews: Metric,
    /// Distinct visitors with a goal hit.
    pub conversions: Metric,
    /// Form submissions.
    pub forms: Metric,
    /// File downloads.
    pub downloads: Metric,
}

/// One row of the page report.
#[derive(Debug, Clone, Serialize)]
pub struct PageRow {
    /// The path.
    pub path: String,
    /// The most recent title seen for the path.
    pub title: Option<String>,
    /// Pageviews.
    pub views: i64,
    /// Distinct visitors.
    pub visitors: i64,
    /// Views divided by visitors.
    pub views_per_visitor: Option<f64>,
    /// Mean `duration_ms` of the pageviews that carried one.
    pub avg_time_ms: Option<f64>,
    /// Share of the visits that included this path and bounced (`0..1`).
    pub bounce_rate: Option<f64>,
    /// Pageviews that were the first of their visit.
    pub entrances: i64,
    /// Pageviews that were the last of their visit.
    pub exits: i64,
}

/// The filters as the screen wrote them, echoed back for chips and exports.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FilterEcho {
    /// Path substring.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Title substring.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Device type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Country code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl From<&Filters> for FilterEcho {
    fn from(filters: &Filters) -> Self {
        Self {
            path: filters.path.clone(),
            title: filters.title.clone(),
            device: filters.device.clone(),
            country: filters.country.clone(),
            source: filters.source.clone(),
        }
    }
}

/// The page report, paged and sorted.
#[derive(Debug, Clone, Serialize)]
pub struct PagesReport {
    /// The range the rows cover.
    pub range: DateRange,
    /// The filters that produced the rows.
    pub filters: FilterEcho,
    /// The sort key in effect.
    pub sort: String,
    /// `asc` or `desc`.
    pub direction: String,
    /// One-based page number.
    pub page: i64,
    /// Rows per page.
    pub per_page: i64,
    /// Rows in the whole filtered set.
    pub total: i64,
    /// The rows of this page.
    pub rows: Vec<PageRow>,
}

/// One row of the sources report.
#[derive(Debug, Clone, Serialize)]
pub struct SourceRow {
    /// Source: the UTM source, the referrer host, or `(direct)`.
    pub source: String,
    /// UTM medium.
    pub medium: Option<String>,
    /// UTM campaign.
    pub campaign: Option<String>,
    /// UTM term.
    pub term: Option<String>,
    /// UTM content.
    pub content: Option<String>,
    /// Visits attributed to the row.
    pub visits: i64,
    /// Distinct visitors of those visits.
    pub visitors: i64,
    /// Distinct visitors of those visits that also reached a goal in the range.
    pub conversions: i64,
    /// Conversions divided by visitors.
    pub conversion_rate: Option<f64>,
}

/// The sources report.
#[derive(Debug, Clone, Serialize)]
pub struct SourcesReport {
    /// The range the rows cover.
    pub range: DateRange,
    /// `combination`, or the one dimension the caller grouped by.
    pub group: String,
    /// The rows, best first.
    pub rows: Vec<SourceRow>,
}

/// One bar panel of the audience report.
#[derive(Debug, Clone, Serialize)]
pub struct DimensionPanel {
    /// Stable key of the panel (`device`).
    pub kind: String,
    /// The panel's title.
    pub title: String,
    /// The values, best first.
    pub rows: Vec<DimensionRow>,
}

/// One row of the countries table.
#[derive(Debug, Clone, Serialize)]
pub struct CountryRow {
    /// ISO-3166 alpha-2 code, or `(unknown)`.
    pub code: String,
    /// Distinct visitors.
    pub visitors: i64,
    /// Pageviews of the visits from that country.
    pub views: i64,
    /// Share of the visitors of the range (`0..1`).
    pub share: f64,
}

/// The audience report: five bar panels plus the countries table.
#[derive(Debug, Clone, Serialize)]
pub struct AudienceReport {
    /// The range the numbers cover.
    pub range: DateRange,
    /// Devices, browsers, operating systems, screen sizes and languages.
    pub panels: Vec<DimensionPanel>,
    /// Countries, best first.
    pub countries: Vec<CountryRow>,
    /// Visitors the shares are computed against.
    pub visitors: i64,
}

/// One row of the events report.
#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    /// Event name.
    pub name: String,
    /// Times the event fired.
    pub count: i64,
    /// Distinct visitors that fired it.
    pub visitors: i64,
    /// Sum of the values the events carried.
    pub value_sum: Option<f64>,
    /// When it was last seen, RFC 3339.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen: Option<OffsetDateTime>,
}

/// The events report.
#[derive(Debug, Clone, Serialize)]
pub struct EventsReport {
    /// The range the rows cover.
    pub range: DateRange,
    /// The rows, most frequent first.
    pub rows: Vec<EventRow>,
}

/// One property of one event, as the breakdown drawer shows it.
#[derive(Debug, Clone, Serialize)]
pub struct PropertyRow {
    /// The property key.
    pub key: String,
    /// The value, rendered as text.
    pub value: String,
    /// Times the pair appeared.
    pub count: i64,
}

/// One bucket of a plain count series.
#[derive(Debug, Clone, Serialize)]
pub struct CountPoint {
    /// Stable bucket key.
    pub bucket: String,
    /// Axis label.
    pub label: String,
    /// The count.
    pub count: i64,
}

/// One event in detail.
#[derive(Debug, Clone, Serialize)]
pub struct EventDetail {
    /// The range the numbers cover.
    pub range: DateRange,
    /// The event name.
    pub name: String,
    /// Times it fired.
    pub count: i64,
    /// Distinct visitors.
    pub visitors: i64,
    /// Sum of the carried values.
    pub value_sum: Option<f64>,
    /// When it was last seen.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen: Option<OffsetDateTime>,
    /// The event per day of the range.
    pub series: Vec<CountPoint>,
    /// The property breakdown, most frequent pair first.
    pub properties: Vec<PropertyRow>,
}

/// One row of the downloads report.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadRow {
    /// The file, or the page it was started from.
    pub value: String,
    /// Download events.
    pub downloads: i64,
    /// Distinct visitors.
    pub visitors: i64,
}

/// The downloads report.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadsReport {
    /// The range the rows cover.
    pub range: DateRange,
    /// Downloads in the range.
    pub total: i64,
    /// By file, best first.
    pub files: Vec<DownloadRow>,
    /// By page, best first.
    pub pages: Vec<DownloadRow>,
}

/// One row of the forms report.
#[derive(Debug, Clone, Serialize)]
pub struct FormRow {
    /// The form name the events carried.
    pub form: String,
    /// Submissions.
    pub submissions: i64,
    /// Distinct visitors of those submissions.
    pub visitors: i64,
    /// Sum of the values submitted.
    pub value_sum: Option<f64>,
    /// `form_start` events for the same form (0 when the site emits none).
    pub starts: i64,
    /// Submissions divided by starts; `None` when the site emits no `form_start`.
    pub completion_rate: Option<f64>,
    /// Starts that never became a submission; `None` without starts.
    pub abandonment: Option<i64>,
    /// When the form was last submitted.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen: Option<OffsetDateTime>,
}

/// The forms report.
#[derive(Debug, Clone, Serialize)]
pub struct FormsReport {
    /// The range the rows cover.
    pub range: DateRange,
    /// The rows, most submitted first.
    pub rows: Vec<FormRow>,
}

/// Any report, as the export endpoint speaks it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "report", rename_all = "snake_case")]
pub enum Report {
    /// The overview.
    Overview(Overview),
    /// The page report.
    Pages(PagesReport),
    /// The sources report.
    Sources(SourcesReport),
    /// The audience report.
    Audience(AudienceReport),
    /// The events report.
    Events(EventsReport),
    /// The downloads report.
    Downloads(DownloadsReport),
    /// The forms report.
    Forms(FormsReport),
}

impl Report {
    /// The report key (`pages`).
    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            Self::Overview(_) => "overview",
            Self::Pages(_) => "pages",
            Self::Sources(_) => "sources",
            Self::Audience(_) => "audience",
            Self::Events(_) => "events",
            Self::Downloads(_) => "downloads",
            Self::Forms(_) => "forms",
        }
    }

    /// The report as CSV — the header row plus exactly the rows the screen showed.
    #[must_use]
    pub fn csv(&self) -> String {
        let (headers, rows) = self.csv_rows();
        let mut body = headers.join(",");
        body.push('\n');
        for row in rows {
            body.push_str(&row.join(","));
            body.push('\n');
        }

        body
    }

    /// The rows of the report as CSV fields, header first.
    #[must_use]
    pub fn csv_rows(&self) -> (Vec<String>, Vec<Vec<String>>) {
        match self {
            Self::Overview(report) => {
                let headers = fields(&[
                    "bucket",
                    "label",
                    "visitors",
                    "pageviews",
                    "previous_visitors",
                    "previous_pageviews",
                ]);
                let rows = report
                    .series
                    .iter()
                    .map(|point| {
                        vec![
                            csv_field(&point.bucket),
                            csv_field(&point.label),
                            point.visitors.to_string(),
                            point.pageviews.to_string(),
                            number(point.previous_visitors.map(|value| value as f64)),
                            number(point.previous_pageviews.map(|value| value as f64)),
                        ]
                    })
                    .collect();
                (headers, rows)
            }
            Self::Pages(report) => {
                let headers = fields(&[
                    "path",
                    "title",
                    "views",
                    "visitors",
                    "views_per_visitor",
                    "avg_time_ms",
                    "bounce_rate",
                    "entrances",
                    "exits",
                ]);
                let rows = report
                    .rows
                    .iter()
                    .map(|row| {
                        vec![
                            csv_field(&row.path),
                            csv_field(row.title.as_deref().unwrap_or("")),
                            row.views.to_string(),
                            row.visitors.to_string(),
                            number(row.views_per_visitor),
                            number(row.avg_time_ms),
                            number(row.bounce_rate),
                            row.entrances.to_string(),
                            row.exits.to_string(),
                        ]
                    })
                    .collect();
                (headers, rows)
            }
            Self::Sources(report) => {
                let headers = fields(&[
                    "source",
                    "medium",
                    "campaign",
                    "term",
                    "content",
                    "visits",
                    "visitors",
                    "conversions",
                    "conversion_rate",
                ]);
                let rows = report
                    .rows
                    .iter()
                    .map(|row| {
                        vec![
                            csv_field(&row.source),
                            csv_field(row.medium.as_deref().unwrap_or("")),
                            csv_field(row.campaign.as_deref().unwrap_or("")),
                            csv_field(row.term.as_deref().unwrap_or("")),
                            csv_field(row.content.as_deref().unwrap_or("")),
                            row.visits.to_string(),
                            row.visitors.to_string(),
                            row.conversions.to_string(),
                            number(row.conversion_rate),
                        ]
                    })
                    .collect();
                (headers, rows)
            }
            Self::Audience(report) => {
                let headers = fields(&["country", "visitors", "views", "share"]);
                let rows = report
                    .countries
                    .iter()
                    .map(|row| {
                        vec![
                            csv_field(&row.code),
                            row.visitors.to_string(),
                            row.views.to_string(),
                            number(Some(row.share)),
                        ]
                    })
                    .collect();
                (headers, rows)
            }
            Self::Events(report) => {
                let headers = fields(&["event", "count", "visitors", "value_sum", "last_seen"]);
                let rows = report
                    .rows
                    .iter()
                    .map(|row| {
                        vec![
                            csv_field(&row.name),
                            row.count.to_string(),
                            row.visitors.to_string(),
                            number(row.value_sum),
                            csv_field(&instant(row.last_seen)),
                        ]
                    })
                    .collect();
                (headers, rows)
            }
            Self::Downloads(report) => {
                let headers = fields(&["kind", "value", "downloads", "visitors"]);
                let mut rows: Vec<Vec<String>> = report
                    .files
                    .iter()
                    .map(|row| {
                        vec![
                            "file".to_owned(),
                            csv_field(&row.value),
                            row.downloads.to_string(),
                            row.visitors.to_string(),
                        ]
                    })
                    .collect();
                rows.extend(report.pages.iter().map(|row| {
                    vec![
                        "page".to_owned(),
                        csv_field(&row.value),
                        row.downloads.to_string(),
                        row.visitors.to_string(),
                    ]
                }));
                (headers, rows)
            }
            Self::Forms(report) => {
                let headers = fields(&[
                    "form",
                    "submissions",
                    "visitors",
                    "value_sum",
                    "starts",
                    "completion_rate",
                    "abandonment",
                    "last_seen",
                ]);
                let rows = report
                    .rows
                    .iter()
                    .map(|row| {
                        vec![
                            csv_field(&row.form),
                            row.submissions.to_string(),
                            row.visitors.to_string(),
                            number(row.value_sum),
                            row.starts.to_string(),
                            number(row.completion_rate),
                            number(row.abandonment.map(|value| value as f64)),
                            csv_field(&instant(row.last_seen)),
                        ]
                    })
                    .collect();
                (headers, rows)
            }
        }
    }
}

/// The header cells, each quoted when it has to be.
fn fields(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| csv_field(name)).collect()
}

/// A number as a CSV field: empty when there is no number.
fn number(value: Option<f64>) -> String {
    match value {
        Some(value) if value.is_finite() => {
            let rounded = (value * 100.0).round() / 100.0;
            if rounded.fract().abs() < f64::EPSILON {
                format!("{}", rounded as i64)
            } else {
                format!("{rounded}")
            }
        }
        _ => String::new(),
    }
}

/// An instant as a CSV field (`2026-09-26T14:03:11Z`), empty when there is none.
fn instant(value: Option<OffsetDateTime>) -> String {
    match value {
        Some(value) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            value.year(),
            u8::from(value.month()),
            value.day(),
            value.hour(),
            value.minute(),
            value.second()
        ),
        None => String::new(),
    }
}

/// One CSV field: quoted when it has to be, with inner quotes doubled (RFC 4180).
#[must_use]
pub fn csv_field(value: &str) -> String {
    let needs_quotes = value
        .chars()
        .any(|ch| matches!(ch, ',' | '"' | '\n' | '\r'))
        || value.starts_with(' ')
        || value.ends_with(' ');
    if !needs_quotes {
        return value.to_owned();
    }
    format!("\"{}\"", value.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------------------------
// Narrowing helpers
// ---------------------------------------------------------------------------------------------

/// The filters a page report combines: path and title on the pageview, the rest on the visit.
#[must_use]
pub fn narrow_pages(filters: &Filters) -> Narrowing {
    let mut narrowing = Narrowing::default();
    if let Some(value) = &filters.path {
        narrowing.and("pages.path ilike '%' || {} || '%'", value);
    }
    if let Some(value) = &filters.title {
        narrowing.and("coalesce(pages.title, '') ilike '%' || {} || '%'", value);
    }
    if let Some(value) = &filters.device {
        narrowing.and("coalesce(visits.device_type, '') = {}", value);
    }
    if let Some(value) = &filters.country {
        narrowing.and("coalesce(visits.country_code::text, '') = {}", value);
    }
    if let Some(value) = &filters.source {
        narrowing.and(
            "coalesce(nullif(visits.source, ''), nullif(visits.referrer_host, ''), '(direct)') = {}",
            value,
        );
    }

    narrowing
}

/// The filters a visit-scoped report combines.
///
/// A visit has no path of its own — its pageviews do — so the path filter narrows through an
/// `exists` over the visit's own pageviews.
#[must_use]
pub fn narrow_visits(filters: &Filters) -> Narrowing {
    let mut narrowing = Narrowing::default();
    if let Some(value) = &filters.path {
        narrowing.and(
            "exists (select 1 from analytics_pageviews pages where pages.visit_id = visits.id \
             and pages.path ilike '%' || {} || '%')",
            value,
        );
    }
    if let Some(value) = &filters.device {
        narrowing.and("coalesce(visits.device_type, '') = {}", value);
    }
    if let Some(value) = &filters.country {
        narrowing.and("coalesce(visits.country_code::text, '') = {}", value);
    }
    if let Some(value) = &filters.source {
        narrowing.and(
            "coalesce(nullif(visits.source, ''), nullif(visits.referrer_host, ''), '(direct)') = {}",
            value,
        );
    }

    narrowing
}

/// The filters an event-scoped report combines: the path on the event, the rest on the visit.
#[must_use]
pub fn narrow_events(filters: &Filters) -> Narrowing {
    let mut narrowing = Narrowing::default();
    if let Some(value) = &filters.path {
        narrowing.and("coalesce(events.path, '') ilike '%' || {} || '%'", value);
    }
    if let Some(value) = &filters.device {
        narrowing.and("coalesce(visits.device_type, '') = {}", value);
    }
    if let Some(value) = &filters.country {
        narrowing.and("coalesce(visits.country_code::text, '') = {}", value);
    }
    if let Some(value) = &filters.source {
        narrowing.and(
            "coalesce(nullif(visits.source, ''), nullif(visits.referrer_host, ''), '(direct)') = {}",
            value,
        );
    }

    narrowing
}

// ---------------------------------------------------------------------------------------------
// The overview
// ---------------------------------------------------------------------------------------------

/// The five headline numbers of one period.
#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    visitors: i64,
    pageviews: i64,
    conversions: i64,
    forms: i64,
    downloads: i64,
}

/// Answer the overview for one site and range.
///
/// `retention_days` is the site's own retention: a range that starts before the oldest day the
/// raw rows still hold is answered from the daily rollups instead, and the answer says so.
pub async fn overview(
    pool: &PgPool,
    site_id: Uuid,
    range: DateRange,
    compare: bool,
    granularity: Granularity,
    retention_days: i32,
    today: Date,
) -> Result<Overview> {
    let previous_range = range.previous();
    let exact = range.from >= today - Duration::days(i64::from(retention_days).max(1) - 1);
    let (from, to) = range.bounds();
    let (previous_from, previous_to) = previous_range.bounds();

    let (totals, previous_totals) = if exact {
        (
            totals_exact(pool, site_id, from, to).await?,
            totals_exact(pool, site_id, previous_from, previous_to).await?,
        )
    } else {
        (
            totals_rollup(pool, site_id, range).await?,
            totals_rollup(pool, site_id, previous_range).await?,
        )
    };

    // Hourly buckets only exist for an exact range: the daily rollups are the only fallback, and
    // a bucket the source cannot answer is not invented.
    let granularity = if exact { granularity } else { Granularity::Day };

    let (series, previous_series) = if exact {
        (
            series_exact(pool, site_id, range, granularity).await?,
            if compare {
                series_exact(pool, site_id, previous_range, granularity).await?
            } else {
                Vec::new()
            },
        )
    } else {
        (
            series_rollup(pool, site_id, range).await?,
            if compare {
                series_rollup(pool, site_id, previous_range).await?
            } else {
                Vec::new()
            },
        )
    };

    let (top_pages, top_sources, devices) = if exact {
        (
            top_pages_exact(pool, site_id, from, to).await?,
            top_sources_exact(pool, site_id, from, to).await?,
            devices_exact(pool, site_id, from, to).await?,
        )
    } else {
        (
            ranked_rollup(pool, site_id, range, "pageviews", "path", true).await?,
            ranked_rollup(pool, site_id, range, "visitors", "referrer", false).await?,
            ranked_rollup(pool, site_id, range, "visitors", "device", false).await?,
        )
    };

    let series = merge_previous(series, &previous_series, compare);

    Ok(Overview {
        range,
        previous_range,
        compare,
        exact,
        previous_has_data: previous_totals.visitors > 0 || previous_totals.pageviews > 0,
        granularity: granularity.name().to_owned(),
        kpis: OverviewKpis {
            visitors: metric(totals.visitors, previous_totals.visitors, compare),
            pageviews: metric(totals.pageviews, previous_totals.pageviews, compare),
            conversions: metric(totals.conversions, previous_totals.conversions, compare),
            forms: metric(totals.forms, previous_totals.forms, compare),
            downloads: metric(totals.downloads, previous_totals.downloads, compare),
        },
        series,
        top_pages,
        top_sources,
        devices,
    })
}

/// One metric beside its previous value.
fn metric(value: i64, previous: i64, compare: bool) -> Metric {
    Metric {
        value,
        previous: if compare { Some(previous) } else { None },
    }
}

/// Fold the previous period's series into the requested one, bucket by bucket.
fn merge_previous(
    mut series: Vec<SeriesPoint>,
    previous: &[SeriesPoint],
    compare: bool,
) -> Vec<SeriesPoint> {
    if !compare {
        return series;
    }
    for (index, point) in series.iter_mut().enumerate() {
        let other = previous.get(index);
        point.previous_visitors = Some(other.map_or(0, |point| point.visitors));
        point.previous_pageviews = Some(other.map_or(0, |point| point.pageviews));
    }

    series
}

/// The KPI totals over `[from, to)` from the raw rows.
async fn totals_exact(
    pool: &PgPool,
    site_id: Uuid,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Totals> {
    let visitors: i64 = sqlx::query_scalar(
        "select count(distinct visitor_hash)::bigint from analytics_visits \
         where site_id = $1 and started_at >= $2 and started_at < $3",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    let pageviews: i64 = sqlx::query_scalar(
        "select count(*)::bigint from analytics_pageviews \
         where site_id = $1 and occurred_at >= $2 and occurred_at < $3",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    let conversions: i64 = sqlx::query_scalar(
        "select count(distinct hits.visitor_hash)::bigint from analytics_goal_hits hits \
         join analytics_goals goals on goals.id = hits.goal_id \
         where goals.site_id = $1 and hits.occurred_at >= $2 and hits.occurred_at < $3",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    let forms: i64 = sqlx::query_scalar(
        "select count(*)::bigint from analytics_events \
         where site_id = $1 and name = 'form_submit' and occurred_at >= $2 and occurred_at < $3",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    let downloads: i64 = sqlx::query_scalar(
        "select count(*)::bigint from analytics_events \
         where site_id = $1 and name = 'download' and occurred_at >= $2 and occurred_at < $3",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    Ok(Totals {
        visitors,
        pageviews,
        conversions,
        forms,
        downloads,
    })
}

/// The KPI totals over `range` from the daily rollups (the sum of the per-day counts).
async fn totals_rollup(pool: &PgPool, site_id: Uuid, range: DateRange) -> Result<Totals> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select metric, coalesce(sum(analytics_daily.count), 0)::bigint from analytics_daily \
         where site_id = $1 and day >= $2 and day <= $3 \
         and dimension_kind = 'total' \
         and metric in ('visitors', 'pageviews', 'conversions', 'forms', 'downloads') \
         group by metric",
    )
    .bind(site_id)
    .bind(range.from)
    .bind(range.to)
    .fetch_all(pool)
    .await?;

    let mut totals = Totals::default();
    for (metric, count) in rows {
        match metric.as_str() {
            "visitors" => totals.visitors = count,
            "pageviews" => totals.pageviews = count,
            "conversions" => totals.conversions = count,
            "forms" => totals.forms = count,
            "downloads" => totals.downloads = count,
            _ => {}
        }
    }

    Ok(totals)
}

/// The bucket list of a range, one entry per bucket, zeroed.
fn empty_series(range: DateRange, granularity: Granularity) -> Vec<SeriesPoint> {
    let mut points = Vec::new();
    let mut cursor = range.from.midnight().assume_utc();
    let end = range.to.midnight().assume_utc();
    let step = match granularity {
        Granularity::Hour => Duration::hours(1),
        Granularity::Day => Duration::days(1),
    };
    let last = match granularity {
        // The last hour of the last day, so "today" shows all 24 hours of it.
        Granularity::Hour => end + Duration::hours(23),
        Granularity::Day => end,
    };

    while cursor <= last {
        points.push(SeriesPoint {
            bucket: bucket_key(cursor, granularity),
            label: bucket_label(cursor, granularity),
            visitors: 0,
            pageviews: 0,
            previous_visitors: None,
            previous_pageviews: None,
        });
        cursor += step;
    }

    points
}

/// The stable key of one bucket.
fn bucket_key(start: OffsetDateTime, granularity: Granularity) -> String {
    match granularity {
        Granularity::Hour => format!(
            "{:04}-{:02}-{:02}T{:02}:00Z",
            start.year(),
            u8::from(start.month()),
            start.day(),
            start.hour()
        ),
        Granularity::Day => start.date().to_string(),
    }
}

/// The axis label of one bucket.
fn bucket_label(start: OffsetDateTime, granularity: Granularity) -> String {
    match granularity {
        Granularity::Hour => format!("{:02}:00", start.hour()),
        Granularity::Day => format!("{} {}", month_short(u8::from(start.month())), start.day()),
    }
}

/// The three-letter month name of a number.
#[must_use]
pub fn month_short(month: u8) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    }
}

/// The visitors and pageviews per bucket over `range`, from the raw rows.
async fn series_exact(
    pool: &PgPool,
    site_id: Uuid,
    range: DateRange,
    granularity: Granularity,
) -> Result<Vec<SeriesPoint>> {
    let (from, to) = range.bounds();
    let width = granularity.seconds();
    let mut series = empty_series(range, granularity);

    let visitors: Vec<(i32, i64)> = sqlx::query_as(
        "select floor(extract(epoch from (started_at - $2::timestamptz)) / $4::float8)::int as bucket, \
         count(distinct visitor_hash)::bigint as count \
         from analytics_visits \
         where site_id = $1 and started_at >= $2 and started_at < $3 group by 1",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .bind(width)
    .fetch_all(pool)
    .await?;

    let pageviews: Vec<(i32, i64)> = sqlx::query_as(
        "select floor(extract(epoch from (occurred_at - $2::timestamptz)) / $4::float8)::int as bucket, \
         count(*)::bigint as count \
         from analytics_pageviews \
         where site_id = $1 and occurred_at >= $2 and occurred_at < $3 group by 1",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .bind(width)
    .fetch_all(pool)
    .await?;

    for (bucket, count) in visitors {
        if let Some(point) = bucket_mut(&mut series, bucket) {
            point.visitors = count;
        }
    }
    for (bucket, count) in pageviews {
        if let Some(point) = bucket_mut(&mut series, bucket) {
            point.pageviews = count;
        }
    }

    Ok(series)
}

/// The visitors and pageviews per day, from the daily rollups.
async fn series_rollup(pool: &PgPool, site_id: Uuid, range: DateRange) -> Result<Vec<SeriesPoint>> {
    let mut series = empty_series(range, Granularity::Day);

    let rows: Vec<(i32, String, i64)> = sqlx::query_as(
        "select (day - $2::date)::int as bucket, metric, coalesce(sum(analytics_daily.count), 0)::bigint \
         from analytics_daily \
         where site_id = $1 and day >= $2 and day <= $3 and dimension_kind = 'total' \
         and metric in ('visitors', 'pageviews') group by 1, 2",
    )
    .bind(site_id)
    .bind(range.from)
    .bind(range.to)
    .fetch_all(pool)
    .await?;

    for (bucket, metric, count) in rows {
        if let Some(point) = bucket_mut(&mut series, bucket) {
            match metric.as_str() {
                "visitors" => point.visitors = count,
                "pageviews" => point.pageviews = count,
                _ => {}
            }
        }
    }

    Ok(series)
}

/// The bucket at an offset, when the offset is inside the series.
fn bucket_mut(series: &mut [SeriesPoint], offset: i32) -> Option<&mut SeriesPoint> {
    usize::try_from(offset)
        .ok()
        .and_then(|index| series.get_mut(index))
}

/// The busiest pages of a range, from the raw rows.
async fn top_pages_exact(
    pool: &PgPool,
    site_id: Uuid,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<DimensionRow>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "select pages.path, count(distinct visits.visitor_hash)::bigint, count(*)::bigint \
         from analytics_pageviews pages join analytics_visits visits on visits.id = pages.visit_id \
         where pages.site_id = $1 and pages.occurred_at >= $2 and pages.occurred_at < $3 \
         group by pages.path order by 3 desc, 1 asc limit $4",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .bind(5_i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(value, visitors, views)| DimensionRow {
            value,
            visitors: Some(visitors),
            views: Some(views),
        })
        .collect())
}

/// The busiest sources of a range, from the raw rows.
async fn top_sources_exact(
    pool: &PgPool,
    site_id: Uuid,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<DimensionRow>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "select coalesce(nullif(source, ''), nullif(referrer_host, ''), '(direct)') as value, \
         count(distinct visitor_hash)::bigint, count(*)::bigint \
         from analytics_visits \
         where site_id = $1 and started_at >= $2 and started_at < $3 \
         group by 1 order by 2 desc, 1 asc limit $4",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .bind(5_i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(value, visitors, visits)| DimensionRow {
            value,
            visitors: Some(visitors),
            views: Some(visits),
        })
        .collect())
}

/// Visitors per device type, from the raw rows.
async fn devices_exact(
    pool: &PgPool,
    site_id: Uuid,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<DimensionRow>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "select coalesce(device_type, '(unknown)') as value, \
         count(distinct visitor_hash)::bigint, count(*)::bigint \
         from analytics_visits \
         where site_id = $1 and started_at >= $2 and started_at < $3 \
         group by 1 order by 2 desc, 1 asc limit $4",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .bind(6_i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(value, visitors, visits)| DimensionRow {
            value,
            visitors: Some(visitors),
            views: Some(visits),
        })
        .collect())
}

/// A ranked dimension from the daily rollups (the range reaches past retention).
async fn ranked_rollup(
    pool: &PgPool,
    site_id: Uuid,
    range: DateRange,
    metric: &str,
    dimension: &str,
    pageviews: bool,
) -> Result<Vec<DimensionRow>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select dimension_value, coalesce(sum(analytics_daily.count), 0)::bigint from analytics_daily \
         where site_id = $1 and day >= $2 and day <= $3 and metric = $4 and dimension_kind = $5 \
         group by 1 order by 2 desc, 1 asc limit $6",
    )
    .bind(site_id)
    .bind(range.from)
    .bind(range.to)
    .bind(metric)
    .bind(dimension)
    .bind(5_i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(value, count)| DimensionRow {
            value,
            visitors: if pageviews { None } else { Some(count) },
            views: if pageviews { Some(count) } else { None },
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// The page report
// ---------------------------------------------------------------------------------------------

/// Answer the page report for one site, range, filters, sort and page.
#[allow(clippy::too_many_arguments)]
pub async fn pages(
    pool: &PgPool,
    site_id: Uuid,
    range: DateRange,
    filters: &Filters,
    sort: Option<&str>,
    direction: Option<&str>,
    page: Option<i64>,
    per_page: Option<i64>,
) -> Result<PagesReport> {
    let (sort_key, sort_fragment, order) = page_sort(sort, direction)?;
    let page = page.unwrap_or(1).max(1);
    let per_page = per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE);
    let (from, to) = range.bounds();
    let narrowing = narrow_pages(filters);
    let clause = narrowing.clause();
    let limit = narrowing.next_index();
    let offset = limit + 1;

    let base = format!(
        "from analytics_pageviews pages join analytics_visits visits on visits.id = pages.visit_id \
         where pages.site_id = $1 and pages.occurred_at >= $2 and pages.occurred_at < $3{clause}"
    );

    let total_sql = format!(
        "select count(*)::bigint from (select pages.path {base} group by pages.path) grouped"
    );
    let mut total_query = sqlx::query_scalar::<_, i64>(&total_sql)
        .bind(site_id)
        .bind(from)
        .bind(to);
    for value in narrowing.binds() {
        total_query = total_query.bind(value);
    }
    let total: i64 = total_query.fetch_one(pool).await?;

    let rows_sql = format!(
        "select pages.path, \
         (array_agg(pages.title order by pages.occurred_at desc) \
          filter (where pages.title is not null))[1] as title, \
         count(*)::bigint as views, \
         count(distinct visits.visitor_hash)::bigint as visitors, \
         avg(pages.duration_ms)::float8 as avg_time_ms, \
         count(distinct visits.id)::bigint as visits, \
         (count(distinct visits.id) filter (where visits.is_bounce))::bigint as bounced_visits, \
         (count(*) filter (where pages.is_entry))::bigint as entrances, \
         (count(*) filter (where pages.is_exit))::bigint as exits \
         {base} group by pages.path order by {sort_fragment} {order}, pages.path asc \
         limit ${limit} offset ${offset}"
    );
    let mut query = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            i64,
            i64,
            Option<f64>,
            i64,
            i64,
            i64,
            i64,
        ),
    >(&rows_sql)
    .bind(site_id)
    .bind(from)
    .bind(to);
    for value in narrowing.binds() {
        query = query.bind(value);
    }
    let rows = query
        .bind(per_page)
        .bind((page - 1) * per_page)
        .fetch_all(pool)
        .await?;

    let rows = rows
        .into_iter()
        .map(
            |(
                path,
                title,
                views,
                visitors,
                avg_time_ms,
                visits,
                bounced_visits,
                entrances,
                exits,
            )| PageRow {
                path,
                title,
                views,
                visitors,
                views_per_visitor: ratio(views as f64, visitors as f64),
                avg_time_ms,
                bounce_rate: ratio(bounced_visits as f64, visits as f64),
                entrances,
                exits,
            },
        )
        .collect();

    Ok(PagesReport {
        range,
        filters: FilterEcho::from(filters),
        sort: sort_key.to_owned(),
        direction: order.to_owned(),
        page,
        per_page,
        total,
        rows,
    })
}

/// The sort key, its SQL fragment and its direction, closed to the documented set.
fn page_sort(
    sort: Option<&str>,
    direction: Option<&str>,
) -> Result<(&'static str, &'static str, &'static str)> {
    let wanted = sort.map(str::trim).filter(|value| !value.is_empty());
    let found = match wanted {
        None => PAGE_SORTS[0],
        Some(value) => PAGE_SORTS
            .iter()
            .find(|(name, _)| *name == value)
            .copied()
            .ok_or_else(|| {
                AnalyticsError::InvalidQuery(format!(
                    "sort \"{value}\" is not one of {}",
                    PAGE_SORTS
                        .iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?,
    };

    let order = match direction.map(str::trim) {
        None | Some("") => "desc",
        Some(value) if value.eq_ignore_ascii_case("desc") => "desc",
        Some(value) if value.eq_ignore_ascii_case("asc") => "asc",
        Some(other) => {
            return Err(AnalyticsError::InvalidQuery(format!(
                "direction \"{other}\" is not asc or desc"
            )));
        }
    };

    Ok((found.0, found.1, order))
}

/// A ratio, or `None` when the denominator is zero (an undefined rate is not `0`).
fn ratio(numerator: f64, denominator: f64) -> Option<f64> {
    if denominator > 0.0 {
        Some(numerator / denominator)
    } else {
        None
    }
}

/// The series of one path: visitors and pageviews per bucket.
pub async fn page_series(
    pool: &PgPool,
    site_id: Uuid,
    path: &str,
    range: DateRange,
    granularity: Granularity,
) -> Result<Vec<SeriesPoint>> {
    let (from, to) = range.bounds();
    let width = granularity.seconds();
    let mut series = empty_series(range, granularity);

    let rows: Vec<(i32, i64, i64)> = sqlx::query_as(
        "select floor(extract(epoch from (pages.occurred_at - $3::timestamptz)) / $5::float8)::int as bucket, \
         count(*)::bigint as views, \
         count(distinct visits.visitor_hash)::bigint as visitors \
         from analytics_pageviews pages join analytics_visits visits on visits.id = pages.visit_id \
         where pages.site_id = $1 and pages.path = $2 and pages.occurred_at >= $3 and pages.occurred_at < $4 \
         group by 1",
    )
    .bind(site_id)
    .bind(path)
    .bind(from)
    .bind(to)
    .bind(width)
    .fetch_all(pool)
    .await?;

    for (bucket, views, visitors) in rows {
        if let Some(point) = bucket_mut(&mut series, bucket) {
            point.pageviews = views;
            point.visitors = visitors;
        }
    }

    Ok(series)
}

// ---------------------------------------------------------------------------------------------
// The sources report
// ---------------------------------------------------------------------------------------------

/// Answer the sources report for one site, range, filters and group-by mode.
pub async fn sources(
    pool: &PgPool,
    site_id: Uuid,
    range: DateRange,
    filters: &Filters,
    group: Option<&str>,
) -> Result<SourcesReport> {
    let group = match group.map(str::trim).filter(|value| !value.is_empty()) {
        None => "combination",
        Some(value) => SOURCE_GROUPS
            .iter()
            .find(|candidate| **candidate == value)
            .copied()
            .ok_or_else(|| {
                AnalyticsError::InvalidQuery(format!(
                    "group \"{value}\" is not one of {}",
                    SOURCE_GROUPS.join(", ")
                ))
            })?,
    };

    let (from, to) = range.bounds();
    let narrowing = narrow_visits(filters);
    let clause = narrowing.clause();
    let limit = narrowing.next_index();

    let converters = "select distinct hits.visitor_hash from analytics_goal_hits hits \
         join analytics_goals goals on goals.id = hits.goal_id \
         where goals.site_id = $1 and hits.occurred_at >= $2 and hits.occurred_at < $3";

    let total_expression =
        "coalesce(nullif(visits.source, ''), nullif(visits.referrer_host, ''), '(direct)')";

    let sql = if group == "combination" {
        format!(
            "with converters as ({converters}) \
             select {total_expression} as source, \
             nullif(visits.medium, '') as medium, nullif(visits.campaign, '') as campaign, \
             nullif(visits.term, '') as term, nullif(visits.content, '') as content, \
             count(*)::bigint as visits, count(distinct visits.visitor_hash)::bigint as visitors, \
             count(distinct converters.visitor_hash)::bigint as conversions \
             from analytics_visits visits \
             left join converters on converters.visitor_hash = visits.visitor_hash \
             where visits.site_id = $1 and visits.started_at >= $2 and visits.started_at < $3{clause} \
             group by 1, 2, 3, 4, 5 order by 6 desc, 1 asc limit ${limit}"
        )
    } else {
        let expression = match group {
            "referrer" => "coalesce(nullif(visits.referrer_host, ''), '(direct)')",
            "medium" => "coalesce(nullif(visits.medium, ''), '(none)')",
            "campaign" => "coalesce(nullif(visits.campaign, ''), '(none)')",
            "term" => "coalesce(nullif(visits.term, ''), '(none)')",
            "content" => "coalesce(nullif(visits.content, ''), '(none)')",
            _ => total_expression,
        };
        format!(
            "with converters as ({converters}) \
             select {expression} as value, \
             count(*)::bigint as visits, count(distinct visits.visitor_hash)::bigint as visitors, \
             count(distinct converters.visitor_hash)::bigint as conversions \
             from analytics_visits visits \
             left join converters on converters.visitor_hash = visits.visitor_hash \
             where visits.site_id = $1 and visits.started_at >= $2 and visits.started_at < $3{clause} \
             group by 1 order by 3 desc, 1 asc limit ${limit}"
        )
    };

    // A grouped report selects one dimension; the combination selects five. Two shapes, two
    // decodings — the query and the type that reads it are written together.
    let rows: Vec<SourceRow> = if group == "combination" {
        type CombinationRow = (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            i64,
            i64,
        );
        let mut query = sqlx::query_as::<_, CombinationRow>(&sql)
            .bind(site_id)
            .bind(from)
            .bind(to);
        for value in narrowing.binds() {
            query = query.bind(value);
        }
        let rows = query.bind(MAX_RANKED_ROWS).fetch_all(pool).await?;

        rows.into_iter()
            .map(
                |(source, medium, campaign, term, content, visits, visitors, conversions)| {
                    SourceRow {
                        source,
                        medium,
                        campaign,
                        term,
                        content,
                        visits,
                        visitors,
                        conversions,
                        conversion_rate: ratio(conversions as f64, visitors as f64),
                    }
                },
            )
            .collect()
    } else {
        let mut query = sqlx::query_as::<_, (String, i64, i64, i64)>(&sql)
            .bind(site_id)
            .bind(from)
            .bind(to);
        for value in narrowing.binds() {
            query = query.bind(value);
        }
        let rows = query.bind(MAX_RANKED_ROWS).fetch_all(pool).await?;

        rows.into_iter()
            .map(|(source, visits, visitors, conversions)| SourceRow {
                source,
                medium: None,
                campaign: None,
                term: None,
                content: None,
                visits,
                visitors,
                conversions,
                conversion_rate: ratio(conversions as f64, visitors as f64),
            })
            .collect()
    };

    Ok(SourcesReport {
        range,
        group: group.to_owned(),
        rows,
    })
}

// ---------------------------------------------------------------------------------------------
// The audience report
// ---------------------------------------------------------------------------------------------

/// Answer the audience report for one site, range and filters.
pub async fn audience(
    pool: &PgPool,
    site_id: Uuid,
    range: DateRange,
    filters: &Filters,
) -> Result<AudienceReport> {
    let (from, to) = range.bounds();
    let narrowing = narrow_visits(filters);
    let clause = narrowing.clause();
    let limit = narrowing.next_index();

    let panels: [(&str, &str, &str); 5] = [
        (
            "device",
            "Devices",
            "coalesce(visits.device_type, '(unknown)')",
        ),
        (
            "browser",
            "Browsers",
            "coalesce(visits.browser, '(unknown)')",
        ),
        (
            "os",
            "Operating systems",
            "coalesce(visits.os, '(unknown)')",
        ),
        (
            "screen",
            "Screen sizes",
            "case when visits.screen_width is null or visits.screen_height is null \
             then '(unknown)' else visits.screen_width::text || '×' || visits.screen_height::text end",
        ),
        (
            "language",
            "Languages",
            "coalesce(nullif(visits.language, ''), '(unknown)')",
        ),
    ];

    let mut report = AudienceReport {
        range,
        panels: Vec::new(),
        countries: Vec::new(),
        visitors: 0,
    };

    for (kind, title, expression) in panels {
        let sql = format!(
            "select {expression} as value, count(distinct visits.visitor_hash)::bigint, count(*)::bigint \
             from analytics_visits visits \
             where visits.site_id = $1 and visits.started_at >= $2 and visits.started_at < $3{clause} \
             group by 1 order by 2 desc, 1 asc limit ${limit}"
        );
        let mut query = sqlx::query_as::<_, (String, i64, i64)>(&sql)
            .bind(site_id)
            .bind(from)
            .bind(to);
        for value in narrowing.binds() {
            query = query.bind(value);
        }
        let rows = query.bind(12_i64).fetch_all(pool).await?;

        if kind == "device" {
            report.visitors = rows.iter().map(|(_, visitors, _)| visitors).sum();
        }

        report.panels.push(DimensionPanel {
            kind: kind.to_owned(),
            title: title.to_owned(),
            rows: rows
                .into_iter()
                .map(|(value, visitors, visits)| DimensionRow {
                    value,
                    visitors: Some(visitors),
                    views: Some(visits),
                })
                .collect(),
        });
    }

    let sql = format!(
        "select coalesce(visits.country_code::text, '(unknown)') as code, \
         count(distinct visits.visitor_hash)::bigint, count(pages.id)::bigint \
         from analytics_visits visits \
         left join analytics_pageviews pages on pages.visit_id = visits.id \
          and pages.occurred_at >= $2 and pages.occurred_at < $3 \
         where visits.site_id = $1 and visits.started_at >= $2 and visits.started_at < $3{clause} \
         group by 1 order by 2 desc, 1 asc limit ${limit}"
    );
    let mut query = sqlx::query_as::<_, (String, i64, i64)>(&sql)
        .bind(site_id)
        .bind(from)
        .bind(to);
    for value in narrowing.binds() {
        query = query.bind(value);
    }
    let countries = query.bind(MAX_RANKED_ROWS).fetch_all(pool).await?;

    let total = report.visitors.max(1);
    report.countries = countries
        .into_iter()
        .map(|(code, visitors, views)| CountryRow {
            code,
            visitors,
            views,
            share: visitors as f64 / total as f64,
        })
        .collect();

    Ok(report)
}

// ---------------------------------------------------------------------------------------------
// Events, downloads, forms
// ---------------------------------------------------------------------------------------------

/// Answer the events report for one site, range and filters.
pub async fn events(
    pool: &PgPool,
    site_id: Uuid,
    range: DateRange,
    filters: &Filters,
) -> Result<EventsReport> {
    let (from, to) = range.bounds();
    let narrowing = narrow_events(filters);
    let clause = narrowing.clause();
    let limit = narrowing.next_index();

    let sql = format!(
        "select events.name, count(*)::bigint, \
         count(distinct visits.visitor_hash)::bigint, sum(events.value)::float8, \
         max(events.occurred_at) \
         from analytics_events events left join analytics_visits visits on visits.id = events.visit_id \
         where events.site_id = $1 and events.occurred_at >= $2 and events.occurred_at < $3{clause} \
         group by events.name order by 2 desc, 1 asc limit ${limit}"
    );
    let mut query =
        sqlx::query_as::<_, (String, i64, i64, Option<f64>, Option<OffsetDateTime>)>(&sql)
            .bind(site_id)
            .bind(from)
            .bind(to);
    for value in narrowing.binds() {
        query = query.bind(value);
    }
    let rows = query.bind(MAX_RANKED_ROWS).fetch_all(pool).await?;

    Ok(EventsReport {
        range,
        rows: rows
            .into_iter()
            .map(|(name, count, visitors, value_sum, last_seen)| EventRow {
                name,
                count,
                visitors,
                value_sum,
                last_seen,
            })
            .collect(),
    })
}

/// Answer one event in detail: its counts, its days and its property breakdown.
pub async fn event_detail(
    pool: &PgPool,
    site_id: Uuid,
    name: &str,
    range: DateRange,
) -> Result<EventDetail> {
    let (from, to) = range.bounds();

    let (count, visitors, value_sum, last_seen): (i64, i64, Option<f64>, Option<OffsetDateTime>) =
        sqlx::query_as(
            "select count(*)::bigint, count(distinct visits.visitor_hash)::bigint, \
             sum(events.value)::float8, max(events.occurred_at) \
             from analytics_events events left join analytics_visits visits on visits.id = events.visit_id \
             where events.site_id = $1 and events.name = $4 \
             and events.occurred_at >= $2 and events.occurred_at < $3",
        )
        .bind(site_id)
        .bind(from)
        .bind(to)
        .bind(name)
        .fetch_one(pool)
        .await?;

    let per_day: Vec<(i32, i64)> = sqlx::query_as(
        "select ((events.occurred_at at time zone 'UTC')::date - $2::date)::int as bucket, \
         count(*)::bigint \
         from analytics_events events \
         where events.site_id = $1 and events.name = $4 \
         and events.occurred_at >= $2 and events.occurred_at < $3 group by 1",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .bind(name)
    .fetch_all(pool)
    .await?;

    let mut series: Vec<CountPoint> = empty_series(range, Granularity::Day)
        .into_iter()
        .map(|point| CountPoint {
            bucket: point.bucket,
            label: point.label,
            count: 0,
        })
        .collect();
    for (bucket, count) in per_day {
        if let Some(point) = usize::try_from(bucket)
            .ok()
            .and_then(|index| series.get_mut(index))
        {
            point.count = count;
        }
    }

    let properties: Vec<(String, String, i64)> = sqlx::query_as(
        "select pair.key, pair.value, count(*)::bigint \
         from analytics_events events, jsonb_each_text(events.properties) as pair(key, value) \
         where events.site_id = $1 and events.name = $4 \
         and events.occurred_at >= $2 and events.occurred_at < $3 \
         group by 1, 2 order by 3 desc, 1 asc, 2 asc limit $5",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .bind(name)
    .bind(MAX_RANKED_ROWS)
    .fetch_all(pool)
    .await?;

    Ok(EventDetail {
        range,
        name: name.to_owned(),
        count,
        visitors,
        value_sum,
        last_seen,
        series,
        properties: properties
            .into_iter()
            .map(|(key, value, count)| PropertyRow { key, value, count })
            .collect(),
    })
}

/// Answer the downloads report: by file and by page.
pub async fn downloads(
    pool: &PgPool,
    site_id: Uuid,
    range: DateRange,
    filters: &Filters,
) -> Result<DownloadsReport> {
    let (from, to) = range.bounds();
    let narrowing = narrow_events(filters);
    let clause = narrowing.clause();
    let limit = narrowing.next_index();

    let base = format!(
        "from analytics_events events left join analytics_visits visits on visits.id = events.visit_id \
         where events.site_id = $1 and events.name = 'download' \
         and events.occurred_at >= $2 and events.occurred_at < $3{clause}"
    );

    let files_sql = format!(
        "select coalesce(nullif(events.properties->>'file', ''), '(unknown)') as value, \
         count(*)::bigint, count(distinct visits.visitor_hash)::bigint {base} \
         group by 1 order by 2 desc, 1 asc limit ${limit}"
    );
    let pages_sql = format!(
        "select coalesce(nullif(events.path, ''), '(unknown)') as value, \
         count(*)::bigint, count(distinct visits.visitor_hash)::bigint {base} \
         group by 1 order by 2 desc, 1 asc limit ${limit}"
    );

    let files = download_rows(pool, &files_sql, site_id, from, to, &narrowing).await?;
    let pages = download_rows(pool, &pages_sql, site_id, from, to, &narrowing).await?;
    let total: i64 = files.iter().map(|row| row.downloads).sum();

    Ok(DownloadsReport {
        range,
        total,
        files,
        pages,
    })
}

/// Run one downloads query and map its rows.
async fn download_rows(
    pool: &PgPool,
    sql: &str,
    site_id: Uuid,
    from: OffsetDateTime,
    to: OffsetDateTime,
    narrowing: &Narrowing,
) -> Result<Vec<DownloadRow>> {
    let mut query = sqlx::query_as::<_, (String, i64, i64)>(sql)
        .bind(site_id)
        .bind(from)
        .bind(to);
    for value in narrowing.binds() {
        query = query.bind(value);
    }
    let rows = query.bind(MAX_RANKED_ROWS).fetch_all(pool).await?;

    Ok(rows
        .into_iter()
        .map(|(value, downloads, visitors)| DownloadRow {
            value,
            downloads,
            visitors,
        })
        .collect())
}

/// Answer the forms report: submissions, completion and abandonment per form.
///
/// Completion is honest about what the site emits: it needs a `form_start` event for the same
/// form. A site that emits only submissions gets submissions and a dash — never an invented rate.
pub async fn forms(
    pool: &PgPool,
    site_id: Uuid,
    range: DateRange,
    filters: &Filters,
) -> Result<FormsReport> {
    let (from, to) = range.bounds();
    let narrowing = narrow_events(filters);
    let clause = narrowing.clause();
    let limit = narrowing.next_index();

    let sql = format!(
        "select coalesce(nullif(events.properties->>'form', ''), '(unnamed)') as form, \
         (count(*) filter (where events.name = 'form_submit'))::bigint as submissions, \
         (count(distinct visits.visitor_hash) filter (where events.name = 'form_submit'))::bigint as visitors, \
         (sum(events.value) filter (where events.name = 'form_submit'))::float8 as value_sum, \
         (count(*) filter (where events.name = 'form_start'))::bigint as starts, \
         max(events.occurred_at) filter (where events.name = 'form_submit') as last_seen \
         from analytics_events events left join analytics_visits visits on visits.id = events.visit_id \
         where events.site_id = $1 and events.name in ('form_submit', 'form_start') \
         and events.occurred_at >= $2 and events.occurred_at < $3{clause} \
         group by 1 order by 2 desc, 1 asc limit ${limit}"
    );
    let mut query =
        sqlx::query_as::<_, (String, i64, i64, Option<f64>, i64, Option<OffsetDateTime>)>(&sql)
            .bind(site_id)
            .bind(from)
            .bind(to);
    for value in narrowing.binds() {
        query = query.bind(value);
    }
    let rows = query.bind(MAX_RANKED_ROWS).fetch_all(pool).await?;

    Ok(FormsReport {
        range,
        rows: rows
            .into_iter()
            .map(
                |(form, submissions, visitors, value_sum, starts, last_seen)| FormRow {
                    form,
                    submissions,
                    visitors,
                    value_sum,
                    starts,
                    completion_rate: ratio(submissions as f64, starts as f64),
                    abandonment: if starts > 0 {
                        Some(starts - submissions)
                    } else {
                        None
                    },
                    last_seen,
                },
            )
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: u8, day: u8) -> Date {
        Date::from_calendar_date(year, Month::try_from(month).unwrap(), day).unwrap()
    }

    fn range(from: (i32, u8, u8), to: (i32, u8, u8)) -> DateRange {
        DateRange::new(day(from.0, from.1, from.2), day(to.0, to.1, to.2)).unwrap()
    }

    #[test]
    fn a_range_knows_its_length_and_the_period_before_it() {
        let week = range((2026, 9, 20), (2026, 9, 26));
        assert_eq!(week.days(), 7);

        let previous = week.previous();
        assert_eq!(previous.to, day(2026, 9, 19));
        assert_eq!(previous.from, day(2026, 9, 13));
        assert_eq!(
            previous.days(),
            7,
            "the comparison period is the same length"
        );
    }

    #[test]
    fn one_day_and_a_year_of_days_both_answer() {
        let single = range((2026, 9, 26), (2026, 9, 26));
        assert_eq!(single.days(), 1);
        assert_eq!(single.previous().to, day(2026, 9, 25));

        let year = range((2025, 9, 27), (2026, 9, 26));
        assert_eq!(year.days(), 365);

        assert!(DateRange::new(day(2026, 9, 26), day(2026, 9, 20)).is_err());
        assert!(DateRange::new(day(2024, 1, 1), day(2026, 1, 2)).is_err());
    }

    #[test]
    fn the_bounds_are_midnights_and_the_day_is_utc() {
        let (from, to) = range((2026, 9, 26), (2026, 9, 26)).bounds();
        assert_eq!(from.date(), day(2026, 9, 26));
        assert_eq!(to.date(), day(2026, 9, 27));
        assert_eq!(from.hour(), 0);
        assert_eq!((to - from).whole_hours(), 24);
    }

    #[test]
    fn granularity_follows_the_range_and_stays_closed() {
        let short = range((2026, 9, 25), (2026, 9, 26));
        let long = range((2026, 9, 1), (2026, 9, 26));
        assert_eq!(Granularity::auto(short), Granularity::Hour);
        assert_eq!(Granularity::auto(long), Granularity::Day);
        assert_eq!(
            Granularity::parse(Some("HOUR"), short).unwrap(),
            Granularity::Hour
        );
        assert_eq!(Granularity::parse(None, long).unwrap(), Granularity::Day);
        assert!(Granularity::parse(Some("week"), long).is_err());
    }

    #[test]
    fn a_bucket_list_covers_the_whole_range_without_gaps() {
        let days = empty_series(range((2026, 9, 24), (2026, 9, 26)), Granularity::Day);
        assert_eq!(days.len(), 3);
        assert_eq!(days[0].bucket, "2026-09-24");
        assert_eq!(days[0].label, "Sep 24");
        assert_eq!(days[2].bucket, "2026-09-26");

        let hours = empty_series(range((2026, 9, 26), (2026, 9, 26)), Granularity::Hour);
        assert_eq!(hours.len(), 24, "a day is 24 hourly buckets");
        assert_eq!(hours[0].label, "00:00");
        assert_eq!(hours[23].label, "23:00");
        assert_eq!(hours[5].bucket, "2026-09-26T05:00Z");
    }

    #[test]
    fn filters_are_validated_before_they_reach_sql() {
        let ok = Filters::new(
            Some("/pricing".to_owned()),
            None,
            Some("Mobile".to_owned()),
            Some("tr".to_owned()),
            None,
        )
        .unwrap();
        assert_eq!(ok.device.as_deref(), Some("mobile"));
        assert_eq!(ok.country.as_deref(), Some("TR"));
        assert!(!ok.is_empty());

        assert!(Filters::new(None, None, Some("toaster".to_owned()), None, None).is_err());
        assert!(Filters::new(None, None, None, Some("TUR".to_owned()), None).is_err());
        assert!(
            Filters::new(
                Some("x".repeat(MAX_FILTER_LENGTH + 1)),
                None,
                None,
                None,
                None
            )
            .is_err()
        );
        assert!(
            Filters::new(Some("   ".to_owned()), None, None, None, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_narrowing_only_writes_the_clauses_in_use() {
        let empty = narrow_pages(&Filters::default());
        assert!(empty.is_empty());
        assert_eq!(empty.clause(), "");
        assert_eq!(empty.next_index(), 4, "with no filters the limit is $4");

        let filters = Filters::new(
            Some("/qa".to_owned()),
            None,
            Some("mobile".to_owned()),
            None,
            Some("newsletter".to_owned()),
        )
        .unwrap();
        let narrowing = narrow_pages(&filters);
        assert_eq!(narrowing.len(), 3);
        assert!(
            narrowing
                .clause()
                .contains("pages.path ilike '%' || $4 || '%'")
        );
        assert!(
            narrowing
                .clause()
                .contains("coalesce(visits.device_type, '') = $5")
        );
        assert!(narrowing.clause().contains("$6"));
        assert!(
            !narrowing.clause().contains("$7"),
            "an unused filter binds nothing"
        );
        assert_eq!(narrowing.next_index(), 7);

        let visits = narrow_visits(&filters);
        assert!(
            visits
                .clause()
                .contains("exists (select 1 from analytics_pageviews pages"),
            "a visit is narrowed by the paths it viewed"
        );
        let events = narrow_events(&filters);
        assert!(events.clause().contains("coalesce(events.path, '')"));
    }

    #[test]
    fn a_page_sort_is_taken_from_the_whitelist_or_refused() {
        assert_eq!(page_sort(None, None).unwrap(), ("views", "views", "desc"));
        assert_eq!(
            page_sort(Some("avg_time"), Some("ASC")).unwrap(),
            ("avg_time", "avg_time_ms", "asc")
        );
        assert_eq!(
            page_sort(Some("bounce_rate"), None).unwrap().1,
            "bounced_visits::float8 / greatest(visits, 1)"
        );
        assert!(page_sort(Some("views; drop table users"), None).is_err());
        assert!(page_sort(Some("views"), Some("sideways")).is_err());
    }

    #[test]
    fn a_rate_with_no_denominator_is_no_rate() {
        assert_eq!(ratio(3.0, 4.0), Some(0.75));
        assert_eq!(ratio(0.0, 0.0), None);
    }

    #[test]
    fn csv_fields_are_quoted_only_when_they_must_be() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("/qa/landing"), "/qa/landing");
        assert_eq!(csv_field("Omnion, Inc"), "\"Omnion, Inc\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("two\nlines"), "\"two\nlines\"");
        assert_eq!(csv_field(" padded "), "\" padded \"");
    }

    #[test]
    fn a_report_renders_the_rows_it_showed() {
        let report = Report::Pages(PagesReport {
            range: range((2026, 9, 26), (2026, 9, 26)),
            filters: FilterEcho::default(),
            sort: "views".to_owned(),
            direction: "desc".to_owned(),
            page: 1,
            per_page: 50,
            total: 1,
            rows: vec![PageRow {
                path: "/qa/landing, two".to_owned(),
                title: Some("QA \"landing\"".to_owned()),
                views: 12,
                visitors: 3,
                views_per_visitor: Some(4.0),
                avg_time_ms: Some(1800.5),
                bounce_rate: Some(0.5),
                entrances: 2,
                exits: 1,
            }],
        });
        assert_eq!(report.key(), "pages");

        let csv = report.csv();
        let lines: Vec<&str> = csv.trim_end().split('\n').collect();
        assert_eq!(lines.len(), 2, "one header and one row");
        assert!(lines[0].starts_with("path,title,views,visitors"));
        assert!(lines[1].contains("\"/qa/landing, two\""));
        assert!(lines[1].contains("\"QA \"\"landing\"\"\""));
        assert!(
            lines[1].contains("1800.5"),
            "a half-millisecond is not rounded away"
        );
    }

    #[test]
    fn an_absent_number_is_an_empty_field_never_a_zero() {
        assert_eq!(number(None), "");
        assert_eq!(number(Some(4.0)), "4");
        assert_eq!(number(Some(0.5)), "0.5");
        assert_eq!(number(Some(f64::NAN)), "");
        assert_eq!(instant(None), "");
    }

    #[test]
    fn a_day_parses_only_in_the_shape_it_is_written() {
        assert_eq!(day_from_str("2026-09-26"), Some(day(2026, 9, 26)));
        assert_eq!(day_from_str(" 2026-01-01 "), Some(day(2026, 1, 1)));
        assert_eq!(day_from_str("2026-13-01"), None);
        assert_eq!(day_from_str("2026-09-31"), None);
        assert_eq!(day_from_str("26-09-2026"), None);
        assert_eq!(day_from_str("today"), None);
    }
}
