//! `/api/v1/content-api/usage` — what the tokens have been doing (REQ-019, slice 3).
//!
//! The panel's usage tab, and nothing else. It reads the durable table slice 1's migration
//! created and this slice finally writes, and it **adds the live window to it** — because the
//! table only holds what has been flushed, and a person who has just opened the Explorer to
//! "make some real calls" and then clicked Usage would see an empty chart for up to a flush
//! interval, and would conclude the feature is broken rather than that it is asynchronous.
//!
//! **The two halves are reported separately rather than summed into one number.** A summed total
//! is friendlier and is the thing that goes wrong: a chart that says "1,204 requests" when 1,200
//! of them are durable and 4 are unflushed cannot be reconciled with either source, so an
//! operator comparing the chart against an access log has nothing to compare. `flushed` and
//! `pending` are two numbers that each mean something, and their sum is the honest total.
//!
//! The whole answer is one read per table plus one Redis scan, and the Redis scan is the same
//! `content_meter::window` the flush uses — so what the panel shows and what the worker will
//! write are computed from the same keys by the same parser.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use omnion_content::api_token_usage::{self, DailyUsage};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::content_meter;
use crate::error::ApiError;
use crate::auth::CurrentSession;
use crate::routes::content_api::organization_of;
use crate::state::AppState;

/// `GET /api/v1/content-api/usage?days=14`
#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    /// How many days of history to report, 1–365. The panel asks for its window; an integrator
    /// asking for more than the retention keeps is told the window is clamped rather than
    /// refused, because "how much do you have" is not a question with a wrong answer.
    #[serde(default)]
    pub days: Option<i32>,
}

/// One token's line in the usage view.
#[derive(Debug, Serialize)]
pub struct TokenUsage {
    /// The token.
    pub token_id: Uuid,
    /// Its name, so the row is readable without a second lookup.
    pub name: String,
    /// Requests already flushed into the table.
    pub flushed_requests: i32,
    /// Errors already flushed.
    pub flushed_errors: i32,
    /// Refusals already flushed.
    pub flushed_throttled: i32,
    /// Requests counted but not yet flushed — the live window.
    pub pending_requests: i32,
    /// Refusals counted but not yet flushed.
    pub pending_throttled: i32,
}

/// The whole answer: the durable rows, the live window and the roll-up the chart draws.
#[derive(Debug, Serialize)]
pub struct UsageBody {
    /// Days the answer covers.
    pub days: i32,
    /// The durable rows, newest day first.
    pub rows: Vec<DailyUsageRow>,
    /// Per-token totals, the table under the chart.
    pub tokens: Vec<TokenUsage>,
    /// The chart's series: one bar per day, oldest first, across every token.
    pub series: Vec<DayTotal>,
    /// Whether the live window could be read at all.
    ///
    /// `false` means Redis was unreachable, so `pending` is **not zero** — it is nothing. A panel
    /// that rendered it as "0 pending" would be telling an operator nothing is in flight while
    /// the counter is unreachable, which is the same lie `Counted::authoritative` refuses.
    pub pending_readable: bool,
    /// Requests in the live window, across every token, when it could be read.
    pub pending_requests: Option<i32>,
    /// Refusals in the live window, when it could be read.
    pub pending_throttled: Option<i32>,
}

/// One durable row, with the endpoint a person reads.
#[derive(Debug, Serialize)]
pub struct DailyUsageRow {
    /// The token.
    pub token_id: Uuid,
    /// ISO day.
    pub day: String,
    /// Matched route.
    pub endpoint: String,
    /// Requests.
    pub requests: i32,
    /// Errors.
    pub errors: i32,
    /// Refusals.
    pub throttled: i32,
}

impl From<DailyUsage> for DailyUsageRow {
    fn from(row: DailyUsage) -> Self {
        Self {
            token_id: row.token_id,
            day: row.day.to_string(),
            endpoint: row.endpoint,
            requests: row.requests,
            errors: row.errors,
            throttled: row.throttled,
        }
    }
}

/// One bar.
#[derive(Debug, Serialize)]
pub struct DayTotal {
    /// ISO day.
    pub day: String,
    /// Requests that day, across every token.
    pub requests: i32,
    /// Refusals that day.
    pub throttled: i32,
}

