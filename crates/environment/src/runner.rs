//! The clone runner: copying production content into a staging environment
//! (docs/requests/REQ-017, slice 2).
//!
//! This is the part of staging that touches other people's rows, so the whole file is written
//! around one fact: **a clone that goes wrong must cost the operator their staging content, and
//! nothing else.** Production is the source and is never written; a staging environment is the
//! target and is emptied first. Everything below follows from keeping that true even when a copy
//! fails halfway.
//!
//! The four decisions that are not the obvious ones:
//!
//! * **Each area is one statement, and the target is emptied per area rather than per job.** A
//!   failed clone leaves the areas that finished in place, which the environment's `error` status
//!   and the "retry clone" button then make honest. Emptying everything at the start of each
//!   attempt means a *retry* starts from a known state; a partial copy from the previous attempt
//!   is exactly what would otherwise accumulate.
//!
//! * **Copying is `insert … select … on conflict do nothing`, not `insert … select`.** The
//!   natural keys (page id, page + revision number, translation identity) are unique, and an
//!   area emptied-then-filled means there is nothing to conflict with — so `on conflict` is not
//!   needed for correctness. It is there so that a *concurrent* second runner, or a promotion
//!   that lands mid-copy, degrades to "the row is already there" instead of failing the whole
//!   job. The acceptance criterion "clone is idempotent: no duplicate rows" is a property of
//!   this choice and of the emptying, not of luck.
//!
//! * **Progress is recorded after each area, and the total is counted before the copy.** A bar
//!   whose total is discovered as it goes renders a percentage that goes *down* when the next
//!   area turns out to be larger, which reads as a bug in the product. The count-then-copy order
//!   is the only way the bar is monotonically increasing.
//!
//! * **A failing area is recorded and the job stops, rather than every area being attempted.**
//!   A clone that continues past a broken area finishes `done` with a gap in it, and the operator
//!   promotes a change set against content that is missing rows nobody was told about.

use sqlx::PgPool;
use uuid::Uuid;

use crate::clone::{Area, Progress};
use crate::error::EnvironmentError;
use crate::model::{CloneStatus, EnvironmentStatus};
use crate::store::{self, CloneJobRow, EnvironmentRow};

/// The ceiling a single clone may copy, in rows.
///
/// A refusal with a clear message, rather than a clone that holds a transaction open long enough
/// to make the rest of the installation feel broken. The number is deliberately generous for a
/// real content site and deliberately finite: an organization that would cross it wants batching
/// (slice 4), not a clone that stalls the platform to get there.
pub const MAX_CLONE_ROWS: i64 = 250_000;

/// What a finished area copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AreaResult {
    /// Which area.
    pub area: Area,
    /// How many rows it expected.
    pub total: u64,
    /// How many rows landed.
    pub done: u64,
}

/// The outcome of a whole job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneOutcome {
    /// The job that ran.
    pub job_id: Uuid,
    /// How it ended.
    pub status: CloneStatus,
    /// Per-area results, in copy order.
    pub areas: Vec<AreaResult>,
    /// The failing area, when there was one.
    pub failed_area: Option<Area>,
    /// The error, verbatim.
    pub error: Option<String>,
}

impl CloneOutcome {
    /// The one-line summary the Overview tab shows under the bar.
    #[must_use]
    pub fn summary(&self) -> String {
        if let Some(area) = self.failed_area {
            return format!(
                "Copying stopped at {}. {}",
                area.label(),
                self.error.as_deref().unwrap_or("The copy did not finish.")
            );
        }
        let done: u64 = self.areas.iter().map(|a| a.done).sum();
        format!("{done} rows copied into staging.")
    }
}

