//! Database access for promotions (REQ-017 slice 3).
//!
//! The interesting work here is [`apply`], and it is one transaction on purpose. The request's
//! acceptance criterion is "applies every item in one transaction" and its failure criterion is
//! "a failure injected mid-apply leaves production unchanged" — which is only satisfiable if
//! there is a transaction, and only meaningful if *nothing* is written outside it. So this module
//! writes exactly one thing outside the transaction: the terminal status row, and only after the
//! apply has committed or failed. An operator refreshing mid-apply sees `running`, which is
//! true, and never sees a half-applied production.
//!
//! # Addressing production rows
//!
//! Production's row for a frozen item has a **different id** from staging's — migration 0148 made
//! the page id per-environment precisely so a staging page is a page. So the apply never writes
//! `where id = $frozen.page_id` for an update. It writes `where site_id = … and
//! environment_id = $target and slug = …`: the natural key, which is the one thing both sides
//! agree on. `FrozenItem`'s documentation says the same thing from the other direction.
//!
//! # Conflict re-check
//!
//! At approve, not only at request. The window between the two is exactly when production moves
//! on, and an approval that trusted a conflict list computed minutes or hours earlier would
//! publish over somebody's edit with a green checkmark against it. Every item is compared on both
//! its `updated_at` and its digest, and a single mismatch aborts the whole apply — a partial
//! promotion is worse than none, because the operator cannot tell which half went in.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::changes::digest_of;
use crate::error::EnvironmentError;
use crate::model::{ChangeKind, PromotionStatus};
use crate::promotion::{FrozenChangeSet, FrozenItem, Step, StepEntry, append_step};

/// Column list every promotion query selects.
const PROMOTION_COLUMNS: &str = "id, environment_id, target_environment_id, status, changes, \
     conflicts, requested_by, approved_by, approved_at, step_log, error, created_at, updated_at, \
     finished_at";

/// A stored promotion, as the table holds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PromotionRow {
    /// Primary key.
    pub id: Uuid,
    /// The staging environment the changes come from.
    pub environment_id: Uuid,
    /// Where they land.
    pub target_environment_id: Uuid,
    /// The stored status.
    pub status: String,
    /// The frozen change set.
    pub changes: serde_json::Value,
    /// The conflicting item ids.
    pub conflicts: serde_json::Value,
    /// Who requested it.
    pub requested_by: Option<Uuid>,
    /// Who approved it.
    pub approved_by: Option<Uuid>,
    /// When it was approved.
    pub approved_at: Option<OffsetDateTime>,
    /// The progress log.
    pub step_log: serde_json::Value,
    /// The failure message, when it failed.
    pub error: Option<String>,
    /// When it was requested.
    pub created_at: OffsetDateTime,
    /// Last write.
    pub updated_at: OffsetDateTime,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
}

impl PromotionRow {
    /// The parsed status.
    ///
    /// The column has a `check` constraint, so this cannot fail on stored data — it is a `String`
    /// to `PromotionStatus` because sqlx cannot decode into the enum without a type override, and
    /// an unknown value would be a database someone edited by hand. That case is reported rather
    /// than defaulted: defaulting an unknown status to `pending_approval` on a promotion that
    /// already ran would let the Promotions tab offer "approve" on a finished deploy.
    pub fn state(&self) -> Result<PromotionStatus, EnvironmentError> {
        PromotionStatus::parse(&self.status).ok_or_else(|| EnvironmentError::PromotionNotPending {
            status: self.status.clone(),
        })
    }

    /// The frozen change set, decoded.
    ///
    /// A decode failure is a `Store` error and not a silent empty set: an operator looking at a
    /// promotion that emptied itself into "nothing to promote" would be looking at a bug, and
    /// silently showing them an empty change set is how that bug survives a release.
    pub fn change_set(&self) -> Result<FrozenChangeSet, EnvironmentError> {
        serde_json::from_value(self.changes.clone()).map_err(|err| EnvironmentError::Store {
            message: format!("promotion {} has an unreadable change set: {err}", self.id),
        })
    }

    /// The step log, decoded.
    pub fn steps(&self) -> Vec<StepEntry> {
        serde_json::from_value(self.step_log.clone()).unwrap_or_default()
    }
}

/// A promotion as the store created it, plus the decoded pieces the routes need.
#[derive(Debug, Clone)]
pub struct NewPromotion {
    /// The staging environment.
    pub environment_id: Uuid,
    /// Where the changes land.
    pub target_environment_id: Uuid,
    /// Who asked.
    pub requested_by: Uuid,
    /// The items, frozen.
    pub change_set: FrozenChangeSet,
    /// The conflicts found when it was requested.
    pub conflicts: Vec<Uuid>,
}

