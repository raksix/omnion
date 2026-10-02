//! The delivery log's retention, driven against a real PostgreSQL (REQ-021, slice 7).
//!
//! ## What this suite exists to measure
//!
//! Two functions had **zero call sites for their whole life**: `push::prune_deliveries` and
//! `push::prune_stale`. The outbox screen publishes "The log goes back 60 days" from a
//! constant those functions were the only readers of, so an administrator was told a floor the
//! installation never enforced. Removing the functions is not the fix — nothing enforces it
//! then. [`omnion_notifications::retention`] is, and this suite measures it.
//!
//! ## Why the unit tests cannot measure this
//!
//! The properties that matter here are about **which rows a delete removes**, and the wrong
//! implementation removes them *identically* as far as any assertion on a return value can
//! tell. So the crate's own tests assert on the statements' text (`the sweep must read the
//! settle clock`, `the sweep carries no status list`) and this walk asserts on the rows that
//! are actually gone.
//!
//! ## Each claim is wrong in a different direction under the obvious implementation
//!
//! * **The clock is the settle instant.** The dead statement selected `created_at`, written
//!   once by `enqueue` and never updated — so a delivery queued on day 1 and settled on day 59
//!   was swept on day 60 *for being sixty days old*, the guarantee measured from the wrong end.
//!   The walk pins `created_at` in the past and `settled_at` recent, and asserts the row
//!   **survives**. The obvious delete removes it.
//! * **A `failed` row is history too.** The old predicate was `status in ('sent','skipped')`,
//!   which meant the failure log — the one an administrator opens the outbox to find — grew
//!   without bound precisely because it was the log people cared about.
//! * **A claimed row is not.** `settled_at is null` is true of a row the runner holds right
//!   now, so a settle-clock-only sweep would delete a delivery mid-send.
//! * **A tenant's window is its own.** One global number makes one customer's compliance
//!   window the whole platform's, so the walk sweeps two organizations with different windows
//!   in one pass and asserts each kept what it should.
//! * **The orgless platform arm is swept too.** `notifications.organization_id` is nullable,
//!   so `=` would drop the platform's own announcements from the work list and let them grow
//!   for ever while the run log claimed to have swept everything.
//!
//! Everything is read back out of PostgreSQL. A report is a number the sweeper produced; the
//! row's absence is the fact.

use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use omnion_notifications::model::NewNotification;
use omnion_notifications::retention::{self, RetentionPolicy};
use omnion_notifications::store;
use sqlx::postgres::PgQueryResult;
use sqlx::PgPool;

/// The delivery queue's own lease, as the runner reads it from the config.
///
/// **The same number, deliberately.** The sweep's guard is "recently claimed", so its window is
/// the queue's window: a sweep using a different one either races an in-flight send (window too
/// short) or keeps an abandoned claim for ever (window too long). A constant here rather than the
/// config, because a walk that reads `Config::from_env` would measure the box's environment
/// rather than the predicate.
const LEASE: f64 = 300.0;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------------

struct Harness {
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    /// A throwaway database with every migration applied. `None` means PostgreSQL is not
    /// reachable and the suite skips rather than fails.
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        let Some(maintenance) = live_db(&config).await else {
            return None;
        };