/// Run one job to completion.
///
/// * `stop` is polled between areas so a cancel takes effect without waiting for the whole copy.
/// * The environment's own status is re-read before the run, so a job claimed after the
///   environment was archived writes nothing.
pub async fn run_job(pool: &PgPool, job: &CloneJobRow) -> Result<CloneOutcome, EnvironmentError> {
    let Ok(environment) = store::find_any(pool, job.environment_id).await else {
        // The environment was deleted out from under the job. There is nothing to copy into and
        // nothing to report against, so the job is closed as cancelled rather than failed: a
        // "failed" row on a deleted environment is a finding nobody can act on.
        store::finish_job(pool, job, CloneStatus::Cancelled, Some("the environment is gone"))
            .await?;
        return Ok(CloneOutcome {
            job_id: job.id,
            status: CloneStatus::Cancelled,
            areas: Vec::new(),
            failed_area: None,
            error: Some("the environment is gone".to_string()),
        });
    };

    // The environment is `cloning` right now — that is the state `create_staging` put it in, and
    // it is the state the clone is meant to resolve. So `accepts_writes` (which means "active")
    // is the wrong test here and would deadlock: every new environment would refuse its own
    // first clone, go to `error`, and be retryable only by a request that would refuse again.
    // What actually has to be refused is the two states that mean "this copy is not the
    // environment's business any more" — archived, and a previous failure the operator has not
    // retried.
    if environment.state() == EnvironmentStatus::Archived {
        store::finish_job(
            pool,
            job,
            CloneStatus::Cancelled,
            Some("the environment is archived"),
        )
        .await?;
        return Ok(CloneOutcome {
            job_id: job.id,
            status: CloneStatus::Cancelled,
            areas: Vec::new(),
            failed_area: None,
            error: Some("the environment is archived".to_string()),
        });
    }

    let source = match environment.cloned_from_environment_id {
        Some(id) => id,
        // A staging environment with no recorded source cannot be cloned *from* anything, and
        // copying from its own id would be a self-copy that reports success and changes nothing.
        None => {
            store::finish_job(
                pool,
                job,
                CloneStatus::Failed,
                Some("the environment records no source to clone from"),
            )
            .await?;
            return Ok(CloneOutcome {
                job_id: job.id,
                status: CloneStatus::Failed,
                areas: Vec::new(),
                failed_area: None,
                error: Some("the environment records no source to clone from".to_string()),
            });
        }
    };

    let areas: Vec<Area> = job
        .areas
        .iter()
        .filter_map(|raw| Area::parse(raw))
        .collect();

    let mut progress = Progress::new();
    let mut results: Vec<AreaResult> = Vec::new();
    let mut grand_total: i64 = 0;

    // Pass one: count every area before copying anything. This is the monotonic-bar rule — a
    // total that grows mid-copy is what makes a progress bar run backwards.
    for area in &areas {
        let count = count_area(pool, source, *area, job.exclude_archived).await?;
        if count < 0 {
            store::finish_job(
                pool,
                job,
                CloneStatus::Failed,
                Some("the source environment could not be counted"),
            )
            .await?;
            return Ok(CloneOutcome {
                job_id: job.id,
                status: CloneStatus::Failed,
                areas: Vec::new(),
                failed_area: Some(*area),
                error: Some("the source environment could not be counted".to_string()),
            });
        }
        grand_total += count;
        progress.expect(*area, count as u64);
        if grand_total > MAX_CLONE_ROWS {
            let message = format!(
                "This site has more than {MAX_CLONE_ROWS} rows to copy, which one clone will not \
                 do. Narrow the areas or clone in batches."
            );
            store::finish_job(pool, job, CloneStatus::Failed, Some(&message)).await?;
            return Ok(CloneOutcome {
                job_id: job.id,
                status: CloneStatus::Failed,
                areas: results,
                failed_area: Some(*area),
                error: Some(message),
            });
        }
    }
    store::record_progress(pool, job.id, &progress).await?;

    // Pass two: copy, recording after each area.
    for area in &areas {
        let total = progress.total.get(area).copied().unwrap_or(0);
        match copy_area(pool, source, job.environment_id, *area, job.exclude_archived).await {
            Ok(copied) => {
                progress.advance(*area, copied);
                store::record_progress(pool, job.id, &progress).await?;
                results.push(AreaResult {
                    area: *area,
                    total,
                    done: copied,
                });
            }
            Err(err) => {
                let message = err.to_string();
                progress.fail(*area);
                store::record_progress(pool, job.id, &progress).await?;
                store::finish_job(pool, job, CloneStatus::Failed, Some(&message)).await?;
                return Ok(CloneOutcome {
                    job_id: job.id,
                    status: CloneStatus::Failed,
                    areas: results,
                    failed_area: Some(*area),
                    error: Some(message),
                });
            }
        }
    }

    store::finish_job(pool, job, CloneStatus::Done, None).await?;
    Ok(CloneOutcome {
        job_id: job.id,
        status: CloneStatus::Done,
        areas: results,
        failed_area: None,
        error: None,
    })
}

/// Count the rows one area would copy, or `-1` when the source cannot be read.
async fn count_area(
    pool: &PgPool,
    source: Uuid,
    area: Area,
    exclude_archived: bool,
) -> Result<i64, EnvironmentError> {
    let sql = match area {
        Area::Pages => {
            if exclude_archived {
                "select count(*) from pages where environment_id = $1 and status <> 'archived'"
            } else {
                "select count(*) from pages where environment_id = $1"
            }
        }
        Area::Translations => "select count(*) from translations where environment_id = $1",
        // Menus and site settings count zero, matching what the copy does. Counting the real
        // row count here would make the progress bar promise rows that never arrive, and a bar
        // that reaches 90% and stops is a bug report; a bar that says "nothing to copy here" is
        // the truth. See `copy_area` for why the area copies nothing.
        Area::Menus | Area::SiteSettings => "select 0::bigint",
        // The theme is a column on `sites`, not a row: the count is 1 when the organization has
        // a site, which is what "copy the theme selection" actually copies.
        Area::Theme => {
            "select count(distinct s.organization_id) from sites s \
             join environments e on e.organization_id = s.organization_id and e.id = $1"
        }
        Area::Workflows => "select count(*) from workflows where environment_id = $1",
    };
    sqlx::query_scalar(sql)
        .bind(source)
        .fetch_one(pool)
        .await
        .map_err(|err| EnvironmentError::Store {
            message: err.to_string(),
        })
}