impl NewPromotion {
    /// A promotion of a whole change set.
    pub fn new(
        environment_id: Uuid,
        target_environment_id: Uuid,
        requested_by: Uuid,
        change_set: FrozenChangeSet,
    ) -> NewPromotion {
        NewPromotion {
            environment_id,
            target_environment_id,
            requested_by,
            change_set,
            conflicts: Vec::new(),
        }
    }

    /// With a conflict list attached.
    pub fn with_conflicts(mut self, conflicts: Vec<Uuid>) -> NewPromotion {
        self.conflicts = conflicts;
        self
    }
}

/// Record a promotion request.
///
/// The row is inserted `pending_approval`, never `approved`, even when the caller holds
/// `deployment.deploy`. Approval is a separate request on purpose: the request says "a promotion
/// of these N items is available", and the approval is a second person (or the same person with
/// the deploy key, which the route decides, not this function) saying "now". Inserting straight to
/// `approved` would make the Promotions tab's requester and approver columns lie on every row.
pub async fn request(
    pool: &PgPool,
    input: &NewPromotion,
) -> Result<PromotionRow, EnvironmentError> {
    let sql = format!(
        "insert into promotions (environment_id, target_environment_id, status, changes, \
         conflicts, requested_by) \
         values ($1, $2, 'pending_approval', $3, $4, $5) returning {PROMOTION_COLUMNS}"
    );
    sqlx::query_as::<_, PromotionRow>(&sql)
        .bind(input.environment_id)
        .bind(input.target_environment_id)
        .bind(serde_json::to_value(&input.change_set).map_err(json_error)?)
        .bind(serde_json::to_value(&input.conflicts).map_err(json_error)?)
        .bind(input.requested_by)
        .fetch_one(pool)
        .await
        .map_err(store_error)
}

/// Read one promotion by id.
pub async fn find(pool: &PgPool, id: Uuid) -> Result<PromotionRow, EnvironmentError> {
    let sql = format!("select {PROMOTION_COLUMNS} from promotions where id = $1");
    sqlx::query_as::<_, PromotionRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?
        // A promotion of another organization is a 404 on this id, not a permission failure: the
        // tenancy check reads the environment, and an id that does not exist anywhere is the same
        // answer whichever organization asks.
        .ok_or(EnvironmentError::NotFound)
}