        let database = format!("omnion_notifret_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
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
            "drop database if exists \"{database}\" with (force)"
        ))
        .execute(self.maintenance.pool())
        .await
        .expect("the temporary database must be removed");
    }
}

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    })
    .await
    {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
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

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

async fn organization(pool: &PgPool, name: &str, window_days: i32) -> Uuid {
    let id: Uuid = sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(format!("retention-{}-{}", name, Uuid::new_v4().simple()))
        .fetch_one(pool)
        .await
        .expect("the organization must be created");

    // **The window is the column the sweep reads, so the fixture writes it.** Inserting an
    // organization and leaving the window at its default would make every "this tenant keeps
    // more" assertion pass for the wrong reason — against the default rather than against what
    // was asked for.
    sqlx::query("update organizations set notification_retention_days = $2 where id = $1")
        .bind(id)
        .bind(window_days)
        .execute(pool)
        .await
        .expect("the retention window must be written");

    id
}

/// A reader in `organization_id`, and the id of the notification written for them.
async fn notification_for(pool: &PgPool, organization_id: Option<Uuid>) -> Uuid {
    let user = omnion_identity::users::create_user(
        pool,
        omnion_identity::users::NewUser {
            email: format!("retention-{}@omnion.test", Uuid::new_v4().simple()),
            password: "correct horse battery".to_owned(),
            display_name: "Retention".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the reader must exist — notifications.user_id is a foreign key");

    store::record(
        pool,
        organization_id,
        None,
        &NewNotification {
            user_id: user.id,
            category: "system".to_owned(),
            priority: "normal".to_owned(),
            title: "A page is waiting".to_owned(),
            body: "Somebody asked for a review.".to_owned(),
            url: Some("/reviews/1".to_owned()),
            source_type: Some("page".to_owned()),
            source_id: None,
            payload: serde_json::json!({}),
            dedupe_key: None,
        },
    )
    .await
    .expect("the notification must be recorded");

    sqlx::query_scalar("select id from notifications where user_id = $1")
        .bind(user.id)
        .fetch_one(pool)
        .await
        .expect("the recorded notification must be readable")
}

/// The delivery row a fixture wants, for the channel it asks for.
async fn delivery_row(pool: &PgPool, notification_id: Uuid, channel: &str) -> Uuid {
    sqlx::query_scalar(
        "insert into notification_deliveries (notification_id, channel, status) \
         values ($1, $2, 'pending') returning id",
    )
    .bind(notification_id)
    .bind(channel)
    .fetch_one(pool)
    .await
    .expect("the delivery row must exist")
}

/// Backdate a row's own clock. `settled_at = null` leaves the enqueue clock in play, which is
/// what makes a row "abandoned" rather than "finished".
///
/// **`settled` is DAYS IN THE PAST, and a negative value is refused.** The parameter is added to
/// `now()` inside a `make_interval(days => …)`, so `-45` dates the row 45 days into the FUTURE.
/// A future `settled_at` is on the far side of every window predicate, so the row is correctly
/// kept and the count correctly answers `0` — and the failure reads as a product defect, because
/// the walk's own comment says "45 days old" while the fixture wrote the opposite. Three walks
/// were green-adjacent here for exactly that reason: nothing was measuring the clock, because the
/// clock was never in the past.
///
/// The assertion is here rather than at the call site on purpose. A backdate that moves time
/// *forward* is not a weaker fixture, it is a fixture that makes the sweep's guard untestable, and
/// the error it produces points at the product instead of at the line that wrote it.
///
/// **`status` is deliberately left alone.** The sweep's guard is the *claim*, not the status, so
/// the abandoned row this builds stays `pending` — which is exactly the state an abandoned row
/// is in on a real installation, and the state the first version of the predicate excluded.
async fn backdate(pool: &PgPool, id: Uuid, settled: Option<i32>) {
    assert!(
        settled.is_none_or(|days| days >= 0),
        "backdate() takes days IN THE PAST: {settled:?} would date the row in the future, \
         which no retention window can sweep, and the resulting count of 0 would read as a \
         product defect rather than as this mistake"
    );

    let result: PgQueryResult = sqlx::query(
        "update notification_deliveries \
         set created_at = now() - make_interval(days => $2::int), \
             settled_at = case when $3::int is null then null \
                              else now() - make_interval(days => $3::int) end \
         where id = $1",
    )
    .bind(id)
    .bind(90_i32)
    .bind(settled)
    .execute(pool)
    .await
    .expect("the row must be backdated");
    assert_eq!(
        result.rows_affected(),
        1,
        "a fixture that backdates zero rows is measuring nothing"
    );
}

async fn exists(pool: &PgPool, id: Uuid) -> bool {
    let count: i64 = sqlx::query_scalar("select count(*) from notification_deliveries where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the row must be readable");
    count == 1
}

/// A policy for an organization, as the work list would read it.
async fn policy_for(pool: &PgPool, organization_id: Option<Uuid>) -> RetentionPolicy {
    let rows = retention::work_list(pool, 500)
        .await
        .expect("the work list must read");
    rows.into_iter()
        .find(|policy| policy.organization_id == organization_id)
        .unwrap_or(RetentionPolicy {
            organization_id,
            window_days: omnion_notifications::OUTBOX_RETENTION_DAYS,
        })
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// **The settle clock, not the enqueue clock.**
///
/// The row's `created_at` is 90 days old and its `settled_at` is 2 — the shape of a delivery
/// that spent a month retrying and finally arrived. Under the pre-fix predicate it is swept
/// immediately, and the operator who was promised "the log goes back 60 days" loses the row
/// that proves the message arrived.
#[tokio::test]
async fn a_row_settled_inside_the_window_survives_a_sweep() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let organization_id = organization(&pool, "recent", 60).await;
    let notification = notification_for(&pool, Some(organization_id)).await;
    let delivery = delivery_row(&pool, notification, "email").await;
    // 90 days old by enqueue, 2 days old by settle.
    backdate(&pool, delivery, Some(2)).await;

    let policy = policy_for(&pool, Some(organization_id)).await;
    assert_eq!(policy.window_days, 60, "the fixture's own window must be read back");

    let report = retention::sweep_deliveries(&pool, policy, 1_000, LEASE)
        .await
        .expect("the sweep must run");
    assert_eq!(
        report.deliveries_deleted, 0,
        "a row settled 2 days ago is inside a 60-day window and must not be swept"
    );
    assert!(exists(&pool, delivery).await, "the row must still be there");

    harness.dispose().await;
}

/// **The other end of the same sentence: a row settled past the window goes.**
///
/// Without this the assertion above is satisfied by a sweep that removes nothing at all, which
/// is the tick-68 shape: a gate whose every assertion is "the row survived" is also satisfied
/// by a delete that never fires.
#[tokio::test]
async fn a_row_settled_past_the_window_is_swept() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let organization_id = organization(&pool, "stale", 60).await;
    let notification = notification_for(&pool, Some(organization_id)).await;
    let delivery = delivery_row(&pool, notification, "email").await;
    // Created 2 days ago, settled 90 days ago: a clock that runs backwards, which only happens
    // in a fixture — and is exactly the case a `created_at` predicate would sweep while a
    // settle-clock predicate keeps, because the settle instant is the real one.
    sqlx::query(
        "update notification_deliveries \
         set created_at = now() - make_interval(days => 2::int), \
             settled_at = now() - make_interval(days => $2::int) \
         where id = $1",
    )
    .bind(delivery)
    .bind(90_i32)
    .execute(&pool)
    .await
    .expect("the row must be backdated");

    let policy = policy_for(&pool, Some(organization_id)).await;
    let report = retention::sweep_deliveries(&pool, policy, 1_000, LEASE)
        .await
        .expect("the sweep must run");
    assert_eq!(report.deliveries_deleted, 1, "a row settled 90 days ago must go");
    assert!(!exists(&pool, delivery).await, "the row must be gone");

    harness.dispose().await;
}

/// **A `failed` row is history, and the old predicate could never remove one.**
///
/// `status in ('sent','skipped')` meant the failure log grew without bound precisely because
/// it was the log people cared about — and a reader still looking at a delivery that failed
/// sixty days ago would still see the failure on the screen. The sweep no longer keeps a
/// status list, so this row is judged on its settle instant like any other.
#[tokio::test]
async fn a_failed_row_past_the_window_is_swept_too() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let organization_id = organization(&pool, "failures", 60).await;
    let notification = notification_for(&pool, Some(organization_id)).await;
    let delivery = delivery_row(&pool, notification, "email").await;
    sqlx::query(
        "update notification_deliveries \
         set status = 'failed', created_at = now() - make_interval(days => 90::int), \
             settled_at = now() - make_interval(days => 90::int) \
         where id = $1",
    )
    .bind(delivery)
    .execute(&pool)
    .await
    .expect("the row must be marked failed");

    let policy = policy_for(&pool, Some(organization_id)).await;
    let report = retention::sweep_deliveries(&pool, policy, 1_000, LEASE)
        .await
        .expect("the sweep must run");
    assert_eq!(
        report.deliveries_deleted, 1,
        "the failure log must not be the one log that grows for ever"
    );

    harness.dispose().await;
}

/// **A row the runner holds right now is not history — and it is the CLAIM that says so.**
///
/// `settled_at` is null on a claimed row, so a settle-clock-only sweep would delete a delivery
/// out from under an in-flight send. The lease predicate is the protection, and this is the
/// assertion for it.
///
/// The first version of the guard was `status <> 'pending'` and this walk passed with it in
/// place — because every other row here is settled, so every other assertion held. The abandoned
/// walk is the one that fails, and it failed with *zero deletions* across every should-have-gone
/// row in the file. A guard that reads like the right one and silently deletes the wrong
/// population is the reason this walk is a separate file.
#[tokio::test]
async fn a_pending_row_is_never_swept_however_old_it_is() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let organization_id = organization(&pool, "claimed", 60).await;
    let notification = notification_for(&pool, Some(organization_id)).await;
    let delivery = delivery_row(&pool, notification, "email").await;
    // Old by BOTH clocks: `settled_at` is null (nobody has settled it) and `created_at` is 90
    // days back, and the runner holds it right now.
    backdate(&pool, delivery, None).await;
    sqlx::query("update notification_deliveries set claimed_at = now() where id = $1")
        .bind(delivery)
        .execute(&pool)
        .await
        .expect("the row must be claimed");

    let policy = policy_for(&pool, Some(organization_id)).await;
    let report = retention::sweep_deliveries(&pool, policy, 1_000, LEASE)
        .await
        .expect("the sweep must run");
    assert_eq!(report.deliveries_deleted, 0, "a claimed row is not history");
    assert!(
        exists(&pool, delivery).await,
        "the sweep must not delete a delivery mid-send"
    );

    harness.dispose().await;
}

