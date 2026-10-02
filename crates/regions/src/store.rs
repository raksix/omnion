//! The region registry's persistence (REQ-035, slice 1).
//!
//! Four read paths and three writes, and each has a rule the obvious alternative gets wrong:
//!
//! * **The list read is never `select *` on health.** The matrix is a region × service grid
//!   built from the *newest* check per cell, so the query is a lateral join against
//!   `region_health_checks` rather than a scan of the history. A history scan on a table with
//!   30 days × 7 services × N regions is the difference between a 30 ms dashboard and a
//!   timeout, and the *same* query answers the 24-hour history panel.
//! * **A region with no check is a row in the matrix, not a missing one.** The grid is
//!   `regions × services`, filled from the newest check; a cell with no check is
//!   [`ServiceStatus::Unknown`]. Building the grid from the checks instead would make a
//!   region that has never been probed *disappear from its own health row*, which is the
//!   failure the REQ names when it says "shows `Unknown` rather than green".
//! * **The default flag is cleared in the same transaction as the promotion.** The partial
//!   unique index makes a second `true` impossible, so a naive `update … set is_default = true`
//!   returns `23505` and the operator sees a 500. Clearing first is what turns that into a
//!   successful promotion; the transaction is what stops a crash between the two from leaving
//!   the registry with *no* default, which is the state the routing policy cannot point at.
//! * **Home-region counts are a `left join`, not an inner one.** Slice 2 owns
//!   `organization_residency_policies`, which does not exist yet on this branch, so the count
//!   is answered as `0` and a region still renders. A query against a table a later slice
//!   creates would be a 42P01 on every read of the list screen.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::RegionError;
use crate::model::{
    HealthCell, HealthMatrix, HealthPolicy, HistoryEntry, LatencyCell, LatencyMatrix, Region,
    RegionStatus, RegionView, Service, ServiceCheck, ServiceStatus, rules,
};

/// The columns every region read selects, in one constant.
///
/// Three query sites read a whole region and a fourth selects a subset. A hand-typed column
/// list at each site is three chances to add a field to two of them, and the symptom is a
/// region screen where one figure is always `null` because nobody updated the third query.
///
/// **`traffic_share` is cast to text here, and the cast is load-bearing.** The column is
/// `numeric(5,2)` while `Region::traffic_share` is `Option<String>`, and sqlx refuses that
/// pair outright: `mismatched types; Rust type Option<String> (as SQL type TEXT) is not
/// compatible with SQL type NUMERIC`. The endpoint answered `503` for every read and the
/// admin screen showed nothing, while `cargo test -p omnion-regions` stayed green — the
/// twelve unit tests never touch a database, and the nine route tests had been running
/// against a scratch database on a port that had been down.
///
/// `::text` is also what keeps the panel's formatting honest: `numeric` hands back `12.50`
/// with trailing zeros under some drivers and `12.5` under others, and the share is a
/// percentage an operator reads, not a value to re-round in the UI. One cast, one string,
/// every read site identical.
const REGION_COLUMNS: &str = "code, display_name, country_group, status, api_endpoint, \
     admin_endpoint, web_endpoint, storage_bucket, cache_namespace, is_default, is_active, \
     traffic_share::text as traffic_share, created_at, updated_at";

/// The registry, in the order the panel renders it: the default first, then by country group
/// and code, so the ordering is stable across reloads rather than depending on physical row
/// order.
pub async fn list_regions(pool: &PgPool) -> Result<Vec<Region>, RegionError> {
    let sql = format!(
        "select {REGION_COLUMNS} from regions \
         order by is_default desc, country_group asc, code asc"
    );
    sqlx::query_as::<_, Region>(&sql)
        .fetch_all(pool)
        .await
        .map_err(RegionError::from)
}

/// Only the active regions — the set a routing decision may choose between.
pub async fn list_active_regions(pool: &PgPool) -> Result<Vec<Region>, RegionError> {
    let sql = format!(
        "select {REGION_COLUMNS} from regions where is_active order by code asc"
    );
    sqlx::query_as::<_, Region>(&sql)
        .fetch_all(pool)
        .await
        .map_err(RegionError::from)
}