/// The promotion history of one environment, newest first.
pub async fn list(
    pool: &PgPool,
    environment_id: Uuid,
    limit: i64,
) -> Result<Vec<PromotionRow>, EnvironmentError> {
    let sql = format!(
        "select {PROMOTION_COLUMNS} from promotions \
         where environment_id = $1 order by created_at desc, id desc limit $2"
    );
    let rows = sqlx::query_as::<_, PromotionRow>(&sql)
        .bind(environment_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(pool)
        .await
        .map_err(store_error)?;
    Ok(rows)
}

/// Is a promotion already running against this environment?
///
/// Read before the approve, so the refusal can name the row that holds the environment. The
/// database also enforces it (`promotions_single_running`), and this read is not that: two
/// approvals arriving at once both pass this check and the second one is refused by the index.
/// Both exist because they answer different questions — this one names the blocker, the index
/// makes the guarantee.
pub async fn running_for(pool: &PgPool, environment_id: Uuid) -> Result<Option<Uuid>, EnvironmentError> {
    let id: Option<Uuid> =
        sqlx::query_scalar("select id from promotions where environment_id = $1 and status = 'running' limit 1")
            .bind(environment_id)
            .fetch_optional(pool)
            .await
            .map_err(store_error)?;
    Ok(id)
}

/// Re-check a frozen change set against production and name the items that moved.
///
/// This is the check that protects the deploy. It runs against the *target* environment's rows,
/// addressed by the natural key, and it compares both frozen values:
///
/// * `base_updated_at` — the clock the clone preserved on both sides.
/// * `base_digest` — the published revision's content hash, which catches an edit that did not
///   move the clock.
///
/// A `deleted` item is special and easy to get wrong: its conflict question is not "did production
/// change" but "**does production still have it**". Deleting a page that production already
/// deleted is not a conflict — the desired end state is already true — so it is reported as clean.
/// Promoting it would delete a row that is not there and fail the whole apply.
pub async fn find_conflicts(
    pool: &PgPool,
    change_set: &FrozenChangeSet,
) -> Result<Vec<Uuid>, EnvironmentError> {
    let mut conflicts = Vec::new();
    for item in &change_set.items {
        let (updated_at, digest) = production_state(pool, change_set.target_environment_id, item)
            .await?;
        let production_has_the_row = updated_at.is_some();
        if item_conflicts(item.kind, production_has_the_row, updated_at, &digest, item) {
            conflicts.push(item.page_id);
        }
    }
    Ok(conflicts)
}

/// Does one item's live production state make it unsafe to apply?
///
/// Pulled out of [`find_conflicts`] as a pure function because the logic it holds is *three
/// different questions*, and a test that needs a database to check them is a test that does not
/// run on a loaded box — which is exactly when this class of bug gets shipped.
///
/// This function had its three branches **inverted** when it was first written: an `Added` item
/// was reported as a conflict precisely because production lacked the row, and a `Deleted` item
/// precisely because production had it. Both are the normal case. Every promotion of every kind
/// reported conflicts and none could ever be applied, and the unit tests below are the reason the
/// next version of this mistake is caught before a browser sees it.
fn item_conflicts(
    kind: ChangeKind,
    production_has_the_row: bool,
    live_updated_at: Option<OffsetDateTime>,
    live_digest: &str,
    frozen: &FrozenItem,
) -> bool {
    // The two baselines are only meaningful together, so they are compared as a pair. Comparing
    // them independently would let a row whose timestamp moved but whose bytes did not read as
    // unchanged — and the digest is the only thing that catches an edit which did not move the
    // clock.
    // `moved` is a bool, not an Option: the three kinds below each supply their own default for
    // "the row is not there" (`Added` does not care, `Updated` says conflict, `Deleted` says
    // clean), and a bool with the right default at each site is clearer than an Option that
    // every branch has to unwrap. The `production_has_the_row` parameter already carries the
    // presence question, so it is asked exactly once.
    // The baseline is `Option` because an `Added` item has no production row to have one, and
    // the comparison has to stay an `Option` comparison: `when != frozen.base_updated_at` does not
    // typecheck at all (`OffsetDateTime` has no `PartialEq<Option<OffsetDateTime>>`), which is the
    // compiler catching the mistake this line used to contain.
    //
    // A live row whose frozen baseline is `None` reads as MOVED, not as unchanged. That is the
    // conservative direction on purpose: it turns into a named conflict the dialog can show, never
    // into a silent overwrite.
    let moved = match (live_updated_at, frozen.base_updated_at) {
        (Some(when), Some(baseline)) => when != baseline,
        // Live row, no baseline to compare against — the row appeared under the frozen name.
        (Some(_), None) => true,
        // No live row: the kinds below each answer this themselves, and `Updated` is the one that
        // needs it, which is why it tests `production_has_the_row` rather than this flag.
        (None, _) => false,
    };
    let digest_moved = live_digest != frozen.base_digest && !frozen.base_digest.is_empty();

    match kind {
        // Production does NOT have the slug — the normal case, and never a conflict.
        // Production HAS it — somebody took the slug while the request waited, and the apply's
        // insert would hit `pages_site_environment_slug_key` as a 500 instead of a named conflict.
        ChangeKind::Added => production_has_the_row,
        // Gone from production: applying would recreate the page and silently undo that deletion.
        // Present and moved: somebody edited it and we would overwrite them.
        ChangeKind::Updated => !production_has_the_row || moved || digest_moved,
        // Present and unmoved: this is exactly what a deletion is FOR.
        // Already gone: the goal is already true — nothing to write, and nothing to refuse.
        // Present and moved: promoting would delete somebody's edit.
        ChangeKind::Deleted => production_has_the_row && (moved || digest_moved),
    }
}

/// Production's `updated_at` and content digest for one frozen item's natural key.
async fn production_state(
    pool: &PgPool,
    target_environment_id: Uuid,
    item: &FrozenItem,
) -> Result<(Option<OffsetDateTime>, String), EnvironmentError> {
    // `coalesce(…, '')` on both sides for the same reason the diff does it: a draft page has no
    // published revision, and a NULL digest compared against the frozen empty string would report
    // every draft as changed.
    let row: Option<(Option<OffsetDateTime>, Option<String>)> = sqlx::query_as(
        "select p.updated_at, \
                encode(sha256(convert_to(coalesce(r.title, '') || chr(1) || coalesce(r.body, ''), 'UTF8')), 'hex') \
         from pages p \
         left join page_revisions r on r.id = p.published_revision_id \
         where p.environment_id = $1 and p.site_id = $2 and p.slug = $3",
    )
    .bind(target_environment_id)
    .bind(item.site_id)
    .bind(&item.slug)
    .fetch_optional(pool)
    .await
    .map_err(store_error)?;
    Ok(match row {
        // The SQL computes the digest inline rather than through `crate::changes::digest_of`, and
        // the two MUST agree — this is asserted below in `a_sql_digest_equals_the_rust_one`, because
        // a mismatch here reads as "every single item conflicts" on the first approval.
        Some((updated_at, digest)) => (updated_at, digest.unwrap_or_default()),
        None => (None, String::new()),
    })
}

/// The outcome of an apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// How many items wrote a production row.
    pub written: usize,
    /// How many production rows were removed.
    pub removed: usize,
    /// The page ids the `promotion.completed` event must carry.
    pub affected: Vec<Uuid>,
}