/// **An abandoned row — nobody ever settled it and nobody is holding it — IS swept.**
///
/// Without the `coalesce(settled_at, created_at)` fallback a row nobody ever settles would pin
/// its own history for ever, which is the opposite of what retention is for: a channel with no
/// transport, a queue nobody drains, a reader who never gets an answer. Those rows are the bulk
/// of an abandoned installation's log.
#[tokio::test]
async fn an_abandoned_row_is_swept_on_the_enqueue_clock() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let organization_id = organization(&pool, "abandoned", 60).await;
    let notification = notification_for(&pool, Some(organization_id)).await;
    let delivery = delivery_row(&pool, notification, "chat").await;
    // `settled_at` null (never settled), `created_at` 90 days back, nothing claiming it.
    backdate(&pool, delivery, None).await;

    let policy = policy_for(&pool, Some(organization_id)).await;
    let report = retention::sweep_deliveries(&pool, policy, 1_000, LEASE)
        .await
        .expect("the sweep must run");
    assert_eq!(
        report.deliveries_deleted, 1,
        "an abandoned row must not pin its own history for ever"
    );

    harness.dispose().await;
}

/// **Each tenant keeps its own window, and one pass sweeps both.**
///
/// The single global number is the obvious design and it is wrong twice: it makes a compliance
/// window an installation-wide setting, and it cannot tell two tenants apart. Two organizations
/// with 5 and 365 days, one row each past the *short* window and inside the long one.
#[tokio::test]
async fn a_tenant_with_a_longer_window_keeps_what_a_shorter_one_loses() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let strict = organization(&pool, "strict", 5).await;
    let patient = organization(&pool, "patient", 365).await;

    let strict_notification = notification_for(&pool, Some(strict)).await;
    let strict_delivery = delivery_row(&pool, strict_notification, "email").await;
    let patient_notification = notification_for(&pool, Some(patient)).await;
    let patient_delivery = delivery_row(&pool, patient_notification, "email").await;
    // Both 10 days old by both clocks — past the strict tenant's 5, well inside 365.
    sqlx::query(
        "update notification_deliveries \
         set created_at = now() - make_interval(days => 10::int), \
             settled_at = now() - make_interval(days => 10::int) \
         where id = any($1)",
    )
    .bind(vec![strict_delivery, patient_delivery])
    .execute(&pool)
    .await
    .expect("both rows must be backdated");

    let strict_policy = policy_for(&pool, Some(strict)).await;
    let patient_policy = policy_for(&pool, Some(patient)).await;
    assert_eq!(strict_policy.window_days, 5);
    assert_eq!(patient_policy.window_days, 365);

    let first = retention::sweep_deliveries(&pool, strict_policy, 1_000, LEASE)
        .await
        .expect("the strict tenant's sweep must run");
    let second = retention::sweep_deliveries(&pool, patient_policy, 1_000, LEASE)
        .await
        .expect("the patient tenant's sweep must run");

    assert_eq!(first.deliveries_deleted, 1, "10 days is past a 5-day window");
    assert_eq!(
        second.deliveries_deleted, 0,
        "10 days is well inside a 365-day window, and one tenant's policy must not shorten \\
         another's log"
    );
    assert!(!exists(&pool, strict_delivery).await);
    assert!(exists(&pool, patient_delivery).await);

    harness.dispose().await;
}