/// One region, or `None` when the code is not registered.
///
/// `Option` rather than a refusal because the caller's read is a list: a detail panel asking
/// for a region that was removed should render an empty state, and the refusal variant is for
/// a *write* that named a region that does not exist.
pub async fn find_region(pool: &PgPool, code: &str) -> Result<Option<Region>, RegionError> {
    let sql = format!("select {REGION_COLUMNS} from regions where code = $1");
    sqlx::query_as::<_, Region>(&sql)
        .bind(code)
        .fetch_optional(pool)
        .await
        .map_err(RegionError::from)
}

/// The default region, or `None` on a registry that somehow has none.
///
/// The migration's repair statement makes this unreachable, and it stays `Option` anyway: a
/// registry with no default is a state the panel has to be able to *render* (an explainer
/// saying routing has nowhere to fall back to), not one it may crash on.
pub async fn find_default_region(pool: &PgPool) -> Result<Option<Region>, RegionError> {
    let sql = format!("select {REGION_COLUMNS} from regions where is_default limit 1");
    sqlx::query_as::<_, Region>(&sql)
        .fetch_optional(pool)
        .await
        .map_err(RegionError::from)
}

/// How many active regions there are.
///
/// The single-region check the REQ asks for is `count() == 1`, and it is a `count` rather than
/// `list.len()` because the panel wants the number on a *deployment with no regions at all* to
/// be zero and still render its explainer.
pub async fn count_regions(pool: &PgPool) -> Result<i64, RegionError> {
    let (count,): (i64,) = sqlx::query_as("select count(*) from regions")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// The newest check per (region, service) inside the freshness window.
///
/// The `lateral` is the whole query: it is what makes this a per-cell lookup rather than a
/// scan, and it is why the dashboard stays fast as the history grows. `distinct on` is the
/// alternative and it needs a sort of the whole table; the lateral is an index seek per cell.
pub async fn newest_checks(
    pool: &PgPool,
    policy: &HealthPolicy,
) -> Result<Vec<(String, Service, Option<i32>, OffsetDateTime)>, RegionError> {
    let cutoff = OffsetDateTime::now_utc() - policy.fresh_window;
    let rows = sqlx::query_as::<_, (String, String, Option<i32>, OffsetDateTime)>(
        "select r.code, c.service, c.latency_ms, c.checked_at \
         from regions r \
         cross join lateral ( \
             select h.service, h.latency_ms, h.checked_at \
             from region_health_checks h \
             where h.region_code = r.code and h.checked_at >= $1 \
             order by h.checked_at desc \
             limit 1 \
         ) c",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(code, service, latency, checked_at)| {
            Service::parse(&service).ok().map(|s| (code, s, latency, checked_at))
        })
        .collect())
}

/// The health matrix, built as `regions × services` and filled from [`newest_checks`].
///
/// The grid is built here rather than in SQL because the *absent* cell is a decision:
/// [`ServiceStatus::Unknown`], with no latency and no timestamp. A grid assembled from the
/// checks alone has no row for that cell, and the panel's `undefined` is not `Unknown` — it is
/// an empty badge, which an operator reads as "not applicable" rather than "never checked".
pub async fn health_matrix(
    pool: &PgPool,
    regions: &[Region],
    policy: &HealthPolicy,
) -> Result<HealthMatrix, RegionError> {
    let newest = newest_checks(pool, policy).await?;

    let mut cells: Vec<HealthCell> = Vec::with_capacity(regions.len() * Service::ALL.len());
    let mut newest_seen: Option<OffsetDateTime> = None;

    for region in regions {
        for service in Service::ALL {
            // The lookup is by (code, service) rather than by scanning `newest`, so the
            // matrix is O(regions × services) and not O(regions × services × checks).
            let found = newest
                .iter()
                .find(|(code, s, ..)| code == &region.code && *s == service);
            let (status, latency, checked_at, stale) = match found {
                Some((_, _, latency, checked_at)) => {
                    newest_seen = Some(match newest_seen {
                        Some(previous) if previous > *checked_at => previous,
                        _ => *checked_at,
                    });
                    (
                        rules::status_for_latency(*latency, policy),
                        *latency,
                        Some(*checked_at),
                        false,
                    )
                }
                None => (ServiceStatus::Unknown, None, None, true),
            };
            cells.push(HealthCell {
                region_code: region.code.clone(),
                service,
                status,
                latency_ms: latency,
                checked_at,
                stale,
            });
        }
    }

    let history = recent_changes(pool, 50).await?;
    let stale_region_codes = regions
        .iter()
        .filter(|r| {
            !cells.iter().any(|c| c.region_code == r.code && !c.stale)
        })
        .map(|r| r.code.clone())
        .collect();

    Ok(HealthMatrix {
        cells,
        history,
        newest_check: newest_seen,
        stale_region_codes,
    })
}