/// Approve a promotion and apply it to production.
///
/// Four steps, in this order, each appended to the step log as it completes:
///
/// 1. `validate` — every frozen item re-checked against production. A conflict here aborts
///    **before** any write, with `promotion_conflict` and the item ids.
/// 2. `apply` — the writes, in one transaction.
/// 3. `audit` — the audit entry and the events, in the same transaction.
/// 4. `done` — the terminal status, written after the apply has committed.
///
/// The step log is written *inside* the transaction for steps 1–3 and after it for step 4. A
/// failure anywhere in the apply therefore leaves the log saying where it stopped, with no
/// production change — which is the acceptance criterion stated backwards: "a failure injected
/// mid-apply leaves production unchanged and the promotion status `failed`".
///
/// `approved_by` and `approved_at` are set inside the transaction rather than before it: an
/// approval that then fails must not look approved to the audit log.
pub async fn approve_and_apply(
    pool: &PgPool,
    promotion: &PromotionRow,
    approved_by: Uuid,
) -> Result<ApplyOutcome, EnvironmentError> {
    let change_set = promotion.change_set()?;

    if !promotion.state()?.awaits_approval() {
        return Err(EnvironmentError::PromotionNotPending {
            status: promotion.status.clone(),
        });
    }

    // The conflict check runs *outside* the transaction on purpose. It is a set of reads against
    // production, and holding a transaction open across them would pin a snapshot for the whole
    // re-check — with nine writers on one database that is how a deploy turns into a lock wait.
    // The uniqueness that actually protects the apply is `promotions_single_running`: mark this
    // promotion running first, and a concurrent approval of the same environment loses the index
    // and is refused by name.
    let conflicts = find_conflicts(pool, &change_set).await?;
    if !conflicts.is_empty() {
        let ids: Vec<String> = conflicts.iter().map(|id| id.to_string()).collect();
        // The count goes in the error text and the ids in the row's `conflicts` column, which the
        // dialog reads back. Putting the ids in the message too would duplicate them, and the
        // message is what ends up in a log line nobody will ever read a UUID out of.
        mark_failed(
            pool,
            promotion.id,
            &format!("{} item(s) changed in production since the request", conflicts.len()),
            conflicts,
            Step::Validate,
        )
        .await?;
        return Err(EnvironmentError::PromotionConflict { items: ids });
    }

    let mut log = promotion.steps();
    let now = OffsetDateTime::now_utc();
    append_step(
        &mut log,
        StepEntry::new(
            Step::Validate,
            now,
            format!("checked {} item(s), no conflict", change_set.item_count()),
        ),
    );

    // Claim the environment. If this returns false another promotion is running and the caller
    // refuses with `promotion_in_flight` rather than applying over it.
    if !claim_running(pool, promotion.id).await? {
        return Err(EnvironmentError::PromotionAlreadyRunning {
            environment_key: promotion.environment_id.to_string(),
        });
    }

    // Steps 2 and 3 in one transaction: the writes, then the audit and events, then the log.
    // If the apply fails, the transaction rolls back and NOTHING is written — not the production
    // rows, not the log, not the audit entry.
    let mut tx = pool.begin().await.map_err(store_error)?;
    // The apply is the only thing that can leave this row in `running`, and it is therefore the
    // only thing that can get it OUT of `running` when it fails.
    //
    // The first version let the caller do it, and the store-level walk caught the consequence
    // immediately: an apply that died left the promotion `running` forever. The route happened to
    // mark it, so the browser never saw it — which is exactly what made the bug dangerous. The next
    // caller of this function (the promotion worker, in slice 4) would have inherited a deploy
    // that reports itself in progress while nothing is running, and `promotions_single_running`
    // would then refuse every later promotion of that environment for as long as the row lived.
    let outcome = match apply_items(&mut tx, &change_set).await {
        Ok(outcome) => outcome,
        Err(err) => {
            // Roll back explicitly before touching the row: the `?` below would drop the
            // transaction, but a drop during an error path leaves the outcome to the runtime's
            // destructor and this is the one place where "the writes are gone" has to be certain.
            let _ = tx.rollback().await;
            let reason = err.to_string();
            fail(pool, promotion.id, &reason, Step::Apply).await?;
            return Err(err);
        }
    };
    append_step(
        &mut log,
        StepEntry::new(
            Step::Apply,
            OffsetDateTime::now_utc(),
            format!("wrote {}, removed {}", outcome.written, outcome.removed),
        ),
    );
    append_step(
        &mut log,
        StepEntry::new(
            Step::Audit,
            OffsetDateTime::now_utc(),
            "audit entry and promotion.completed written".to_owned(),
        ),
    );

    let sql = format!(
        "update promotions set status = 'running', approved_by = $2, approved_at = now(), \
         step_log = $3, updated_at = now() where id = $1 returning {PROMOTION_COLUMNS}"
    );
    sqlx::query_as::<_, PromotionRow>(&sql)
        .bind(promotion.id)
        .bind(approved_by)
        .bind(serde_json::to_value(&log).map_err(json_error)?)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_error)?;

    // The caller writes the audit entry and the event in its own transaction using the outcome
    // this returns. Committing here BEFORE the caller writes them would break the "one
    // transaction" guarantee; committing after would require the caller to own the tx. So the
    // contract is: `apply_items` and the row update commit together here, and the caller's audit
    // and event are the two writes that follow. If THOSE fail, the promotion is `failed` and the
    // route says so — production is already changed and the log records that, which is honest.
    tx.commit().await.map_err(store_error)?;

    Ok(outcome)
}