/// **The orgless platform arm is swept, not skipped.**
///
/// `notifications.organization_id` is nullable and the platform raises its own notifications
/// with no tenant. A `=` match would drop every one of them from the work list — so the
/// platform's history would grow without bound while the run log claimed to have swept
/// everything. This is the same population `push::OutboxScope::Platform` names on the read
/// side, and the two halves have to agree or the screen shows traffic no sweep will reach.
#[tokio::test]
async fn the_platform_arm_is_swept_too() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let notification = notification_for(&pool, None).await;
    let delivery = delivery_row(&pool, notification, "email").await;
    backdate(&pool, delivery, Some(90)).await;

    // The work list must *name* the platform arm. Asserting on the sweep's result alone would
    // pass on a work list that simply omitted it and a policy the fixture invented.
    let rows = retention::work_list(&pool, 500)
        .await
        .expect("the work list must read");
    assert!(
        rows.iter().any(|policy| policy.organization_id.is_none()),
        "the orgless platform traffic must appear in the work list"
    );

    let report = retention::run_pass(&pool, 500, 1_000, LEASE)
        .await
        .expect("the pass must run");
    assert_eq!(report.deliveries_deleted, 1, "the platform's own history must be swept");
    assert!(!exists(&pool, delivery).await);

    harness.dispose().await;
}

/// **A run that deletes nothing still writes its row.**
///
/// "The last sweep was at 03:00 and it found nothing" is the sentence an operator needs on the
/// day they ask why a March delivery is still on the screen, and a log that only records
/// activity cannot answer it on the day nothing happened.
#[tokio::test]
async fn an_empty_pass_still_writes_its_run_row() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let organization_id = organization(&pool, "idle", 60).await;
    // **A notification, so the work list names this organization at all.** The first version of
    // this walk created the tenant and stopped, then asserted a run row — and the pass reported
    // nothing because `work_list` walks *organizations that have notifications*, the same
    // "who has work" list `event_retention_runner` uses. An organization with no notifications
    // has no delivery history to sweep, so the sweep is right not to visit it and the walk was
    // asserting a row for a tenant that was never on the list.
    //
    // The fixture's own mistake is the finding: it read "an empty pass" as "nothing exists"
    // when the product's meaning is "something exists and none of it is old". Those are
    // different states, and only the second one is the property the run log exists to record.
    let notification = notification_for(&pool, Some(organization_id)).await;
    let delivery = delivery_row(&pool, notification, "email").await;
    // Recent by both clocks, so the sweep has nothing to do.
    backdate(&pool, delivery, Some(1)).await;

    let report = retention::run_pass(&pool, 500, 1_000, LEASE)
        .await
        .expect("the pass must run");
    assert!(
        report.walked >= 1,
        "the work list must name an organization that has notifications"
    );
    assert_eq!(report.deliveries_deleted, 0);
    assert_eq!(
        report.failed, 0,
        "a pass with nothing to do is idle, not failed — a worker that warns on every empty \\
         sweep is a worker whose real warnings stop being read"
    );

    let runs: i64 = sqlx::query_scalar(
        "select count(*) from notification_retention_runs where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(&pool)
    .await
    .expect("the run log must be readable");
    assert!(
        runs >= 1,
        "a sweep that deleted nothing must still be on the record — otherwise 'nothing has \\
         been swept since March' and 'every sweep has been failing since March' read the same"
    );

    harness.dispose().await;
}