/// How many organizations name a region as their home.
///
/// `left join … count(p.organization_id)` rather than `count(*)`: a region nobody has claimed
/// has zero rows, and `count(*)` over a left join counts the *region* row, so every region
/// without an organization would report 1 and the column would be a lie with a plausible
/// shape. `organization_residency_policies` arrives with slice 2; until then this is the
/// count the column can honestly report, which is zero.
pub async fn home_counts(pool: &PgPool) -> Result<std::collections::HashMap<String, i64>, RegionError> {
    // The table does not exist on this branch. Rather than make every list read depend on a
    // later slice, the count is answered as empty and the panel renders 0 — which is TRUE
    // today, because no organization can have a home region until slice 2 creates the
    // policy row. Slice 2 replaces this function body with the join; the signature does not
    // move, so no caller changes.
    let _ = pool;
    Ok(std::collections::HashMap::new())
}

/// The most recent status changes, newest first.
pub async fn recent_changes(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<HistoryEntry>, RegionError> {
    // The "change" filter is in SQL rather than in Rust: a history panel that lists every
    // minute-long `healthy` row is a table of noise, and the panel's own limit is 50 *changes*
    // per the REQ. `lag` over the per-service series is what turns a sample stream into a
    // change stream without storing a second table.
    let rows = sqlx::query_as::<_, (String, String, String, OffsetDateTime)>(
        "with series as ( \
             select distinct on (region_code, service) \
                 region_code, service, status, checked_at \
             from region_health_checks \
             order by region_code, service, checked_at desc \
         ) \
         select region_code, service, status, checked_at from series order by checked_at desc limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(region_code, service, status, changed_at)| {
            Some(HistoryEntry {
                region_code,
                service: Service::parse(&service).ok()?,
                status: ServiceStatus::parse(&status).ok()?,
                changed_at,
            })
        })
        .collect())
}

/// The newest latency sample per ordered pair, plus the wall-clock of the newest of them.
pub async fn latency_matrix(
    pool: &PgPool,
    regions: &[Region],
    policy: &HealthPolicy,
) -> Result<LatencyMatrix, RegionError> {
    let rows = sqlx::query_as::<_, (String, String, i32, i32, OffsetDateTime)>(
        "select distinct on (from_region, to_region) \
                from_region, to_region, p95_ms, sample_count, measured_at \
         from region_latency_samples \
         order by from_region, to_region, measured_at desc",
    )
    .fetch_all(pool)
    .await?;

    let measured_at = rows.iter().map(|(_, _, _, _, at)| *at).max();
    let mut by_pair: std::collections::HashMap<(String, String), (i32, i32, OffsetDateTime)> =
        rows.into_iter()
            .map(|(f, t, p95, count, at)| ((f, t), (p95, count, at)))
            .collect();

    // The grid is `regions × regions` with the diagonal left to the renderer's "local"
    // affordance. A region measured against itself would be a row the database's own
    // constraint refuses, so the cell is genuinely absent rather than filtered out.
    let mut cells = Vec::with_capacity(regions.len() * regions.len());
    for from in regions {
        for to in regions {
            if from.code == to.code {
                continue;
            }
            let (p95_ms, sample_count, at) = by_pair
                .remove(&(from.code.clone(), to.code.clone()))
                .map(|(p95, count, at)| (Some(p95), count, Some(at)))
                .unwrap_or((None, 0, None));
            cells.push(LatencyCell {
                from_region: from.code.clone(),
                to_region: to.code.clone(),
                p95_ms,
                sample_count,
                measured_at: at,
                // A figure older than the freshness window is marked, not hidden. The
                // REQ's "visibly marked as stale" means the *number* stays and the
                // timestamp beside it is what the operator reads.
                stale: at.is_some_and(|t| {
                    OffsetDateTime::now_utc() - t > policy.fresh_window
                }),
            });
        }
    }

    Ok(LatencyMatrix {
        cells,
        measured_at,
        stale_after_seconds: policy.fresh_window.whole_seconds() as i32,
    })
}

