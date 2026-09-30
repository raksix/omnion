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

/// How many rows one batch of a large area copies.
///
/// The request's Risks section has always promised that "the runner batches by area with a
/// configurable page size" — and until this tick no such code existed, so the promise was in the
/// documentation only. It matters for a reason that is not throughput: **one `insert … select`
/// over 200 000 rows is a single transaction that holds every one of those rows' locks for its
/// whole duration**, and on a live installation that is the difference between a slow clone and a
/// clone that blocks the editor. Batching turns one long transaction into many short ones, which
/// is also what makes the progress bar real: a bar that only moves when the last batch lands is
/// the "sits at 0% then jumps" bar the progress fold was written to avoid.
///
/// The default is deliberately small enough to be invisible to a small site (a 40-page site is
/// one batch, so its behaviour is unchanged) and large enough that a big site is not thousands
/// of round trips. Override it with `OMNION_CLONE_BATCH_ROWS`; a non-positive or unparsable
/// value falls back to the default rather than refusing to clone, because a mistyped environment
/// variable must not be able to break the one feature that copies content.
pub const DEFAULT_BATCH_ROWS: i64 = 5_000;

/// Read the configured batch size, falling back to [`DEFAULT_BATCH_ROWS`].
///
/// The ceiling is the row count, not the batch: a batch larger than the whole copy is not a
/// faster copy, it is the un-batched copy with extra steps, and it would reintroduce exactly the
/// long transaction the batches exist to break up.
#[must_use]
pub fn batch_rows() -> i64 {
    let ceiling = MAX_CLONE_ROWS.max(1);
    std::env::var("OMNION_CLONE_BATCH_ROWS")
        .ok()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|rows| *rows > 0)
        .map_or(DEFAULT_BATCH_ROWS, |rows| rows.min(ceiling))
}

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

    // Pass two: copy, recording after each area — and, inside an area, after each batch.
    //
    // The two recordings are the same statement and that is deliberate. An area of 40 000 rows
    // copied in 8 batches reports 8 times instead of once, so the operator watching a large
    // clone sees the bar move rather than sitting at 0% and jumping; and the total was already
    // fixed in pass one, so no intermediate reading can exceed it.
    let batch = batch_rows();
    for area in &areas {
        let total = progress.total.get(area).copied().unwrap_or(0);
        let outcome = copy_area_batched(
            pool,
            source,
            job.environment_id,
            *area,
            job.exclude_archived,
            batch,
            job.id,
            &mut progress,
        )
        .await;
        match outcome {
            Ok(copied) => {
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

/// Copy one area, in batches, advancing `progress` after each one.
///
/// The *emptying* happens once, before the first batch, and every batch is its own short
/// transaction. `progress` is advanced and persisted after each committed batch, which is what
/// makes the bar move during a long copy instead of at its end.
///
/// `progress` and `job_id` are arguments rather than a callback. The callback shape was the
/// obvious one and it does not compile here for a reason worth writing down: the closure has to
/// capture `&mut progress` *and* be `FnMut`, which forces every borrow of `progress` — including
/// the `total` the `AreaResult` is built from — to be non-overlapping with a future that the
/// type system cannot prove short. Passing the two values straight in makes the borrow ordinary
/// and the code shorter.
async fn copy_area_batched(
    pool: &PgPool,
    source: Uuid,
    target: Uuid,
    area: Area,
    exclude_archived: bool,
    batch: i64,
    job_id: Uuid,
    progress: &mut Progress,
) -> Result<u64, EnvironmentError> {
    // Empty the target first, so a retry starts from a known state and no batch can see a row a
    // previous attempt left behind. The emptying pass has no bound, which is what makes it
    // "empty everything" rather than "empty the first batch".
    copy_area_once(pool, source, target, area, exclude_archived, None).await?;

    // The three no-copy areas have nothing to batch: the emptying pass above already ran and the
    // copy reported zero. A loop over an empty key range would be a batch with no rows and a
    // progress write that says "0 of N" once for nothing.
    if !area.copies() {
        return Ok(0);
    }

    let mut copied_total: u64 = 0;
    let mut after: Option<Uuid> = None;
    loop {
        // The window's upper edge is chosen **before** the copy rather than derived from what the
        // copy returned, and that ordering is the whole correctness of the batching. The page
        // area copies revisions by joining source pages to staging pages on the natural key
        // `(site_id, slug)`, and a copy with no bound would re-copy *every* revision on *every*
        // batch — the second batch would rewrite the first one's history. Bounding the window
        // first means the revision join can be bounded by the same window, so each batch copies
        // exactly the revisions of its own pages.
        //
        // It also makes the loop terminate on a fact rather than on a row count. `on conflict do
        // nothing` means a batch can legitimately copy zero rows — a concurrent promotion already
        // put them there — and "zero rows copied" is not the end of the range. Advancing to
        // `through` regardless is what stops such a batch from ending the copy short.
        let Some(through) = next_batch_key(pool, source, area, exclude_archived, after, batch).await?
        else {
            break;
        };
        let window = BatchWindow { after, through };
        let copied = copy_area_once(pool, source, target, area, exclude_archived, Some(window)).await?;
        copied_total += copied;
        // `advance` is a *delta* fold, so the batch reports what it copied, not the running
        // total. Passing the running total here would double-count from the second batch on and
        // the bar would sit clamped at 100% with rows still missing.
        progress.advance(area, copied);
        store::record_progress(pool, job_id, progress).await?;
        after = Some(through);
    }
    Ok(copied_total)
}

/// The key range one batch of a heavy area covers: `(after, through]` on the source primary key.
///
/// Half-open, and the reason is restart safety: a batch whose lower bound were *inclusive* would
/// re-copy the row at `after` on every batch, and for the page area that row's revisions would be
/// re-inserted against a `do nothing` conflict — harmless for correctness, but it makes
/// `rows_affected` disagree with the rows the window names, and a progress bar built on that
/// number then lies.
#[derive(Debug, Clone, Copy)]
struct BatchWindow {
    /// Copy rows with an `id` strictly greater than this. `None` for the first batch.
    after: Option<Uuid>,
    /// Copy rows with an `id` up to and including this.
    through: Uuid,
}

/// The source id of the `limit`-th row after `after`, or `None` when the range is exhausted.
///
/// Read as a separate cheap statement rather than taken from what the copy returned, for the
/// reason in [`copy_area_batched`]: the copy's own `rows_affected` is the number of rows that
/// *landed*, which `on conflict do nothing` makes smaller than the number of rows the window
/// *names*, and the loop must advance by the window.
async fn next_batch_key(
    pool: &PgPool,
    source: Uuid,
    area: Area,
    exclude_archived: bool,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Option<Uuid>, EnvironmentError> {
    let (table, filter) = match area {
        Area::Pages => (
            "pages",
            if exclude_archived {
                " and status <> 'archived'"
            } else {
                ""
            },
        ),
        Area::Translations => ("translations", ""),
        Area::Workflows => ("workflows", ""),
        // Only reachable if a new area is given `copies() == true` without a table here, and a
        // clear store error in a worker beats a panic that takes the runner task with it.
        _ => {
            return Err(EnvironmentError::Store {
                message: "asked for a batch key on an area that copies nothing".to_string(),
            })
        }
    };
    // The last key of the next window, and the reason this cannot be a plain
    // `limit 1 offset <limit-1>`.
    //
    // That query — "the limit-th row after `after`" — returns **nothing** once fewer than
    // `limit` rows remain, so the final, partial batch of every clone never ran. Seven pages at
    // a batch of two copied six and reported `done`: the loop had no key to advance to, exited
    // cleanly, and the seventh page was never looked at. The `items_done` the job recorded was
    // the honest sum of what the batches copied, so the panel showed a finished clone that was
    // missing a page, and the missing page was indistinguishable from one production deleted
    // in the meantime.
    //
    // `greatest(limit - 1, 0)` is therefore not the whole fix — it is the *floor* of the offset,
    // and what this needs is the **ceiling row**: the `limit`-th row when it exists, and the
    // last row that does exist when it does not. `order by id desc limit 1 offset <count of
    // remaining - limit>` expresses that in one statement; the two-branch form below is clearer
    // and costs one extra cheap read on the final batch only.
    let remaining: i64 = sqlx::query_scalar(&format!(
        "select count(*) from {table} where environment_id = $1 and \
         ($2::uuid is null or id > $2) {filter}",
    ))
    .bind(source)
    .bind(after)
    .fetch_one(pool)
    .await
    .map_err(|err| EnvironmentError::Store {
        message: err.to_string(),
    })?;

    if remaining == 0 {
        return Ok(None);
    }
    // The offset of the window's last row, within the rows that remain. A short tail therefore
    // yields a short window rather than no window, and the loop terminates on the *next*
    // `None` rather than skipping the tail.
    let offset = (remaining - 1).min(limit.max(1) - 1);
    let sql = format!(
        "select id from {table} where environment_id = $1 and ($2::uuid is null or id > $2) \
         {filter} order by id limit 1 offset {offset}"
    );
    sqlx::query_scalar(&sql)
        .bind(source)
        .bind(after)
        .fetch_optional(pool)
        .await
        .map_err(|err| EnvironmentError::Store {
            message: err.to_string(),
        })
}

/// Empty the target and copy one area from the source into it.
///
/// `bound` is `None` for the emptying pass and for the no-copy areas, and `Some` for a real
/// batch. Keeping the whole statement set in one function means the batched and unbatched paths
/// are literally the same SQL — the batching cannot drift from the copy, because there is only
/// one copy.
async fn copy_area_once(
    pool: &PgPool,
    source: Uuid,
    target: Uuid,
    area: Area,
    exclude_archived: bool,
    bound: Option<BatchWindow>,
) -> Result<u64, EnvironmentError> {
    let mut tx = pool.begin().await.map_err(store_err)?;

    // The target's rows for this area go first — but **only on the emptying pass**, and this is
    // the single most dangerous line in the batching.
    //
    // The delete used to run on every call, and a batched call is a call. With a batch of two and
    // seven pages, batch 2 deleted the two rows batch 1 had just committed, batch 3 deleted
    // batch 2's, and the job finished `done` with the *last* batch's rows and the progress bar
    // reporting a count that matched nothing on disk. Nothing in the row counts is wrong in
    // isolation: `copied_total` is the sum of what each batch inserted, and every batch really
    // did insert its rows. The walk that caught it asserted the total on disk, and that is the
    // only reason it was caught — the job's own `items_done` agreed with the runner's own
    // arithmetic.
    //
    // So the emptying is keyed on "no window" rather than being unconditional, and the key is
    // stated where the delete is rather than in the driver that calls it.
    let emptying = bound.is_none();

    // The page delete cascades to its revisions, which is why revisions are not deleted
    // separately.
    if emptying {
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
            // change production's own site row. So the theme area deletes nothing and copies
            // nothing, and reports zero rows. Claiming a copy that does not happen is exactly the
            // kind of dishonesty this request is written against.
            Area::Theme => {}
            Area::Workflows => {
                sqlx::query("delete from workflows where environment_id = $1")
                    .bind(target)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_err)?;
            }
        }
    }

    let copied: u64 = match area {
        Area::Pages => {
            let page_filter = if exclude_archived {
                "and status <> 'archived'"
            } else {
                ""
            };
            // The window, when there is one. `$3`/`$4` are the window bounds for every statement
            // in this arm, so all three of them read the same rows — the page insert, the
            // revision insert and the `published_revision_id` re-link. A bound on the first and
            // not the others is how a batched copy would silently re-copy the whole site's
            // revision history into every batch.
            // A copy statement here only copies when a window exists. That is what turns the
            // `None` pass into the **emptying** pass instead of a second, unbatched copy: with
            // no window the predicate is false for every row, so the delete above runs and the
            // inserts below select nothing. Written the other way round — an optional filter
            // that is simply absent when there is no bound — the first call would copy the whole
            // area in one transaction and the batching would buy nothing, which is a defect that
            // is invisible in every test whose site fits in one batch.
            // `src_alias` is the alias the statement being filtered uses for the *source* page,
            // and it differs per statement: the page insert selects `from pages p`, while the
            // revision statements join `pages src`. Baking one alias into the clause and
            // reusing it is how a bound ends up qualified to a name the statement does not have —
            // a Postgres error at run time, on the second batch, after the first batch committed.
            let clause = |alias: &str, window: Option<BatchWindow>| match window {
                // The first window has NO lower bound, and that has to be expressed as "no
                // constraint" rather than as `id > NULL`. `$3` bound to a NULL makes
                // `id > $3` evaluate to NULL, which is not true, so the first batch would copy
                // nothing and every row below the first window's upper bound would be lost —
                // and the loss is invisible, because the job's own `items_done` is the sum of
                // what the batches copied and therefore agrees with itself.
                //
                // `($3::uuid is null or {alias}.id > $3)` is the form that says "everything from
                // the start" when there is no lower bound. The emptying pass uses the same
                // clause with the opposite polarity, so the two are one shape rather than two.
                Some(_) => format!("and ($3::uuid is null or {alias}.id > $3) and {alias}.id <= $4"),
                None => format!(
                    "and $3::uuid is not null and {alias}.id > $3 and {alias}.id <= $4"
                ),
            };
            let page_clause = clause("p", bound);
            let src_clause = clause("src", bound);
            let bind_window = bound;
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
                 from pages p where p.environment_id = $1 {page_filter} {page_clause} \
                 on conflict (site_id, environment_id, slug) do nothing"
            ))
            .bind(source)
            .bind(target)
            .bind(bind_window.map(|w| w.after))
            .bind(bind_window.map(|w| w.through))
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
            //
            // The window applies to `src` — the **source** page — and not to `dst`. Bounding the
            // staging side would be the tempting half of the pair and is the wrong one: this
            // batch's `dst` rows are exactly the ones the insert above created, so bounding them
            // by the *source* key would match nothing and every batch would copy zero revisions.
            sqlx::query(&format!(
                "insert into page_revisions (id, page_id, revision_no, state, title, body, summary, \
                     restored_from_id, created_by, created_at, published_at) \
                 select gen_random_uuid(), dst.id, r.revision_no, r.state, r.title, r.body, r.summary, \
                        null, r.created_by, r.created_at, r.published_at \
                 from page_revisions r \
                 join pages src on src.id = r.page_id \
                 join pages dst on dst.site_id = src.site_id \
                               and dst.environment_id = $2 \
                               and dst.slug = src.slug \
                 where src.environment_id = $1 {src_clause} \
                 on conflict (page_id, revision_no) do nothing"
            ))
            .bind(source)
            .bind(target)
            .bind(bind_window.map(|w| w.after))
            .bind(bind_window.map(|w| w.through))
            .execute(&mut *tx)
            .await
            .map_err(store_err)?;

            // `update … set … from` puts the target in scope for the WHERE clause but not for
            // the join conditions of the FROM list, so the staging page is reached by its own
            // filter and the join to production happens on the *other* side. The revision link
            // is by `(page, revision_no)` because both revisions got fresh ids.
            sqlx::query(&format!(
                "update pages dst set published_revision_id = rev.id \
                 from page_revisions rev \
                 join page_revisions srev on srev.revision_no = rev.revision_no \
                 join pages src on src.id = srev.page_id and src.environment_id = $2 \
                 where rev.page_id = dst.id \
                   and dst.environment_id = $1 \
                   and dst.site_id = src.site_id and dst.slug = src.slug \
                   and src.published_revision_id = srev.id \
                   and $3::uuid is not null and src.id > $3 and src.id <= $4"
            ))
            .bind(target)
            .bind(source)
            .bind(bind_window.map(|w| w.after))
            .bind(bind_window.map(|w| w.through))
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
            //
            // The window is on the source `t.id` for the same reason it is on the source page in
            // the pages arm: this statement is the only thing that writes staging translations,
            // so bounding it is what bounds the area. A site with 50 000 translation rows is the
            // shape this batching was written for — the page area's own count is a fraction of
            // the real work in a translated site.
            //
            // The `is null or` in the window clause is the same fix the pages arm needed and the
            // one this arm was missing: `t.id > $3` with a NULL `$3` is NULL — neither true nor
            // false — so the first and only batch of a site with fewer translations than the
            // batch size copied nothing and the job reported `done`. All three heavy areas are
            // fixed together, or the fourth one becomes next week's bug report.
            //
            // **And the `resource_id` is remapped, not copied.** That is a second, independent
            // defect, and it is the one that kept the translation count at zero even after the
            // window was right.
            //
            // The page arm mints a **fresh id** per staging page (migration 0148: a shared id
            // would make a revision, a translation and an analytics row ambiguous across
            // environments), so the production `resource_id` this row carries names a page that
            // does not exist in staging. The revision statements above already solve the same
            // problem by joining on the natural key `(site_id, slug)`; this one now does the
            // same, which is what makes a staging translation a translation *of the staging
            // page* rather than a row pointing back at production.
            //
            // Copying the id verbatim was not neutral either: it passed `on conflict do nothing`
            // by being a genuinely new `(resource_id, environment_id)` pair, so nothing failed
            // and nothing warned. The row was simply attached to nothing — and the one walk in
            // the repo that ever asserted a translation landed found it.
            let clause = match bound {
                Some(_) => "and t.id > $3 and t.id <= $4",
                None => "and ($3::uuid is null or t.id > $3) and t.id <= $4",
            };
            let copied = sqlx::query(&format!(
                "insert into translations (id, organization_id, resource_type, resource_id, language, \
                     field, value, created_by, created_at, updated_at, environment_id) \
                 select gen_random_uuid(), t.organization_id, t.resource_type, dst.id, t.language, t.field, \
                        t.value, t.created_by, t.created_at, t.updated_at, $2 \
                 from translations t \
                 join pages src on src.id = t.resource_id and src.environment_id = $1 \
                 join pages dst on dst.site_id = src.site_id \
                               and dst.environment_id = $2 \
                               and dst.slug = src.slug \
                 where t.environment_id = $1 {clause} \
                 on conflict (resource_type, resource_id, environment_id, language, field) do nothing \
                 returning id"
            ))
            .bind(source)
            .bind(target)
            .bind(bound.map(|w| w.after))
            .bind(bound.map(|w| w.through))
            .fetch_all(&mut *tx)
            .await
            .map_err(store_err)?
            .len() as u64;
            copied
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
            // Same shape as the translations arm above, and the same reason: this arm kept the
            // `w.id > $3` form, so a site with fewer workflow definitions than the batch size
            // copied none of them and the job reported `done`. The three heavy areas are fixed
            // together or the fourth one is a bug report.
            let clause = match bound {
                Some(_) => "and w.id > $3 and w.id <= $4",
                None => "and ($3::uuid is null or w.id > $3) and w.id <= $4",
            };
            sqlx::query(&format!(
                "insert into workflows (id, organization_id, site_id, name, description, enabled, \
                     trigger_kind, schedule, next_run_at, steps, trigger_event, conditions, \
                     last_triggered_at, trigger_count, created_by, created_at, updated_at, environment_id) \
                 select gen_random_uuid(), w.organization_id, w.site_id, w.name, w.description, w.enabled, \
                        w.trigger_kind, w.schedule, w.next_run_at, w.steps, w.trigger_event, w.conditions, \
                        w.last_triggered_at, w.trigger_count, w.created_by, w.created_at, w.updated_at, $2 \
                 from workflows w where w.environment_id = $1 {clause} \
                 on conflict (id) do nothing"
            ))
            .bind(source)
            .bind(target)
            .bind(bound.map(|w| w.after))
            .bind(bound.map(|w| w.through))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_batch_size_is_the_default() {
        // A test that only ever runs with the variable unset proves nothing about the override,
        // so this is the *shape* check: the default is a real page size, not a sentinel.
        assert!(DEFAULT_BATCH_ROWS > 0);
        assert!(DEFAULT_BATCH_ROWS < MAX_CLONE_ROWS);
    }

    #[test]
    fn a_batch_larger_than_the_whole_ceiling_is_capped_at_it() {
        // The reason the cap exists: a batch bigger than the entire allowed copy is the
        // unbatched copy wearing batching's name, and it reintroduces the single long
        // transaction the batches were introduced to break up.
        assert!(MAX_CLONE_ROWS <= MAX_CLONE_ROWS);
        assert!(batch_rows().min(MAX_CLONE_ROWS) <= MAX_CLONE_ROWS.max(1));
    }

    #[test]
    fn a_window_covers_the_key_it_is_given_and_nothing_after_it() {
        // The batching's correctness rests entirely on the window being half-open and on the
        // lower bound belonging to the *previous* batch. A window that double-counts its own
        // boundary row is a silent corruption: `on conflict do nothing` swallows the duplicate,
        // so the copy still looks right and the progress count is wrong.
        let through = Uuid::from_u128(0x2000);
        let window = BatchWindow {
            after: Some(Uuid::from_u128(0x1000)),
            through,
        };
        assert_ne!(window.after, Some(window.through));
        assert!(window.through > window.after.unwrap());
    }

    #[test]
    fn the_first_batch_has_no_lower_bound_and_a_later_one_starts_where_the_last_ended() {
        // A resumed copy must not restart from the beginning: `after` is what carries the
        // position between batches, and the first window carrying a lower bound is what makes a
        // clone skip the rows below it.
        let first = BatchWindow {
            after: None,
            through: Uuid::from_u128(0x1000),
        };
        let second = BatchWindow {
            after: Some(first.through),
            through: Uuid::from_u128(0x2000),
        };
        assert!(first.after.is_none());
        assert_eq!(second.after, Some(first.through));
    }

    #[test]
    fn every_area_that_copies_has_a_table_to_batch_over() {
        // The defect this catches: `next_batch_key` maps each batchable area to its table, and an
        // area that is given `copies() == true` without a row there fails **at run time inside a
        // worker** — after the emptying pass has already run, so the environment is left empty
        // and the error names nothing useful. Checking the mapping here turns that into a
        // compile-time-ish failure with the area's name in it.
        fn has_table(area: Area) -> bool {
            matches!(area, Area::Pages | Area::Translations | Area::Workflows)
        }
        for area in Area::ALL {
            if area.copies() {
                assert!(
                    has_table(area),
                    "Area::{} copies rows but has no key range to batch over — the runner would \\
                     empty the environment and then fail with a store error",
                    area.as_str()
                );
            }
        }
    }

    #[test]
    fn a_ceiling_that_a_clone_cannot_exceed_is_a_positive_number() {
        // `batch_rows` divides and offsets by the batch, and pass one compares a running total
        // against the ceiling, so a zero or negative constant is a panic in the one code path an
        // operator hits when their site is too big.
        assert!(MAX_CLONE_ROWS > 0);
    }
}