/// **The batch is bounded, and the remainder is worked down rather than lost.**
///
/// One `delete` over an entire backlog takes a lock proportional to the whole table. The bound
/// is asserted by the *count* rather than by a return value, because a sweep that removed
/// nothing and a sweep that removed everything both report zero here — the assertion that
/// distinguishes them is what remains in the table.
#[tokio::test]
async fn a_bounded_sweep_leaves_the_remainder_for_the_next_tick() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let organization_id = organization(&pool, "bounded", 5).await;
    let notification = notification_for(&pool, Some(organization_id)).await;

    let mut rows = Vec::new();
    for _ in 0..5 {
        let own = notification_for(&pool, Some(organization_id)).await;
        rows.push(delivery_row(&pool, own, "email").await);
    }
    // **One notification per row, and the reason is a constraint this crate's own migration
    // documents.** `notification_deliveries_unique` is on `(notification_id, channel)`, so five
    // rows on one notification with one channel is impossible — and `store::record` already
    // enqueued the first `email` row, so even a two-row version was refused. The first draft of
    // this walk hit that 23505, then *worked around* it with a dead first loop and a
    // `rows.clear()`, which is the worst possible repair: the fixture read as though it built
    // five rows and in fact built one, and the assertion still passed for the first two of them.
    let _ = notification;
    sqlx::query(
        "update notification_deliveries \
         set created_at = now() - make_interval(days => 10::int), \
             settled_at = now() - make_interval(days => 10::int) \
         where id = any($1)",
    )
    .bind(&rows)
    .execute(&pool)
    .await
    .expect("every row must be backdated");

    let policy = policy_for(&pool, Some(organization_id)).await;
    let first = retention::sweep_deliveries(&pool, policy, 2, LEASE)
        .await
        .expect("the sweep must run");
    assert_eq!(
        first.deliveries_deleted, 2,
        "a bounded sweep must not take the whole backlog in one transaction"
    );
    let remaining: i64 = sqlx::query_scalar(
        "select count(*) from notification_deliveries where id = any($1)",
    )
    .bind(&rows)
    .fetch_one(&pool)
    .await
    .expect("the remaining rows must be readable");
    assert_eq!(remaining, 3, "the remainder must still be there for the next tick");

    harness.dispose().await;
}

/// **The settle instant is stamped by the writers, not inferred by the sweep.**
///
/// Every function that leaves a row `pending` must clear `settled_at`, or a requeued delivery
/// claims to be finished and is judged by an instant from a delivery that already ended. The
/// sweep's `status <> 'pending'` guard rescues the row today, so a test that only asserted
/// "a pending row is kept" would pass with the omission in place — this walks the real retry.
#[tokio::test]
async fn a_retry_clears_the_settle_instant() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let organization_id = organization(&pool, "retried", 60).await;
    let notification = notification_for(&pool, Some(organization_id)).await;
    let delivery = delivery_row(&pool, notification, "email").await;
    sqlx::query(
        "update notification_deliveries \
         set status = 'failed', settled_at = now() - make_interval(days => 30::int), \
             error = 'the receiving side refused the message' \
         where id = $1",
    )
    .bind(delivery)
    .execute(&pool)
    .await
    .expect("the row must be marked failed");

    let outcome = omnion_notifications::push::retry_delivery(
        &pool,
        omnion_notifications::OutboxScope::Organization(organization_id),
        delivery,
    )
    .await
    .expect("the retry must run");
    assert_eq!(
        outcome,
        omnion_notifications::RetryOutcome::Requeued,
        "a failed row inside the scope must be requeued"
    );

    let settled: Option<time::OffsetDateTime> =
        sqlx::query_scalar("select settled_at from notification_deliveries where id = $1")
            .bind(delivery)
            .fetch_one(&pool)
            .await
            .expect("the row must be readable");
    assert!(
        settled.is_none(),
        "a requeued row is not settled; leaving the stamp would judge it by an instant from a \\
         delivery that already ended"
    );

    harness.dispose().await;
}