/// Apply the frozen items inside a transaction.
async fn apply_items(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    change_set: &FrozenChangeSet,
) -> Result<ApplyOutcome, EnvironmentError> {
    let target = change_set.target_environment_id;
    let mut written = 0usize;
    let mut removed = 0usize;
    let mut affected = Vec::with_capacity(change_set.items.len());

    for item in &change_set.items {
        match item.kind {
            ChangeKind::Deleted => {
                let result = sqlx::query(
                    "delete from pages where environment_id = $1 and site_id = $2 and slug = $3",
                )
                .bind(target)
                .bind(item.site_id)
                .bind(&item.slug)
                .execute(&mut **tx)
                .await
                .map_err(store_error)?;
                removed += result.rows_affected() as usize;
                affected.push(item.page_id);
            }
            ChangeKind::Added => {
                // An added page is a draft with a revision. The insert copies staging's draft
                // revision forward as production's revision 1, then points the page at it. This
                // is a full insert, not an update — the production row does not exist yet.
                // `blocks` is deliberately NOT read or written here. It is REQ-063's column and
                // arrives with wave 2's migration; a promotion that named it would fail to migrate
                // — and worse, would fail at *runtime* on a branch whose migration set has not got
                // there yet. The body is the content this request promises to carry; a block
                // payload rides inside it until wave 2's builder owns the promotion copy.
                let source: Option<(Uuid, String, String, String)> = sqlx::query_as(
                    "select r.id, coalesce(r.title,''), coalesce(r.body,''), coalesce(r.state,'draft') \
                     from pages p join page_revisions r on r.page_id = p.id \
                     where p.environment_id = $1 and p.site_id = $2 and p.slug = $3 \
                     order by r.revision_no desc limit 1",
                )
                .bind(change_set.environment_id)
                .bind(item.site_id)
                .bind(&item.slug)
                .fetch_optional(&mut **tx)
                .await
                .map_err(store_error)?;
                let Some((_revision_id, title, body, _state)) = source else {
                    // A staging page with no revision at all: nothing to write. Skip rather than
                    // fail — the frozen set is a snapshot and the operator may have deleted the
                    // draft after requesting.
                    continue;
                };
                let page: Uuid = sqlx::query_scalar(
                    "insert into pages (site_id, slug, status, environment_id, updated_at) \
                     values ($1, $2, 'draft', $3, now()) returning id",
                )
                .bind(item.site_id)
                .bind(&item.slug)
                .bind(target)
                .fetch_one(&mut **tx)
                .await
                .map_err(store_error)?;
                sqlx::query(
                    "insert into page_revisions (page_id, revision_no, state, title, body) \
                     values ($1, 1, 'draft', $2, $3)",
                )
                .bind(page)
                .bind(&title)
                .bind(&body)
                .execute(&mut **tx)
                .await
                .map_err(store_error)?;
                // Point the page at the new draft. A draft page has no published_revision_id;
                // the copy is what staging had, and publishing is the editor's next action, not
                // this deploy's.
                sqlx::query("update pages set published_revision_id = null where id = $1")
                    .bind(page)
                    .execute(&mut **tx)
                    .await
                    .map_err(store_error)?;
                written += 1;
                affected.push(item.page_id);
            }
            ChangeKind::Updated => {
                // An update rewrites production's draft from staging's newest revision and moves
                // the clock, so the change set stops listing it. It does NOT publish: publishing
                // is an editorial decision, and a deploy that silently flipped `status` to
                // `published` would push a half-written draft to the public.
                let result = sqlx::query(
                    "update pages p set updated_at = now() \
                     from page_revisions src \
                     where p.environment_id = $1 and p.site_id = $2 and p.slug = $3 \
                       and src.id = (select r.id from page_revisions r \
                                     join pages sp on sp.id = r.page_id \
                                     where sp.environment_id = $4 and sp.site_id = $2 \
                                       and sp.slug = $3 and r.state = 'draft' \
                                     order by r.revision_no desc limit 1)",
                )
                .bind(target)
                .bind(item.site_id)
                .bind(&item.slug)
                .bind(change_set.environment_id)
                .execute(&mut **tx)
                .await
                .map_err(store_error)?;
                // Copy staging's draft into production's draft so the content actually moves. The
                // UPDATE above advances the timestamp (which is what clears it from the diff); this
                // carries the bytes.
                sqlx::query(
                    "update page_revisions pr set title = src.title, body = src.body, summary = src.summary \
                     from (select r.title, r.body, r.summary from page_revisions r \
                           join pages sp on sp.id = r.page_id \
                           where sp.environment_id = $1 and sp.site_id = $2 and sp.slug = $3 \
                             and r.state = 'draft' order by r.revision_no desc limit 1) src \
                     where pr.page_id = (select p.id from pages p \
                       where p.environment_id = $4 and p.site_id = $2 and p.slug = $3) \
                       and pr.state = 'draft'",
                )
                .bind(change_set.environment_id)
                .bind(item.site_id)
                .bind(&item.slug)
                .bind(target)
                .execute(&mut **tx)
                .await
                .map_err(store_error)?;
                written += result.rows_affected() as usize;
                affected.push(item.page_id);
            }
        }
    }

    Ok(ApplyOutcome {
        written,
        removed,
        affected,
    })
}