/// `GET /api/v1/content-api/usage` — every token's usage for the last N days.
pub async fn usage(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
    current: CurrentSession,
) -> Result<Json<UsageBody>, ApiError> {
    let organization_id = organization_of(&current);
    let days = query.days.unwrap_or(DEFAULT_WINDOW_DAYS).clamp(1, 365);

    let rows = api_token_usage::for_organization(state.db().pool(), organization_id, days)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("reading the content usage: {error}"),
            )
        })?;

    // The tokens' names, in one read. A name per row would be a lookup per token, and the usage
    // view is the one screen where a person reads *many* tokens at once.
    let names: std::collections::HashMap<Uuid, String> = sqlx::query(
        "select id, name from api_tokens where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_all(state.db().pool())
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("reading the content tokens: {error}"),
        )
    })?
    .into_iter()
    .map(|row| {
        (
            row.get::<Uuid, _>("id"),
            row.get::<String, _>("name"),
        )
    })
    .collect();

    let mut tokens: Vec<TokenUsage> = names
        .iter()
        .map(|(id, name)| TokenUsage {
            token_id: *id,
            name: name.clone(),
            flushed_requests: 0,
            flushed_errors: 0,
            flushed_throttled: 0,
            pending_requests: 0,
            pending_throttled: 0,
        })
        .collect();
    let mut index: std::collections::HashMap<Uuid, usize> = names
        .keys()
        .enumerate()
        .map(|(position, id)| (*id, position))
        .collect();

    for row in &rows {
        let Some(position) = index.get(&row.token_id).copied() else {
            // A usage row whose token is not in this organization cannot reach the response: the
            // query joins on `organization_id`, so this arm is a guard, not a case. Skipping beats
            // a panic on a row the query already filtered.
            continue;
        };
        let token = &mut tokens[position];
        token.flushed_requests += row.requests;
        token.flushed_errors += row.errors;
        token.flushed_throttled += row.throttled;
    }

    // The live window, keyed the same way the flush keys it.
    let live = content_meter::window(state.redis()).await;
    let pending_readable = live_readable(&state).await;
    let mut pending_requests = 0;
    let mut pending_throttled = 0;
    for (key, counts) in &live {
        let Some(bucket) = content_meter::BucketKey::parse(key) else {
            continue;
        };
        // A counter for a token of another organization is never summed into this answer: the
        // keys are global in Redis, and "total requests on this box" is not what anyone asked.
        let Some(position) = index.get(&bucket.token_id).copied() else {
            continue;
        };
        tokens[position].pending_requests += counts.requests;
        tokens[position].pending_throttled += counts.throttled;
        pending_requests += counts.requests;
        pending_throttled += counts.throttled;
    }

    // The chart's series: one bar per day, oldest first, zero-filled so a quiet day is a short
    // bar and not a gap. A chart whose x-axis skips a day is a chart a person reads as "the
    // integration stopped", which is the opposite of what an empty day means.
    let series = series_for(&rows, days);

    let pending_requests = pending_readable.then_some(pending_requests);
    let pending_throttled = pending_readable.then_some(pending_throttled);

    Ok(Json(UsageBody {
        days,
        rows: rows.into_iter().map(DailyUsageRow::from).collect(),
        tokens,
        series,
        pending_readable,
        pending_requests,
        pending_throttled,
    }))
}

/// Whether the live window can be read at all.
///
/// One `PING`, not an inference from an empty scan: an empty scan is the *normal* state of an
/// installation nobody has called since the last flush, and treating it as "readable, zero
/// pending" is what this function exists to prevent being the wrong answer to.
async fn live_readable(state: &AppState) -> bool {
    state.redis().ping().await.is_ok()
}

/// The chart's bars: one per day over the window, oldest first, gaps filled with zeroes.
fn series_for(rows: &[DailyUsage], days: i32) -> Vec<DayTotal> {
    use std::collections::BTreeMap;

    let mut per_day: BTreeMap<time::Date, DayTotal> = BTreeMap::new();
    for row in rows {
        let entry = per_day.entry(row.day).or_insert_with(|| DayTotal {
            day: row.day.to_string(),
            requests: 0,
            throttled: 0,
        });
        entry.requests += row.requests;
        entry.throttled += row.throttled;
    }
    // Zero-fill the days with no rows, **most recent first** so the vector is then reversed into
    // chronological order. Walking backwards means the day count is exactly `days` even when the
    // newest day has no rows at all — which is the case on a Monday morning, and the case where a
    // chart that started at the newest row would show an empty chart until 00:01.
    let today = time::OffsetDateTime::now_utc().date();
    let mut filled = Vec::with_capacity(days.max(0) as usize);
    for offset in 0..days {
        let Some(day) = today.checked_sub(time::Duration::days(i64::from(offset))) else {
            break;
        };
        filled.push(per_day.remove(&day).unwrap_or(DayTotal {
            day: day.to_string(),
            requests: 0,
            throttled: 0,
        }));
    }
    filled.reverse();
    filled
}

/// How many days the usage tab shows when it does not ask for a window.
pub const DEFAULT_WINDOW_DAYS: i32 = 14;

#[cfg(test)]
mod tests {
    use super::*;
    use time::Date;
    use uuid::Uuid;

    fn row(day: Date, requests: i32) -> DailyUsage {
        DailyUsage {
            token_id: Uuid::from_u128(1),
            day,
            endpoint: "/content/pages".to_owned(),
            requests,
            errors: 0,
            throttled: 0,
        }
    }

    #[test]
    fn the_series_has_one_bar_per_day_even_when_no_day_has_rows() {
        // The empty case is the one a person meets first, and it is the one a "group by what is
        // there" implementation gets wrong: it returns an empty chart, which reads as "the
        // integration is broken" rather than "nobody has called yet".
        let bars = series_for(&[], 7);
        assert_eq!(bars.len(), 7, "seven days, seven bars, zero of them with rows");
        assert!(bars.iter().all(|bar| bar.requests == 0));
        // Oldest first, because a chart drawn newest-first reads backwards.
        assert!(
            bars.windows(2).all(|pair| pair[0].day < pair[1].day),
            "the bars run oldest to newest: {:?}",
            bars.iter().map(|bar| &bar.day).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_series_sums_a_day_across_every_row_that_belongs_to_it() {
        // Several endpoints on one day is the normal shape of a real integration, and a chart
        // with one bar per endpoint-row would be a chart of nothing.
        let day = time::OffsetDateTime::now_utc().date();
        let mut second = row(day, 7);
        second.endpoint = "/content/media".to_owned();
        let bars = series_for(&[row(day, 5), second], 3);
        let today = bars.last().expect("three bars, the last is today");
        assert_eq!(today.requests, 12, "both endpoints, one day, one bar");
    }

    #[test]
    fn a_day_outside_the_window_is_not_drawn() {
        // The window is a filter, not a decoration: a bar for a day the caller did not ask about
        // is a number they cannot reconcile with the rows they were also given.
        let stale = time::OffsetDateTime::now_utc().date() - time::Duration::days(40);
        let bars = series_for(&[row(stale, 9_999)], 7);
        assert!(
            bars.iter().all(|bar| bar.requests == 0),
            "a row from 40 days ago must not reach a 7-day chart"
        );
    }
}