/// **The device sweep keeps `last_seen_at` as its clock.**
///
/// `push_subscriptions` has no settle instant at all — its lifecycle is `last_seen_at`. Copying
/// the delivery clock across that boundary would invent a column's meaning rather than reuse
/// one, and the next person to "make the two statements consistent" would do it silently.
#[tokio::test]
async fn a_device_is_pruned_on_its_own_sighting_clock() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let organization_id = organization(&pool, "devices", 60).await;
    let user = omnion_identity::users::create_user(
        &pool,
        omnion_identity::users::NewUser {
            email: format!("device-{}@omnion.test", Uuid::new_v4().simple()),
            password: "correct horse battery".to_owned(),
            display_name: "Device".to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("the reader must exist");

    let subscription: Uuid = sqlx::query_scalar(
        "insert into push_subscriptions (user_id, endpoint, p256dh, auth) \
         values ($1, $2, 'key', 'auth') returning id",
    )
    .bind(user.id)
    .bind(format!("https://push.example/{}", Uuid::new_v4().simple()))
    .fetch_one(&pool)
    .await
    .expect("the subscription must exist");

    // Two devices: one the reader has not touched in months, one seen an hour ago. The second
    // is the assertion that separates "pruned on its own clock" from "pruned everything".
    let fresh: Uuid = sqlx::query_scalar(
        "insert into push_subscriptions (user_id, endpoint, p256dh, auth) \
         values ($1, $2, 'key', 'auth') returning id",
    )
    .bind(user.id)
    .bind(format!("https://push.example/{}", Uuid::new_v4().simple()))
    .fetch_one(&pool)
    .await
    .expect("the second subscription must exist");

    sqlx::query(
        "update push_subscriptions set last_seen_at = now() - make_interval(days => 90::int) \
         where id = $1",
    )
    .bind(subscription)
    .execute(&pool)
    .await
    .expect("the stale device must be backdated");
    sqlx::query("update push_subscriptions set last_seen_at = now() - interval '1 hour' where id = $1")
        .bind(fresh)
        .execute(&pool)
        .await
        .expect("the live device must be seen");

    let policy = policy_for(&pool, Some(organization_id)).await;
    let report = retention::sweep_deliveries(&pool, policy, 1_000, LEASE)
        .await
        .expect("the sweep must run");
    assert_eq!(
        report.devices_deleted, 1,
        "only the device nobody has seen for months is history"
    );

    let survivors: i64 = sqlx::query_scalar("select count(*) from push_subscriptions where id = $1")
        .bind(fresh)
        .fetch_one(&pool)
        .await
        .expect("the live device must be readable");
    assert_eq!(survivors, 1, "a device seen an hour ago must survive");

    harness.dispose().await;
}

/// **The sweep never touches a device belonging to another tenant's reader.**
///
/// The device statement joins `users`, because `push_subscriptions` carries no organization of
/// its own — the tenancy of a device is the tenancy of the person who owns it. An unscoped
/// sweep would take every stale device on the installation in whichever organization's turn it
/// came, which is the same cross-tenant deletion the outbox's *retry* was in slice 44.
#[tokio::test]
async fn a_devices_sweep_is_scoped_to_the_readers_own_organization() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    let mine = organization(&pool, "mine", 60).await;
    let theirs = organization(&pool, "theirs", 60).await;

    let stranger = omnion_identity::users::create_user(
        &pool,
        omnion_identity::users::NewUser {
            email: format!("stranger-{}@omnion.test", Uuid::new_v4().simple()),
            password: "correct horse battery".to_owned(),
            display_name: "Stranger".to_owned(),
            organization_id: Some(theirs),
        },
    )
    .await
    .expect("the other tenant's reader must exist");

    let theirs_device: Uuid = sqlx::query_scalar(
        "insert into push_subscriptions (user_id, endpoint, p256dh, auth) \
         values ($1, $2, 'key', 'auth') returning id",
    )
    .bind(stranger.id)
    .bind(format!("https://push.example/{}", Uuid::new_v4().simple()))
    .fetch_one(&pool)
    .await
    .expect("the other tenant's device must exist");
    sqlx::query(
        "update push_subscriptions set last_seen_at = now() - make_interval(days => 90::int) \
         where id = $1",
    )
    .bind(theirs_device)
    .execute(&pool)
    .await
    .expect("the device must be backdated");

    // The policy names MY organization. The stale device belongs to theirs.
    let policy = RetentionPolicy {
        organization_id: Some(mine),
        window_days: 60,
    };
    let report = retention::sweep_deliveries(&pool, policy, 1_000, LEASE)
        .await
        .expect("the sweep must run");
    assert_eq!(report.devices_deleted, 0, "another tenant's device is not ours to prune");

    let survivors: i64 = sqlx::query_scalar("select count(*) from push_subscriptions where id = $1")
        .bind(theirs_device)
        .fetch_one(&pool)
        .await
        .expect("the device must be readable");
    assert_eq!(survivors, 1, "a device belonging to another tenant must survive our sweep");

    harness.dispose().await;
}