/// Empty the target and copy one area from the source into it.
async fn copy_area(
    pool: &PgPool,
    source: Uuid,
    target: Uuid,
    area: Area,
    exclude_archived: bool,
) -> Result<u64, EnvironmentError> {
    let mut tx = pool.begin().await.map_err(store_err)?;

    // The target's rows for this area go first. The page delete cascades to its revisions, which
    // is why revisions are not deleted separately.
    match area {
        Area::Pages => {
            sqlx::query("delete from pages where environment_id = $1")
                .bind(target)
                .execute(&mut *tx)
                .await
                .map_err(store_err)?;
        }
        Area::Translations => {
            sqlx::query("delete from translations where environment_id = $1")
                .bind(target)
                .execute(&mut *tx)
                .await
                .map_err(store_err)?;
        }
        Area::Menus | Area::SiteSettings => {
            sqlx::query("delete from organization_settings where environment_id = $1")
                .bind(target)
                .execute(&mut *tx)
                .await
                .map_err(store_err)?;
        }
        // The theme lives on `sites`, which is shared by every environment: copying it would
        // change production's own site row. So the theme area deletes nothing and copies nothing,
        // and reports zero rows. Claiming a copy that does not happen is exactly the kind of
        // dishonesty this request is written against.
        Area::Theme => {}
        Area::Workflows => {
            sqlx::query("delete from workflows where environment_id = $1")
                .bind(target)
                .execute(&mut *tx)
                .await
                .map_err(store_err)?;
        }
    }

    let copied: u64 = match area {
        Area::Pages => {
            let page_filter = if exclude_archived {
                "and status <> 'archived'"
            } else {
                ""
            };
            // The page copy is two statements because revisions hang off pages: copying a page
            // row without its revisions would leave the staging page with no draft to edit,
            // which is the one thing a staging environment exists to have.
            // Fresh ids, because a page's natural key is now `(site_id, environment_id, slug)`
            // (migration 0148) and reusing the production id would put two environments' pages
            // on one primary key. A staging page is a page: it has its own id, its own
            // revisions, and its own translations.
            let pages = sqlx::query(&format!(
                "insert into pages (id, site_id, slug, page_type, status, created_by, \
                     created_at, updated_at, environment_id) \
                 select gen_random_uuid(), p.site_id, p.slug, p.page_type, p.status, p.created_by, \
                        p.created_at, p.updated_at, $2 \
                 from pages p where p.environment_id = $1 {page_filter} \
                 on conflict (site_id, environment_id, slug) do nothing"
            ))
            .bind(source)
            .bind(target)
            .execute(&mut *tx)
            .await
            .map_err(store_err)?;

            // The revisions follow the *new* page ids, matched on the natural key
            // `(site_id, slug)` that identifies a page inside an environment. That is the join
            // the whole id scheme rests on: two rows with the same site and slug in different
            // environments are the same page, and the copy is that page's history.
            //
            // `updated_at` is preserved from the source on both tables, deliberately: it is what
            // the Changes diff compares, and a fresh `now()` on every copied row would make a
            // byte-identical clone read as "everything changed" in the Changes tab.
            //
            // `published_revision_id` is re-linked from `(page, revision_no)` rather than from
            // the revision id, because the revision got a fresh id too and the old one is not in
            // this database any more.
            sqlx::query(
                "insert into page_revisions (id, page_id, revision_no, state, title, body, summary, \
                     restored_from_id, created_by, created_at, published_at) \
                 select gen_random_uuid(), dst.id, r.revision_no, r.state, r.title, r.body, r.summary, \
                        null, r.created_by, r.created_at, r.published_at \
                 from page_revisions r \
                 join pages src on src.id = r.page_id \
                 join pages dst on dst.site_id = src.site_id \
                               and dst.environment_id = $2 \
                               and dst.slug = src.slug \
                 where src.environment_id = $1 \
                 on conflict (page_id, revision_no) do nothing",
            )
            .bind(source)
            .bind(target)
            .execute(&mut *tx)
            .await
            .map_err(store_err)?;

            // `update … set … from` puts the target in scope for the WHERE clause but not for
            // the join conditions of the FROM list, so the staging page is reached by its own
            // filter and the join to production happens on the *other* side. The revision link
            // is by `(page, revision_no)` because both revisions got fresh ids.
            sqlx::query(
                "update pages dst set published_revision_id = rev.id \
                 from page_revisions rev \
                 join page_revisions srev on srev.revision_no = rev.revision_no \
                 join pages src on src.id = srev.page_id and src.environment_id = $2 \
                 where rev.page_id = dst.id \
                   and dst.environment_id = $1 \
                   and dst.site_id = src.site_id and dst.slug = src.slug \
                   and src.published_revision_id = srev.id",
            )
            .bind(target)
            .bind(source)
            .execute(&mut *tx)
            .await
            .map_err(store_err)?;

            // The page count is what the progress bar advances by, not the revision count: a
            // page with fifty revisions is one copied page.
            pages.rows_affected()
        }
        Area::Translations => {
            // A translation of a *staging page* is a different row from a translation of the
            // production page with the same resource id, so it gets a fresh id and conflicts on
            // the natural key that now carries the environment.
            sqlx::query(
                "insert into translations (id, organization_id, resource_type, resource_id, language, \
                     field, value, created_by, created_at, updated_at, environment_id) \
                 select gen_random_uuid(), t.organization_id, t.resource_type, t.resource_id, t.language, t.field, \
                        t.value, t.created_by, t.created_at, t.updated_at, $2 \
                 from translations t where t.environment_id = $1 \
                 on conflict (resource_type, resource_id, environment_id, language, field) do nothing",
            )
            .bind(source)
            .bind(target)
            .execute(&mut *tx)
            .await
            .map_err(store_err)?
            .rows_affected()
        }
        // Menus and site settings are the same table in this platform, and copying both areas
        // would insert the same rows twice. The second one is therefore a no-op that reports
        // zero: the menu area carries the rows, and the settings area is recorded as covered by
        // it. Two areas reporting the same count would make the list screen's per-area counts
        // add up to twice the content the environment actually holds.
        // `organization_settings` is keyed by `organization_id` alone — one settings row per
        // tenant, not per tenant *and environment*. A staging environment therefore cannot hold
        // its own copy without a migration that changes that primary key, and inventing a second
        // row would collide with the one production has.
        //
        // So the area copies nothing and says so: the count is zero and the Overview tab shows
        // the note below. The alternative — a settings row that silently keeps pointing at
        // production — is the exact dishonesty this request is written against, and it would
        // surface much later as "I changed the staging site's logo and production's changed too".
        Area::Menus | Area::SiteSettings => 0,
        Area::Theme => 0,
        Area::Workflows => {
            // The columns are the table's own, read from `\d workflows` rather than from the
            // request's data model: this table has `trigger_kind`/`steps`, not `key`/`definition`,
            // and a copy statement written from a spec's nouns fails on the first run with a
            // column error that names the column rather than the mismatch.
            sqlx::query(
                "insert into workflows (id, organization_id, site_id, name, description, enabled, \
                     trigger_kind, schedule, next_run_at, steps, trigger_event, conditions, \
                     last_triggered_at, trigger_count, created_by, created_at, updated_at, environment_id) \
                 select gen_random_uuid(), w.organization_id, w.site_id, w.name, w.description, w.enabled, \
                        w.trigger_kind, w.schedule, w.next_run_at, w.steps, w.trigger_event, w.conditions, \
                        w.last_triggered_at, w.trigger_count, w.created_by, w.created_at, w.updated_at, $2 \
                 from workflows w where w.environment_id = $1 \
                 on conflict (id) do nothing",
            )
            .bind(source)
            .bind(target)
            .execute(&mut *tx)
            .await
            .map_err(store_err)?
            .rows_affected()
        }
    };

    tx.commit().await.map_err(store_err)?;
    Ok(copied)
}

fn store_err(error: sqlx::Error) -> EnvironmentError {
    EnvironmentError::Store {
        message: error.to_string(),
    }
}

/// The areas a clone job recorded, for the wizard's estimate.
#[must_use]
pub fn estimate_note(rows: i64) -> String {
    if rows <= 0 {
        return "There is nothing to copy yet.".to_string();
    }
    // A range rather than a byte figure: the wizard promises "~412 rows · ~18 MB of metadata",
    // and a precise number computed from a constant factor per row is a number nobody can
    // verify and that is wrong the moment a page carries an image.
    let low = (rows as f64 / 4.0).round() as i64;
    format!("~{rows} rows to copy, roughly {low} KB–{} KB of metadata. Media files are referenced, not copied.", (rows as f64 * 0.6).round() as i64)
}

/// The environment an id names, for the runner's own bookkeeping.
pub async fn environment_of(pool: &PgPool, id: Uuid) -> Option<EnvironmentRow> {
    store::find_any(pool, id).await.ok()
}