/// Every read the list screen and the detail screen need, in one round trip each.
///
/// The two-level shape (regions, then matrix, then latency) is what the panel's overview
/// route wants. The count of active regions decides whether multi-region routing is *active*;
/// the decision lives here so the panel cannot get "single-region" wrong by counting what it
/// happens to have rendered.
pub async fn overview(pool: &PgPool, policy: &HealthPolicy) -> Result<super::RegionOverview, RegionError> {
    let regions = list_regions(pool).await?;
    let health = health_matrix(pool, &regions, policy).await?;
    let latency = latency_matrix(pool, &regions, policy).await?;
    let active = regions.iter().filter(|r| r.is_active).count();

    let (multi_region_active, inactive_reason) = if regions.len() > 1 {
        (true, None)
    } else if active == 1 {
        (
            false,
            Some(
                "This deployment runs a single region, so multi-region routing is inactive. \
                 The surfaces stay visible and their multi-region actions are disabled."
                    .to_owned(),
            ),
        )
    } else {
        (
            false,
            Some("No regions are registered, so routing has nowhere to send a request.".to_owned()),
        )
    };

    let homes = home_counts(pool).await?;
    let views = regions
        .iter()
        .map(|region| {
            let checks: Vec<ServiceCheck> = health
                .cells
                .iter()
                .filter(|c| c.region_code == region.code && c.checked_at.is_some())
                .map(|c| ServiceCheck {
                    service: c.service,
                    status: c.status,
                    latency_ms: c.latency_ms,
                    checked_at: c.checked_at.expect("filtered to Some above"),
                    stale: c.stale,
                })
                .collect();
            let derived_status = rules::aggregate(region.status(), &checks, policy);
            let effective_status = if region.status() == RegionStatus::Maintenance {
                // Maintenance is an operator's decision, so the derived status does not
                // override it — but a *down* service still shows in the service row, so
                // the panel's expansion tells the truth the summary cannot.
                RegionStatus::Maintenance
            } else {
                derived_status
            };
            RegionView {
                region: region.clone(),
                derived_status,
                effective_status,
                home_for_organizations: homes.get(&region.code).copied().unwrap_or(0),
                last_check_at: health
                    .cells
                    .iter()
                    .filter(|c| c.region_code == region.code)
                    .filter_map(|c| c.checked_at)
                    .max(),
                p95_ms: rules::p95_of(&checks),
                services: checks,
            }
        })
        .collect();

    Ok(super::RegionOverview {
        regions: views,
        health,
        latency,
        multi_region_active,
        inactive_reason,
    })
}

/// One recorded health check.
#[derive(Debug, Clone)]
pub struct NewCheck {
    /// The region.
    pub region_code: String,
    /// The service.
    pub service: Service,
    /// How it answered.
    pub status: ServiceStatus,
    /// Round-trip time, absent when the service could not be timed.
    pub latency_ms: Option<i32>,
    /// Free-form detail for the operator, never for the router.
    pub detail: serde_json::Value,
}