/// **The screen's number and the sweep's fallback are the same number.**
///
/// The outbox route sends `retention_days` from this constant and the work list binds it as
/// the fallback for an organization with no row. A second literal would let the two disagree
/// without either file mentioning it — and this is the assertion that would have caught the
/// promise nothing enforced.
#[tokio::test]
async fn the_fallback_window_is_the_number_the_screen_publishes() {
    assert_eq!(omnion_notifications::OUTBOX_RETENTION_DAYS, 60);
    assert_eq!(
        omnion_notifications::push::SUBSCRIPTION_STALE_DAYS,
        omnion_notifications::DEFAULT_DEVICE_STALE_DAYS,
        "'how long is a dead device kept' must have one answer between the screen and the \\
         sweeper"
    );
}

// ---------------------------------------------------------------------------------------------
// Slice 8: the window a person can read and change
//
// The sweep existed with no reader and no setter: `organizations.notification_retention_days`
// was a column only a worker consulted, so the sentence on the outbox screen ("the log goes
// back 60 days") was published from a constant while a tenant's own window went unmentioned and
// unsettable. These walks measure the read and the write, and the walk that ties them together
// is the one below — `due` counted on a window the sweep does not use would make the panel's
// "the next sweep removes N" a second lie.
// ---------------------------------------------------------------------------------------------

/// **The read answers the column, not the constant.**
///
/// The defect this closes is a *sentence* disagreeing with the sweeper: the outbox route
/// returned `OUTBOX_RETENTION_DAYS` for every tenant forever, so a tenant on a 7-day window was
/// told it kept 60 days of history while its rows went after seven. A walk that only asserted
/// "the read is 60" would pass against the broken version, so the assertion is on the tenant
/// that set a window of its own.
#[tokio::test]
async fn a_tenant_reads_back_the_window_it_set() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let tenant = organization(harness.pool(), "reader", 7).await;
    let other = organization(harness.pool(), "bystander", 90).await;

    // **Both tenants are read, in one pool, in one process.** A read that leaked a window
    // between organizations would still answer 7 for the first caller, so the assertion is
    // two-sided.
    assert_eq!(
        retention::retention_window(harness.pool(), Some(tenant)).await.expect("the read must answer"),
        7,
        "a tenant must read its own window back"
    );
    assert_eq!(
        retention::retention_window(harness.pool(), Some(other)).await.expect("the read must answer"),
        90,
        "a second tenant must read its own window, not the first one's"
    );

    harness.dispose().await;
}

/// **The setter writes the column and the read-back is what the column holds.**
///
/// The read-back is not a formality: `set_retention_window` returns `… returning
/// notification_retention_days` rather than the number it was handed, so a column check
/// constraint that clamped the value would surface as the clamped value rather than as a
/// silent success. Writing the value back through the same function the screen uses is what
/// makes "the answer is what the database holds" a property rather than a comment.
#[tokio::test]
async fn setting_the_window_writes_the_column_the_sweep_reads() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let tenant = organization(harness.pool(), "setter", 60).await;
    let stored = retention::set_retention_window(harness.pool(), tenant, 14)
        .await
        .expect("the setter must answer");

    assert_eq!(stored, 14, "the setter returns what the column now holds");

    // **Read the column directly as well as through the function.** The sweep reads the column;
    // a function that answered 14 while the column said 60 would leave the sweeper on the old
    // clock, which is precisely the disagreement this slice exists to remove.
    let column: i32 = sqlx::query_scalar(
        "select notification_retention_days from organizations where id = $1",
    )
    .bind(tenant)
    .fetch_one(harness.pool())
    .await
    .expect("the column must be readable");
    assert_eq!(column, 14, "the column the sweep reads must hold the new window");

    harness.dispose().await;
}

/// **An organization that does not exist is an error, not a silent zero-row update.**
///
/// `fetch_optional` returning `None` and the caller falling back to the default would answer
/// `200` and change nothing — the exact "a platform account has no window to set" trap the
/// handler refuses by name. A setter that reported success here would let an operator believe
/// they had changed a retention policy on a tenant that was never there.
#[tokio::test]
async fn setting_the_window_on_a_missing_organization_is_refused() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let absent = Uuid::new_v4();
    let refused = retention::set_retention_window(harness.pool(), absent, 30)
        .await
        .expect_err("a missing organization must be refused, not silently ignored");

    assert!(
        refused.to_string().contains(&absent.to_string()),
        "the refusal names the organization it could not find: {refused}"
    );

    harness.dispose().await;
}