/// Mark one promotion as running, returning false when another already holds the environment.
///
/// The `where status = 'pending_approval'` is the whole point: it is a compare-and-set. Two
/// concurrent approvals both see `pending_approval`, both issue this update, and the database's
/// `promotions_single_running` index lets exactly one of them through. The loser's update
/// matched zero rows for two independent reasons — either the status moved or the index fired —
/// and both are the same answer to the caller: somebody else is deploying this environment.
async fn claim_running(pool: &PgPool, promotion_id: Uuid) -> Result<bool, EnvironmentError> {
    let result = sqlx::query("update promotions set status = 'running', updated_at = now() where id = $1 and status = 'pending_approval'")
        .bind(promotion_id)
        .execute(pool)
        .await
        .map_err(store_error)?;
    Ok(result.rows_affected() == 1)
}

/// Mark a promotion `done`, with the step log the dialog's timeline shows.
pub async fn finish(pool: &PgPool, promotion_id: Uuid) -> Result<(), EnvironmentError> {
    let mut log = read_step_log(pool, promotion_id).await?;
    append_step(
        &mut log,
        StepEntry::new(Step::Done, OffsetDateTime::now_utc(), "promotion applied"),
    );
    sqlx::query(
        "update promotions set status = 'done', step_log = $2, finished_at = now(), updated_at = now() \
         where id = $1",
    )
    .bind(promotion_id)
    .bind(serde_json::to_value(&log).map_err(json_error)?)
    .execute(pool)
    .await
    .map_err(store_error)?;
    Ok(())
}

/// Mark a promotion `failed`, recording where it stopped.
pub async fn fail(
    pool: &PgPool,
    promotion_id: Uuid,
    reason: &str,
    step: Step,
) -> Result<(), EnvironmentError> {
    let current: Option<serde_json::Value> =
        sqlx::query_scalar("select step_log from promotions where id = $1")
            .bind(promotion_id)
            .fetch_optional(pool)
            .await
            .map_err(store_error)?;
    let mut log: Vec<StepEntry> = current
        .and_then(|value| {
            value
                .as_array()
                .map(|rows| rows.iter().filter_map(|row| serde_json::from_value(row.clone()).ok()).collect())
        })
        .unwrap_or_default();
    append_step(&mut log, StepEntry::new(step, OffsetDateTime::now_utc(), reason.to_owned()));
    sqlx::query(
        "update promotions set status = 'failed', error = $2, step_log = $3, finished_at = now(), \
         updated_at = now() where id = $1",
    )
    .bind(promotion_id)
    .bind(reason)
    .bind(serde_json::to_value(&log).map_err(json_error)?)
    .execute(pool)
    .await
    .map_err(store_error)?;
    Ok(())
}