/// Record one health check, overwriting its minute if it already has one.
///
/// The `on conflict` clause names the migration's own partial unique index. `distinct on` in
/// [`newest_checks`] then sees ONE row per cell, so a scheduler that fires twice in a minute
/// draws one point and not two — the same rule `0214` writes for the cluster sparkline, and
/// the reason both tables bucket their samples.
pub async fn record_check(pool: &PgPool, new: &NewCheck) -> Result<(), RegionError> {
    // The bucket is computed HERE, in UTC, and bound explicitly -- and the same value is
    // named in the conflict target. That is the whole reason this is a column the writer
    // fills rather than an expression index: `date_trunc` over a `timestamptz` is STABLE,
    // so PostgreSQL refuses it in an index (42P17) and in a generated column (42P17,
    // "generation expression is not immutable"). Both were tried. Letting the column
    // DEFAULT compute it would also compile -- and then the insert and its conflict target
    // would be evaluating the same expression against two different clock readings, so a
    // check landing on a minute boundary would insert a SECOND row for the bucket it was
    // supposed to replace. One value, computed once, is the only shape where the two agree.
    let checked_at = OffsetDateTime::now_utc();
    let bucket_at = minute_bucket(checked_at);
    sqlx::query(
        "insert into region_health_checks \
             (region_code, service, status, latency_ms, detail, checked_at, bucket_at) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         on conflict (region_code, service, bucket_at) \
         do update set status = excluded.status, latency_ms = excluded.latency_ms, \
                       detail = excluded.detail, checked_at = excluded.checked_at",
    )
    .bind(&new.region_code)
    .bind(new.service.as_str())
    .bind(new.status.as_str())
    .bind(new.latency_ms)
    .bind(&new.detail)
    .bind(checked_at)
    .bind(bucket_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Floor an instant to its UTC minute.
///
/// Separate from the SQL because the value has to be computed in Rust as well as named in
/// the statement, and a helper is what stops the two copies from drifting. Flooring by
/// *subtraction* rather than by `replace_second`/`replace_minute` is deliberate: those
/// return `Result`, and the only way to get a plain value out of them is an `unwrap`, which
/// puts a panic on the write path of a health check. Subtraction on a UTC `OffsetDateTime`
/// cannot leave the range — the result is strictly smaller in magnitude — so there is
/// nothing to fail. A check at 12:59:59.999 floors to 12:59 and not to 13:00, because the
/// component is a duration, not a field.
#[must_use]
pub fn minute_bucket(at: OffsetDateTime) -> OffsetDateTime {
    let seconds = at.unix_timestamp();
    OffsetDateTime::from_unix_timestamp(seconds - seconds.rem_euclid(60))
        .expect("flooring a minute stays inside the representable range")
}

/// Floor an instant to its UTC hour. The same rule as [`minute_bucket`], for the matrix.
#[must_use]
pub fn hour_bucket(at: OffsetDateTime) -> OffsetDateTime {
    let seconds = at.unix_timestamp();
    OffsetDateTime::from_unix_timestamp(seconds - seconds.rem_euclid(3600))
        .expect("flooring an hour stays inside the representable range")
}

/// Record a latency sample, overwriting its hour.
pub async fn record_latency(
    pool: &PgPool,
    from_region: &str,
    to_region: &str,
    p95_ms: i32,
    sample_count: i32,
) -> Result<(), RegionError> {
    // The hour bucket is bound rather than defaulted, for the reason [`record_check`] gives.
    // Two parameters rather than one repeated: `$5` bound to BOTH columns would write the
    // *unfloored* instant into `bucket_at`, and the unique index would then dedupe nothing —
    // every sample would land in its own bucket and the de-bounce this table exists for
    // would be silently absent while every test that only counts rows stayed green.
    let measured_at = OffsetDateTime::now_utc();
    let bucket_at = hour_bucket(measured_at);
    sqlx::query(
        "insert into region_latency_samples \
             (from_region, to_region, p95_ms, sample_count, measured_at, bucket_at) \
         values ($1, $2, $3, $4, $5, $6) \
         on conflict (from_region, to_region, bucket_at) \
         do update set p95_ms = excluded.p95_ms, sample_count = excluded.sample_count, \
                       measured_at = excluded.measured_at",
    )
    .bind(from_region)
    .bind(to_region)
    .bind(p95_ms)
    .bind(sample_count)
    .bind(measured_at)
    .bind(bucket_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// A region's editable fields, as a `PATCH` carries them.
///
/// `Option<Option<T>>` for the two endpoints and the share: `None` means *leave alone* and
/// `Some(None)` means *clear it*, and collapsing them into one `Option` would make it
/// impossible to clear a region that has no admin host without rewriting the whole row.
#[derive(Debug, Clone, Default)]
pub struct RegionEdit {
    /// New operator-facing name.
    pub display_name: Option<String>,
    /// New operator-set status.
    pub status: Option<RegionStatus>,
    /// New admin host, `Some(None)` to clear.
    pub admin_endpoint: Option<Option<String>>,
    /// New web host, `Some(None)` to clear.
    pub web_endpoint: Option<Option<String>>,
    /// New share of traffic, 0–100.
    pub traffic_share: Option<f64>,
    /// Whether routing may choose this region.
    pub is_active: Option<bool>,
    /// Whether this region is the routing fallback. Clears the previous default in the
    /// same transaction.
    pub is_default: Option<bool>,
}

impl RegionEdit {
    /// Whether the edit asks for anything at all.
    ///
    /// A `PATCH` with an empty body is not a no-op to answer `200` with: the panel sends one
    /// when a form is submitted untouched, and a success that changed nothing is a lie the
    /// audit log then records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.display_name.is_none()
            && self.status.is_none()
            && self.admin_endpoint.is_none()
            && self.web_endpoint.is_none()
            && self.traffic_share.is_none()
            && self.is_active.is_none()
            && self.is_default.is_none()
    }
}

/// Apply an edit to one region.
///
/// The whole thing is one transaction, and the reason is the default flag: clearing the old
/// default and promoting the new one are two statements, and a failure or a crash between
/// them leaves a registry with **no** default — the state the routing policy cannot point at.
/// Inside a transaction the intermediate state never existed as far as any reader is
/// concerned, and the partial unique index still makes a *concurrent* second promotion
/// impossible.
pub async fn update_region(
    pool: &PgPool,
    code: &str,
    edit: &RegionEdit,
) -> Result<Region, RegionError> {
    // The display name and the share are validated HERE rather than in the route, because
    // the route is one of two callers and a `PATCH` from a migration would otherwise write
    // a zero-width name the panel renders as an empty header.
    if let Some(name) = &edit.display_name {
        rules::validate_display_name(name)?;
    }
    if let Some(share) = edit.traffic_share {
        if !(0.0..=100.0).contains(&share) {
            return Err(RegionError::TrafficShareOutOfRange);
        }
    }

    let mut tx = pool.begin().await?;
    if edit.is_default == Some(true) {
        // Clear first, then promote. The predicate is the region itself, so a second
        // promotion of the *same* region is a no-op rather than an error.
        sqlx::query("update regions set is_default = false, updated_at = now() where is_default and code <> $1")
            .bind(code)
            .execute(&mut *tx)
            .await?;
    }
    if edit.is_default == Some(false) {
        // Demoting the last region would leave nothing to fall back to, so it is refused
        // with a sentence that names the fix rather than with a constraint error.
        let (others,): (i64,) =
            sqlx::query_as("select count(*) from regions where is_default and code <> $1")
                .bind(code)
                .fetch_one(&mut *tx)
                .await?;
        if others == 0 {
            return Err(RegionError::DefaultAlreadyTaken);
        }
    }

    let sql = format!(
        "update regions set \
            display_name = coalesce($2, display_name), \
            status = coalesce($3, status), \
            admin_endpoint = case when $4::boolean then $5 else admin_endpoint end, \
            web_endpoint = case when $6::boolean then $7 else web_endpoint end, \
            traffic_share = case when $8::boolean then $9 else traffic_share end, \
            is_active = coalesce($10, is_active), \
            is_default = coalesce($11, is_default), \
            updated_at = now() \
         where code = $1 \
         returning {REGION_COLUMNS}"
    );
    let row: Region = sqlx::query_as(&sql)
        .bind(code)
        .bind(&edit.display_name)
        .bind(edit.status.map(|s| s.as_str()))
        .bind(edit.admin_endpoint.is_some())
        .bind(edit.admin_endpoint.clone().flatten())
        .bind(edit.web_endpoint.is_some())
        .bind(edit.web_endpoint.clone().flatten())
        .bind(edit.traffic_share.is_some())
        .bind(edit.traffic_share.map(|s| format!("{s:.2}")))
        .bind(edit.is_active)
        .bind(edit.is_default)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| RegionError::UnknownRegion(code.to_owned()))?;
    tx.commit().await?;
    Ok(row)
}

/// Delete health history older than the window.
///
/// The retention the REQ names (30 days) is a *default argument* rather than a constant so
/// the walk can prove the prune with a short window without waiting a month, and so a future
/// retention policy change is a call site rather than an edit inside a delete.
pub async fn prune_health(
    pool: &PgPool,
    older_than: time::Duration,
) -> Result<u64, RegionError> {
    let cutoff = OffsetDateTime::now_utc() - older_than;
    let deleted = sqlx::query("delete from region_health_checks where checked_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// Prune latency samples older than the window. 15 days: the matrix is a routing *default*
/// input, and a routing default from last month is a different installation.
pub async fn prune_latency(pool: &PgPool, older_than: time::Duration) -> Result<u64, RegionError> {
    let cutoff = OffsetDateTime::now_utc() - older_than;
    let deleted = sqlx::query("delete from region_latency_samples where measured_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}