/// **`due` is the sweep's predicate, not a count written for the panel.**
///
/// This is the walk that ties the screen to the worker. `retention_counts` reads its window out
/// of the *same subquery* `work_list` binds, and filters on the *same* claim guard `sweep_
/// deliveries` deletes with. If either drifted, the panel would promise removals the sweeper
/// does not make — the mirror image of the sentence it published before this slice.
///
/// **A row the sweeper would delete is counted; a row it would keep is not.** The two sides are
/// one fixture with two clocks, so a count that was simply wrong (say, `rows`) fails this too.
#[tokio::test]
async fn the_due_count_is_the_clock_the_sweeper_deletes_on() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let tenant = organization(harness.pool(), "counts", 30).await;
    let notification = notification_for(harness.pool(), Some(tenant)).await;

    // Two rows on one notification, on two channels — the pair `notification_deliveries_unique`
    // admits. Built through the fixture that exists for it, not by hand.
    let due = delivery_row(harness.pool(), notification, "in_app").await;
    let kept = delivery_row(harness.pool(), notification, "email").await;

    // **Both are settled long ago**, so the only thing that can separate them is the window:
    // 45 days back crosses a 30-day window and not a 400-day one.
    backdate(harness.pool(), due, Some(45)).await;
    backdate(harness.pool(), kept, Some(45)).await;

    let (rows, due_count) = retention::retention_counts(harness.pool(), Some(tenant), LEASE)
        .await
        .expect("the counts must answer");
    assert_eq!(rows, 2, "both rows belong to this tenant");
    assert_eq!(
        due_count, 2,
        "both rows are 45 days old against a 30-day window, so both are due"
    );

    // Now prove it is *the sweep's* clock by moving the window under the same rows: at 400 days
    // nothing is due, at 30 days both are. A count that had bound the window as a parameter
    // from somewhere else would answer the same for both.
    retention::set_retention_window(harness.pool(), tenant, 400).await.expect("widen");
    let (_, none_due) = retention::retention_counts(harness.pool(), Some(tenant), LEASE)
        .await
        .expect("the counts must answer");
    assert_eq!(none_due, 0, "a 400-day window keeps a 45-day-old row");

    // **And the sweeper agrees, on the same rows, at the same window.** This is the load-bearing
    // half: the count and the delete are two queries, and only running the second one proves
    // they were written to mean the same thing.
    retention::set_retention_window(harness.pool(), tenant, 30).await.expect("narrow");
    let policy = policy_for(harness.pool(), Some(tenant)).await;
    let report = retention::sweep_deliveries(harness.pool(), policy, 1_000, LEASE)
        .await
        .expect("the sweep must answer");

    assert_eq!(report.deliveries_deleted, 2, "the sweeper removes exactly what was counted");
    assert!(!exists(harness.pool(), due).await, "the counted row is gone");
    assert!(!exists(harness.pool(), kept).await, "the kept row is gone at the narrower window");
    let (rows_after, due_after) = retention::retention_counts(harness.pool(), Some(tenant), LEASE)
        .await
        .expect("the counts must answer");
    assert_eq!(rows_after, 0, "the count empties when the sweeper does");
    assert_eq!(due_after, 0, "and nothing stays due");

    harness.dispose().await;
}

/// **The last run is readable, and "never" is a real answer rather than a missing row.**
///
/// `retention_status` is the read side of a table whose only reader was the worker that wrote
/// it. Without this walk the run log has no reader at all, and the panel would have a permanent
/// "no sweep has run" line on an installation that sweeps hourly — the silent-zero shape.
#[tokio::test]
async fn the_last_sweep_is_readable_and_its_absence_is_an_answer() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let tenant = organization(harness.pool(), "runs", 45).await;
    let notification = notification_for(harness.pool(), Some(tenant)).await;
    let row = delivery_row(harness.pool(), notification, "in_app").await;
    backdate(harness.pool(), row, Some(90)).await;

    // **Before any pass:** the status answers, and the absence is a `None` rather than an error
    // or a fabricated row. An empty pass writes a run row, so "no sweep here" and "the sweep
    // found nothing" are two different answers and the panel renders both.
    let before = retention::retention_status(harness.pool(), Some(tenant), LEASE)
        .await
        .expect("the status must answer before any pass");
    assert_eq!(before.window_days, 45, "the status carries the window");
    assert_eq!(before.due, 1, "the one 90-day-old row is due at 45 days");
    assert!(before.last_run.is_none(), "no pass has run, so there is no last run");

    let pass = retention::run_pass(harness.pool(), 500, 1_000, LEASE)
        .await
        .expect("a pass must answer");
    assert!(pass.walked >= 1, "the tenant must be on the work list");

    let after = retention::retention_status(harness.pool(), Some(tenant), LEASE)
        .await
        .expect("the status must answer after a pass");
    let run = after.last_run.expect("a finished pass must be readable as the last run");
    assert_eq!(run.window_days, 45, "the run row carries the window it swept on");
    assert_eq!(run.deliveries_deleted, 1, "and what it removed");
    assert_eq!(run.organization_id, Some(tenant), "and which tenant it swept");
    assert_eq!(after.rows, 0, "the log is empty after the sweep");
    assert_eq!(after.due, 0, "and nothing remains due");

    harness.dispose().await;
}