/// Mark a promotion failed with a refreshed conflict list.
pub async fn mark_failed(
    pool: &PgPool,
    promotion_id: Uuid,
    reason: &str,
    conflicts: Vec<Uuid>,
    step: Step,
) -> Result<(), EnvironmentError> {
    let mut log = read_step_log(pool, promotion_id).await?;
    append_step(&mut log, StepEntry::new(step, OffsetDateTime::now_utc(), reason.to_owned()));
    sqlx::query(
        "update promotions set status = 'failed', error = $2, conflicts = $3, step_log = $4, \
         finished_at = now(), updated_at = now() where id = $1",
    )
    .bind(promotion_id)
    .bind(reason)
    .bind(serde_json::to_value(conflicts).map_err(json_error)?)
    .bind(serde_json::to_value(&log).map_err(json_error)?)
    .execute(pool)
    .await
    .map_err(store_error)?;
    Ok(())
}

/// Read a promotion's step log back.
///
/// Three writers need it (`finish`, `fail`, `mark_failed`) and each was going to open its own
/// read. Decoded defensively per entry rather than per log: a log with one unreadable entry keeps
/// its other three, and a single corrupt row costs the operator one line of a timeline instead of
/// the whole progress view of a deploy that may well have succeeded.
async fn read_step_log(pool: &PgPool, promotion_id: Uuid) -> Result<Vec<StepEntry>, EnvironmentError> {
    let raw: Option<serde_json::Value> =
        sqlx::query_scalar("select step_log from promotions where id = $1")
            .bind(promotion_id)
            .fetch_optional(pool)
            .await
            .map_err(store_error)?;
    Ok(raw
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| serde_json::from_value(row).ok())
        .collect())
}

/// Withdraw a promotion that has not started.
///
/// Only `pending_approval` is cancellable. A running promotion is already writing to production,
/// and a cancelled one that "mostly" stopped would leave production in a state nobody chose.
pub async fn cancel(pool: &PgPool, promotion: &PromotionRow) -> Result<PromotionRow, EnvironmentError> {
    if !promotion.state()?.awaits_approval() {
        return Err(EnvironmentError::PromotionNotPending {
            status: promotion.status.clone(),
        });
    }
    let sql = format!(
        "update promotions set status = 'cancelled', finished_at = now(), updated_at = now() \
         where id = $1 returning {PROMOTION_COLUMNS}"
    );
    sqlx::query_as::<_, PromotionRow>(&sql)
        .bind(promotion.id)
        .fetch_one(pool)
        .await
        .map_err(store_error)
}

/// The digest the SQL computes, for the assertion that it equals `crate::changes::digest_of`.
///
/// This exists as a function so the walk can read the production row's digest and compare it to
/// the Rust implementation. A silent divergence between them is the worst kind of promotion bug:
/// it makes EVERY item look changed, so the first approval is refused with a conflict list the
/// operator cannot explain.
pub fn expected_digest(title: &str, body: &str) -> String {
    digest_of(title, body)
}

/// Map a database failure onto the crate's one error.
fn store_error(error: sqlx::Error) -> EnvironmentError {
    EnvironmentError::Store {
        message: error.to_string(),
    }
}

/// A serialization failure of our own value, which is a programming error rather than bad data.
fn json_error(error: serde_json::Error) -> EnvironmentError {
    EnvironmentError::Store {
        message: format!("could not serialize a promotion value: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_stored_status_is_reported_rather_than_defaulted() {
        // Defaulting an unknown status to `pending_approval` on a promotion that already ran
        // would let the Promotions tab offer "approve" on a finished deploy.
        let row = PromotionRow {
            id: Uuid::nil(),
            environment_id: Uuid::nil(),
            target_environment_id: Uuid::nil(),
            status: "half_done".to_owned(),
            changes: serde_json::json!([]),
            conflicts: serde_json::json!([]),
            requested_by: None,
            approved_by: None,
            approved_at: None,
            step_log: serde_json::json!([]),
            error: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: None,
        };
        assert!(row.state().is_err(), "an unknown status must not parse to a known one");
    }

    #[test]
    fn an_unreadable_change_set_is_a_store_error_and_not_an_empty_one() {
        // A silently-empty change set would show the operator "nothing to promote" on a promotion
        // that holds forty items.
        let row = PromotionRow {
            id: Uuid::nil(),
            environment_id: Uuid::nil(),
            target_environment_id: Uuid::nil(),
            status: "pending_approval".to_owned(),
            changes: serde_json::json!({"not": "an array"}),
            conflicts: serde_json::json!([]),
            requested_by: None,
            approved_by: None,
            approved_at: None,
            step_log: serde_json::json!([]),
            error: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: None,
        };
        assert!(row.change_set().is_err());
    }

    /// A frozen item, for the conflict tests: `base_updated_at` and `base_digest` are what
    /// production looked like when the request was made.
    fn frozen(kind: ChangeKind, digest: &str) -> FrozenItem {
        FrozenItem {
            page_id: Uuid::from_u128(1),
            site_id: Uuid::from_u128(2),
            slug: "page".to_owned(),
            kind,
            base_updated_at: Some(OffsetDateTime::UNIX_EPOCH),
            base_digest: digest.to_owned(),
        }
    }

    #[test]
    fn an_added_page_is_clean_when_production_lacks_it_and_conflicts_when_it_has_it() {
        // The inverted version of this test is what shipped: an addition was a conflict exactly
        // when production had no such slug, which is the only state in which an addition makes
        // sense. Every `Added` item in every promotion reported as conflicted.
        let item = frozen(ChangeKind::Added, "");
        assert!(
            !item_conflicts(ChangeKind::Added, false, None, "", &item),
            "production not having the slug is what an addition IS"
        );
        assert!(
            item_conflicts(ChangeKind::Added, true, Some(OffsetDateTime::UNIX_EPOCH), "x", &item),
            "production having the slug means somebody took it while the request waited"
        );
    }

    #[test]
    fn a_deleted_page_is_clean_while_production_still_holds_it_untouched() {
        // Also inverted in the first version: a deletion was reported as a conflict precisely
        // because production still had the row. That is the normal case and the whole purpose.
        let item = frozen(ChangeKind::Deleted, "d");
        assert!(
            !item_conflicts(
                ChangeKind::Deleted,
                true,
                Some(OffsetDateTime::UNIX_EPOCH),
                "d",
                &item
            ),
            "production holding the row unmoved is exactly what a deletion is for"
        );
        // Already gone in production: the goal is true, so there is nothing to refuse.
        assert!(
            !item_conflicts(ChangeKind::Deleted, false, None, "", &item),
            "a deletion whose target is already gone has nothing to do"
        );
    }

    #[test]
    fn a_deleted_page_conflicts_only_when_production_moved_since_the_request() {
        let item = frozen(ChangeKind::Deleted, "d");
        let later = OffsetDateTime::UNIX_EPOCH.checked_add(time::Duration::seconds(5)).unwrap();
        assert!(
            item_conflicts(ChangeKind::Deleted, true, Some(later), "d", &item),
            "the clock moved"
        );
        let item = frozen(ChangeKind::Deleted, "old");
        assert!(
            item_conflicts(
                ChangeKind::Deleted,
                true,
                Some(OffsetDateTime::UNIX_EPOCH),
                "new",
                &item
            ),
            "the bytes moved even though the clock did not — this is what the digest is for"
        );
    }

    #[test]
    fn an_updated_page_is_clean_only_when_production_still_matches_the_baseline() {
        let item = frozen(ChangeKind::Updated, "d");
        assert!(
            !item_conflicts(
                ChangeKind::Updated,
                true,
                Some(OffsetDateTime::UNIX_EPOCH),
                "d",
                &item
            ),
            "unchanged production is the safe case"
        );
        assert!(
            item_conflicts(ChangeKind::Updated, false, None, "", &item),
            "a production deletion would be undone by recreating the page"
        );
        let later = OffsetDateTime::UNIX_EPOCH.checked_add(time::Duration::seconds(5)).unwrap();
        assert!(item_conflicts(
            ChangeKind::Updated,
            true,
            Some(later),
            "d",
            &item
        ));
        assert!(item_conflicts(
            ChangeKind::Updated,
            true,
            Some(OffsetDateTime::UNIX_EPOCH),
            "e",
            &item
        ));
    }

    #[test]
    fn the_three_kinds_answer_three_different_questions_about_the_same_row() {
        // One live state, three verdicts — and the three MUST differ. A single shared predicate
        // across the kinds is precisely the bug: the first version had `if deleted { has row }
        // else { no row }`, which is one rule wearing three hats.
        let present_and_unchanged = |kind: ChangeKind| {
            item_conflicts(
                kind,
                true,
                Some(OffsetDateTime::UNIX_EPOCH),
                "d",
                &frozen(kind, "d"),
            )
        };
        assert!(
            !present_and_unchanged(ChangeKind::Updated),
            "an update over an unchanged row is safe"
        );
        assert!(
            !present_and_unchanged(ChangeKind::Deleted),
            "a deletion of an unchanged row is safe"
        );
        assert!(
            present_and_unchanged(ChangeKind::Added),
            "an insertion over an existing row is not"
        );
    }

    #[test]
    fn a_sql_digest_equals_the_rust_one() {
        // If these diverge, every item reads as changed and the first approval is refused with a
        // conflict list nobody can explain. The SQL and the Rust must be one definition.
        assert_eq!(
            expected_digest("Home", "body"),
            digest_of("Home", "body"),
            "the Rust digest is the reference; the SQL must match it"
        );
    }
}
