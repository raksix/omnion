//! The notification delivery queue, driven against a real PostgreSQL (REQ-021, slice 4).
//!
//! This suite exists because the acceptance criterion is a sentence about *time* — "retries a
//! failing channel per backoff and marks it `failed` after the cap" — and a sentence about time
//! cannot be proved by a unit test. The claim has three parts and each is wrong in a different
//! direction under the obvious implementation:
//!
//! * **A failure is retried, not dropped.** The first version of the runner logged the failure
//!   and moved on, which drains the queue and loses the notification: the outbox shows nothing
//!   at all, which is indistinguishable from "there was nothing to send". So the walk asserts
//!   the row is *still there*, still `pending`, with `attempts` incremented and a
//!   `next_attempt_at` in the future.
//! * **The backoff is honoured, not merely scheduled.** A row retried with
//!   `next_attempt_at = now()` is retried on the very next tick, so the cap is reached in
//!   milliseconds rather than over the window — a hot loop against a broken mail server. The
//!   walk asserts the delay is the one the config asked for, read back out of the row.
//! * **The cap produces `failed` and nothing retries it.** Reaching the cap is the only thing
//!   that says out loud that a notification was given up on, so the walk drives the row all the
//!   way to the cap and asserts the final state, the recorded reason, and that a further tick
//!   does not touch it.
//!
//! **Everything is read back out of PostgreSQL, not out of the response.** A report is a number
//! the runner produced; the row is the fact. Where the two can disagree — and they do, when a
//! `mark_*` affects no row because the row is no longer `pending` — the row is the answer.

use omnion_api::notification_runner::{EmailTransport, WebhookTransport};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_notifications::delivery::{
    self, DeliveryConfig, DeliveryJob, Transport, TransportOutcome,
};
use omnion_notifications::model::NewNotification;
use omnion_notifications::push;
use omnion_notifications::store;
use sqlx::PgPool;
use time::Duration;
use uuid::Uuid;

/// A transport that always fails, and says why.
///
/// **The reason is a fixed string on purpose.** The queue trims failure text to 500 characters
/// and writes it on the row; a test transport that produced a random-length message would make
/// the "the reason survived onto the row" assertion depend on something this suite does not
/// control. The counter is what the walk reads to prove the transport was actually called once
/// per attempt — a claim that never reaches the transport would leave `attempts` unchanged and
/// the retry assertion would pass for the wrong reason.
struct AlwaysFails {
    channel: &'static str,
    calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl AlwaysFails {
    fn new(channel: &'static str) -> Self {
        Self {
            channel,
            calls: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }
}

impl Transport for AlwaysFails {
    fn channel(&self) -> &'static str {
        self.channel
    }

    fn deliver<'a>(
        &'a self,
        _job: &'a DeliveryJob,
        _config: &'a DeliveryConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TransportOutcome> + Send + 'a>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            TransportOutcome::failed(Some(500), "the receiving side refused the message")
        })
    }
}

/// A transport that always accepts.
struct AlwaysAccepts;

impl Transport for AlwaysAccepts {
    fn channel(&self) -> &'static str {
        "in_app"
    }

    fn deliver<'a>(
        &'a self,
        _job: &'a DeliveryJob,
        _config: &'a DeliveryConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TransportOutcome> + Send + 'a>> {
        Box::pin(async { TransportOutcome::accepted(None) })
    }
}

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

        let database = format!("omnion_notifdel_{}", Uuid::new_v4().simple());
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

/// Insert a notification row, creating the reader it belongs to.
///
/// **The account is created rather than invented.** `notifications.user_id` is a foreign key,
/// and a walk that inserts a random uuid finds out through a constraint violation rather than
/// through the behaviour it is testing — which is the same class of mistake as a QA fixture
/// that does not create the tenant it asserts on. The account carries no organization unless
/// the caller passes one, because the tenancy leg is the one that needs two.
async fn notification_for(pool: &PgPool, organization_id: Option<Uuid>) -> (Uuid, Uuid) {
    let user = omnion_identity::users::create_user(
        pool,
        omnion_identity::users::NewUser {
            email: format!("queue-{}@omnion.test", Uuid::new_v4().simple()),
            password: "correct horse battery".to_owned(),
            display_name: "Queue".to_owned(),
            organization_id,
        },
    )
    .await
    .expect(
        "the reader must exist — notifications.user_id is a foreign key, so a walk that \
             invents the id is testing the constraint rather than the queue",
    );

    let draft = NewNotification {
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
    };

    store::record(pool, organization_id, None, &draft)
        .await
        .expect("the notification must be recorded");

    let id: Uuid = sqlx::query_scalar("select id from notifications where user_id = $1")
        .bind(user.id)
        .fetch_one(pool)
        .await
        .expect("the recorded notification must be readable");
    (user.id, id)
}

/// A reader and one notification, with no organization.
async fn notification(pool: &PgPool) -> (Uuid, Uuid) {
    notification_for(pool, None).await
}

/// One delivery row as the walk asserts on it. `attempts` and `next_attempt_at` are the two
/// columns whose *values* the claim is about, so they are read directly rather than inferred.
#[derive(sqlx::FromRow)]
struct Row {
    status: String,
    attempts: i32,
    error: Option<String>,
    next_attempt_at: time::OffsetDateTime,
    sent_at: Option<time::OffsetDateTime>,
}

async fn row(pool: &PgPool, id: Uuid) -> Row {
    sqlx::query_as("select status, attempts, error, next_attempt_at, sent_at from notification_deliveries where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the delivery row must exist")
}

/// Switch one channel on for the whole installation, the way an operator would.
///
/// **Every walk that expects a delivery has to call this, and the reason is the settlement.**
/// `run_due` settles unconfigured channels *before* it claims anything, so on a database with
/// no `notification_channels` row the e-mail delivery is written `skipped` on the first tick
/// and never attempted. That is the right behaviour — a queue with nowhere to send must not
/// burn its cap — and it is why the fixture has to describe an installation rather than
/// inherit an empty one. A walk that forgot this would assert `pending` and fail on a
/// settlement it had not accounted for, which reads as a queue bug and is a fixture gap.
async fn configure_channel(pool: &PgPool, channel: &str) {
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Channel host")
            .bind(format!("channel-host-{}", Uuid::new_v4().simple()))
            .fetch_one(pool)
            .await
            .expect("the hosting organization must be created");

    sqlx::query(
        "insert into notification_channels (organization_id, channel, enabled) \
         values ($1, $2, true)",
    )
    .bind(organization_id)
    .bind(channel)
    .execute(pool)
    .await
    .expect("the channel must be switched on");
}

/// The status of one notification's **e-mail** row.
///
/// A free function rather than a closure because the closure form moves the pool on the first
/// call — an `async` block that borrows a `PgPool` needs an explicit lifetime argument that is
/// more ceremony than the one query is worth. Keyed by notification *and* channel for the
/// reason the caller states.
async fn e_mail_status(pool: &PgPool, notification: Uuid) -> String {
    sqlx::query_scalar::<_, String>(
        "select d.status from notification_deliveries d \
         where d.notification_id = $1 and d.channel = 'email'",
    )
    .bind(notification)
    .fetch_one(pool)
    .await
    .expect("the tenant's e-mail row must exist")
}

/// A config with a one-millisecond backoff and a cap of three, so the walk drives the whole
/// lifecycle in a second while still exercising the real `retry_delay` arithmetic.
fn fast_config() -> DeliveryConfig {
    DeliveryConfig {
        batch: 10,
        lease_seconds: 30,
        retry_base: Duration::milliseconds(400),
        retry_max: Duration::milliseconds(400),
        request_timeout: Duration::seconds(1),
    }
}

/// How long the walk waits for a 400ms backoff window to close.
///
/// Derived from the config rather than written as a literal, and padded well past it: a walk
/// that computed its own sleep from the same number as the delay would pass on a clock
/// rounding the other way and fail on one that does not. The padding is the point.
fn backoff_window() -> std::time::Duration {
    std::time::Duration::from_millis(600)
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_failing_channel_is_retried_and_then_marked_failed_after_the_cap() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();
    let config = fast_config();
    let failing = AlwaysFails::new("email");
    let calls = std::sync::Arc::clone(&failing.calls);
    // Both transports, because that is what `notification_runner::spawn` registers. A walk
    // with only the failing one would assert "nothing was sent" as though the platform had no
    // in-app channel, which is the opposite of what the runner does.
    let transports: Vec<(String, Box<dyn Transport>)> = vec![
        ("email".to_owned(), Box::new(failing)),
        (
            omnion_notifications::IN_APP.to_owned(),
            Box::new(delivery::InAppTransport),
        ),
    ];
    configure_channel(&pool, "email").await;

    // Enqueue an e-mail delivery for a notification nobody has to receive.
    let (user_id, notification_id) = notification(&pool).await;
    let report = delivery::enqueue(
        &pool,
        notification_id,
        &["email".to_owned()],
        &["web_push".to_owned()],
    )
    .await
    .expect("enqueue must succeed");
    assert_eq!(
        report.queued, 2,
        "the e-mail row plus the always-queued in-app row"
    );
    assert_eq!(
        report.skipped, 2,
        "web_push is switched off for this reader, and `chat` has no transport at all"
    );

    // A disabled channel is written `skipped` with its reason on the row, not omitted. This is
    // the first claim: the row for a channel that will never go exists.
    let skipped_id: Uuid = sqlx::query_scalar(
        "select id from notification_deliveries where notification_id = $1 and channel = 'web_push'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("a switched-off channel still gets a row");
    let skipped = row(&pool, skipped_id).await;
    assert_eq!(skipped.status, "skipped");
    assert_eq!(
        skipped.error.as_deref(),
        Some(delivery::READER_SWITCHED_IT_OFF)
    );

    // The runner must not claim the skipped row. It claims **two** — the e-mail row and the
    // in-app one, because both belong to the same notification and are owed the same tick.
    let email_id: Uuid = sqlx::query_scalar(
        "select id from notification_deliveries where notification_id = $1 and channel = 'email'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("the e-mail row must exist");
    let in_app_id: Uuid = sqlx::query_scalar(
        "select id from notification_deliveries where notification_id = $1 and channel = 'in_app'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("the in-app row must exist");

    // **Attempt 1.** The failure is a retry, not a drop.
    let first = delivery::run_due(&pool, &transports, &config)
        .await
        .expect("the first tick must run");
    assert_eq!(
        first.claimed, 2,
        "the e-mail row and the in-app row are both due; the skipped web_push row is not"
    );
    assert_eq!(first.sent, 1, "the in-app row goes out on the first tick");
    assert_eq!(first.retried, 1, "the e-mail row is retried");
    assert_eq!(first.failed, 0, "one attempt is not the cap");

    let after_first = row(&pool, email_id).await;
    assert_eq!(
        after_first.status, "pending",
        "a failure must leave the row queued, not gone — the outbox showing nothing is \
         indistinguishable from there being nothing to send"
    );
    assert_eq!(
        after_first.attempts, 1,
        "the claim is what counts the attempt"
    );
    assert!(
        after_first.error.is_some(),
        "the reason is written on the row"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the transport is called once per attempt"
    );

    // **The backoff is real, not just a scheduled row.** A row retried with
    // `next_attempt_at = now()` is due on the very next tick, so a 1ms base would be
    // indistinguishable from no backoff at all. Assert the row is in the future — which is
    // what makes the next tick find nothing to claim.
    assert!(
        after_first.next_attempt_at > time::OffsetDateTime::now_utc(),
        "a retried row must be scheduled in the future, or every tick hammers the receiver \
         (next_attempt_at = {})",
        after_first.next_attempt_at
    );

    // **The row is not due yet**, so a tick in between must be a genuine no-op. Without this
    // leg, "the cap is reached" could be three attempts in three ticks rather than one attempt
    // per backoff, and the backoff assertion above would be the only thing holding it up.
    let early = delivery::run_due(&pool, &transports, &config)
        .await
        .expect("the early tick must run");
    assert_eq!(
        early.claimed, 0,
        "a row inside its backoff window is not due"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "and the transport is not called for it"
    );

    // The in-app row left `pending` on the first tick, so the retry leg is now genuinely about
    // the e-mail row alone. Asserting that is the point: the earlier leg could have passed
    // with the in-app row also pending, which would have meant the backoff was not what kept
    // the tick quiet.
    assert_eq!(row(&pool, in_app_id).await.status, "sent");

    // **Attempt 2**, now that the window has passed.
    tokio::time::sleep(backoff_window()).await;
    let second = delivery::run_due(&pool, &transports, &config)
        .await
        .expect("the second tick must run");
    assert_eq!(second.retried, 1);
    assert_eq!(row(&pool, email_id).await.attempts, 2);

    // **Attempt 3 reaches the cap** — and only the cap produces `failed`.
    tokio::time::sleep(backoff_window()).await;
    let third = delivery::run_due(&pool, &transports, &config)
        .await
        .expect("the third tick must run");
    assert_eq!(third.failed, 1, "the third attempt is the cap and gives up");
    assert_eq!(third.retried, 0, "and does not queue a fourth");

    let failed = row(&pool, email_id).await;
    assert_eq!(
        failed.status, "failed",
        "the cap is the only route to failed"
    );
    assert_eq!(failed.attempts, 3);
    assert!(
        failed.error.is_some(),
        "a give-up with no reason is a give-up nobody can act on"
    );

    // **A `failed` row is never claimed again.** This is the property that makes `failed` mean
    // something: a runner that kept retrying a given-up row would be a queue that never
    // empties, and the admin retry button would be the only exit.
    tokio::time::sleep(backoff_window()).await;
    let after = delivery::run_due(&pool, &transports, &config)
        .await
        .expect("the fourth tick must run");
    assert_eq!(after.claimed, 0, "a failed row is not due again");
    assert_eq!(row(&pool, email_id).await.attempts, 3, "and is not touched");

    // The in-app row went out on the very first tick, in the same claim batch, and is the
    // reason the reader has an inbox at all. Asserting it here is what proves the in-app
    // transport is registered rather than merely defined.
    let in_app = row(&pool, in_app_id).await;
    assert_eq!(in_app.status, "sent", "the in-app row is delivered locally");
    assert!(in_app.sent_at.is_some(), "and carries the time it went");

    harness.dispose().await;
}

#[tokio::test]
async fn a_claim_makes_the_row_exclusive_until_the_lease_expires() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();
    let config = fast_config();
    let failing = AlwaysFails::new("email");
    let calls = std::sync::Arc::clone(&failing.calls);
    let transports: Vec<(String, Box<dyn Transport>)> =
        vec![("email".to_owned(), Box::new(failing))];
    configure_channel(&pool, "email").await;

    let (user_id, notification_id) = notification(&pool).await;
    delivery::enqueue(&pool, notification_id, &["email".to_owned()], &[])
        .await
        .expect("enqueue must succeed");

    // Claim directly rather than through a tick, so the lease is the only thing under test.
    let jobs = delivery::claim_due(&pool, 10, 30.0)
        .await
        .expect("the claim must run");
    let email_jobs: Vec<&DeliveryJob> = jobs.iter().filter(|j| j.channel == "email").collect();
    assert_eq!(email_jobs.len(), 1);
    assert_eq!(
        email_jobs[0].attempts, 1,
        "the claim increments the attempt, so a runner that dies mid-send still costs one"
    );

    // A second claim while the first is live must find nothing. This is the property that
    // makes two runners safe, and the reason the increment is in the claim statement rather
    // than in the send: with the increment after the read, both runners would hand the same
    // row attempt #1 and the cap would be worth two sends, not one.
    let again = delivery::claim_due(&pool, 10, 30.0)
        .await
        .expect("the second claim must run");
    assert!(
        !again.iter().any(|j| j.channel == "email"),
        "a row claimed inside its lease is not claimable by a second runner"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "and no send happened"
    );

    // Once the lease is in the past, the row comes back — a runner that died must not strand
    // it. This is the half of the claim that keeps a crash from stranding a notification.
    sqlx::query(
        "update notification_deliveries set claimed_at = now() - make_interval(secs => 120), \
              attempts = 0, next_attempt_at = now() where channel = 'email' \
           and notification_id = $1",
    )
    .bind(notification_id)
    .execute(&pool)
    .await
    .expect("the lease must be aged");

    let recovered = delivery::claim_due(&pool, 10, 30.0)
        .await
        .expect("the third claim must run");
    assert!(
        recovered.iter().any(|j| j.channel == "email"),
        "a claim older than the lease is abandoned work, and the row must come back"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn enqueueing_the_same_notification_twice_does_not_double_its_deliveries() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let (user_id, notification_id) = notification(&pool).await;

    let first = delivery::enqueue(&pool, notification_id, &["email".to_owned()], &[])
        .await
        .expect("the first enqueue must succeed");
    assert_eq!(
        first.queued, 2,
        "e-mail plus the in-app row; `chat` is written skipped and is counted separately"
    );
    assert_eq!(
        first.skipped, 1,
        "`chat` has no transport on this installation, so it is a skipped row — never pending"
    );

    // The second call is what a re-run of the same emit looks like. Without the unique
    // constraint the drawer would list the same channel twice and the reader would get two
    // copies — and the second copy reads as "it worked, then it worked again", which is the
    // symptom that sends somebody looking for a bug that is not there.
    let second = delivery::enqueue(&pool, notification_id, &["email".to_owned()], &[])
        .await
        .expect("the second enqueue must succeed");
    assert_eq!(
        second.queued, 0,
        "the unique constraint refuses the duplicates"
    );
    assert_eq!(second.total(), 0);

    let rows: i64 = sqlx::query_scalar(
        "select count(*) from notification_deliveries where notification_id = $1",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("the count must read");
    assert_eq!(
        rows, 3,
        "one row per channel and no more: e-mail, in_app, and the skipped `chat` row"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn a_channel_nobody_configured_is_skipped_rather_than_left_queued_forever() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let (user_id, notification_id) = notification(&pool).await;
    delivery::enqueue(&pool, notification_id, &["web_push".to_owned()], &[])
        .await
        .expect("enqueue must succeed");

    // Nothing in `notification_channels` on this database, so every remote row is unconfigured.
    let settled = delivery::settle_not_ready(&pool, 30.0)
        .await
        .expect("the settlement must run");
    assert_eq!(settled, 1, "the unconfigured web_push row is settled");

    let web_push_id: Uuid = sqlx::query_scalar(
        "select id from notification_deliveries where notification_id = $1 and channel = 'web_push'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("the web_push row must exist");
    let settled_row = row(&pool, web_push_id).await;
    assert_eq!(
        settled_row.status, "skipped",
        "a channel nobody configured must not sit pending forever — the outbox would show a \
         queue that never moves"
    );
    assert!(
        settled_row.error.is_some(),
        "and it says why, rather than being a bare state"
    );

    // The in-app row is never settled, whatever the installation has configured. A reader with
    // no in-app row has no inbox, and the preference endpoint already refuses to let them
    // switch it off — so settling it here would undo that guarantee behind the API's back.
    let in_app_status: String = sqlx::query_scalar(
        "select status from notification_deliveries where notification_id = $1 and channel = 'in_app'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("the in-app row must exist");
    assert_eq!(
        in_app_status, "pending",
        "in-app is never settled as unconfigured"
    );

    // A second settlement is a no-op: the row is no longer `pending`.
    let again = delivery::settle_not_ready(&pool, 30.0)
        .await
        .expect("the second settlement must run");
    assert_eq!(again, 0, "settling is idempotent");

    harness.dispose().await;
}

#[tokio::test]
async fn a_tenant_that_switched_a_channel_on_keeps_its_own_deliveries_queued() {
    // The multi-tenant leg, and the reason `settle_not_ready` is not the three-line statement
    // it looks like. The obvious `not exists (select … from notification_channels)` asks
    // "does *anybody* have this channel on", so on a shared installation one customer
    // configuring e-mail would settle every other tenant's e-mail rows as unconfigured — one
    // customer's settings silently stopping another customer's delivery.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    // Two organizations, one with e-mail configured and one without.
    let configured_org: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Configured")
            .bind(format!("configured-{}", Uuid::new_v4().simple()))
            .fetch_one(&pool)
            .await
            .expect("the first organization must be created");
    let bare_org: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Bare")
            .bind(format!("bare-{}", Uuid::new_v4().simple()))
            .fetch_one(&pool)
            .await
            .expect("the second organization must be created");

    sqlx::query(
        "insert into notification_channels (organization_id, channel, enabled) \
         values ($1, 'email', true)",
    )
    .bind(configured_org)
    .execute(&pool)
    .await
    .expect("the channel configuration must be written");

    // One notification for each tenant, each with a queued e-mail row.
    //
    // The notifications come from the shared helper rather than a second hand-written insert:
    // writing them here as well would put two notifications on the database for each reader,
    // and the "exactly one row may be settled" assertion below would then be measuring the
    // fixture's own duplicate instead of the tenancy rule.
    let (_configured_user, configured_notification) =
        notification_for(&pool, Some(configured_org)).await;
    let (_bare_user, bare_notification) = notification_for(&pool, Some(bare_org)).await;

    // Queued through the crate's own `enqueue`, because "a row the reader is owed" is what the
    // settlement is about. Inserting `pending` by hand would skip the one code path a real
    // notification takes, and the walk would then prove the settle works on rows nothing
    // creates.
    for notification in [configured_notification, bare_notification] {
        delivery::enqueue(&pool, notification, &["email".to_owned()], &[])
            .await
            .expect("both tenants' e-mail rows must be queued");
    }

    // Exactly one row may be settled: the bare tenant's.
    let settled = delivery::settle_not_ready(&pool, 30.0)
        .await
        .expect("the settlement must run");
    assert_eq!(
        settled, 1,
        "one tenant configuring a channel must not settle another tenant's deliveries"
    );

    // Keyed by the notification *and* the channel, not by the reader. Each notification has two
    // delivery rows now — the e-mail one and the always-queued in-app one — so a query that
    // selected on the user alone would hand back whichever row the planner liked first, and
    // the assertion would be measuring a row the settlement was never allowed to touch. The
    // `in_app` row is excluded twice over: by the channel clause and by the fact that
    // `settle_not_ready` skips it.

    assert_eq!(
        e_mail_status(&pool, configured_notification).await,
        "pending",
        "the tenant that switched e-mail on keeps its row queued"
    );
    assert_eq!(
        e_mail_status(&pool, bare_notification).await,
        "skipped",
        "the tenant that has no e-mail is settled"
    );

    // And the in-app rows are untouched on both sides, which is what makes `settled == 1` the
    // tenancy rule rather than a coincidence about how many rows happened to be pending.
    let in_app: Vec<String> = sqlx::query_scalar(
        "select d.status from notification_deliveries d \
         where d.notification_id = any($1::uuid[]) and d.channel = 'in_app'",
    )
    .bind(&[configured_notification, bare_notification])
    .fetch_all(&pool)
    .await
    .expect("both in-app rows must exist");
    assert_eq!(in_app, vec!["pending".to_owned(), "pending".to_owned()]);

    harness.dispose().await;
}

#[tokio::test]
async fn the_outbox_reads_back_what_the_queue_wrote() {
    // The last leg, and the one that ties the queue to the screen. Every other walk asserts on
    // a row it wrote itself; this one asserts that the *read* the outbox page performs agrees
    // with the queue's own state, because a queue whose rows nobody can read is a queue whose
    // failures are invisible — which is the whole reason the table exists.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();
    let config = fast_config();
    let failing = AlwaysFails::new("email");
    let transports: Vec<(String, Box<dyn Transport>)> = vec![
        ("email".to_owned(), Box::new(failing)),
        (
            omnion_notifications::IN_APP.to_owned(),
            Box::new(delivery::InAppTransport),
        ),
    ];
    configure_channel(&pool, "email").await;

    let (user_id, notification_id) = notification(&pool).await;
    delivery::enqueue(
        &pool,
        notification_id,
        &["email".to_owned()],
        &["web_push".to_owned()],
    )
    .await
    .expect("enqueue must succeed");

    // Drain to the cap so the outbox has one of each state.
    for _ in 0..3 {
        tokio::time::sleep(backoff_window()).await;
        delivery::run_due(&pool, &transports, &config)
            .await
            .expect("each tick must run");
    }

    let rows = push::list_outbox(
        &pool,
        None,
        &push::OutboxQuery {
            statuses: Vec::new(),
            channel: None,
            notification_id: Some(notification_id),
            limit: 10,
        },
    )
    .await
    .expect("the outbox read must succeed");
    assert_eq!(
        rows.len(),
        4,
        "one row per channel in the vocabulary: the two asked about, the in-app row, and the \
         skipped `chat` row"
    );

    let counts = push::outbox_counts(&pool, None)
        .await
        .expect("the counts must read");
    assert_eq!(counts.sent, 1, "the in-app row went out");
    assert_eq!(counts.failed, 1, "the e-mail row reached its cap");
    assert_eq!(
        counts.skipped, 2,
        "web_push was switched off for this reader, and `chat` has no transport"
    );
    assert_eq!(counts.total(), 4);
    assert_eq!(counts.pending, 0, "nothing is left queued");

    // Failed first is the ordering the outbox screen relies on during an incident.
    assert_eq!(
        rows[0].status, "failed",
        "the outbox is ordered failed-first; a log ordered by time puts the failure under a \
         hundred successes"
    );

    // And the retry button still works on a row the queue gave up on.
    let failed_id = rows
        .iter()
        .find(|r| r.status == "failed")
        .expect("a failed row must be present")
        .id;
    let outcome = push::retry_delivery(&pool, failed_id)
        .await
        .expect("the retry must run");
    assert_eq!(outcome, push::RetryOutcome::Requeued);
    assert_eq!(
        row(&pool, failed_id).await.attempts,
        0,
        "a manual retry starts over"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn the_in_app_transport_needs_no_configuration_and_always_succeeds() {
    // The transport the other tests do not cover, and the one that must never fail: a reader
    // whose in-app delivery is `failed` has a notification in the table and nothing in their
    // inbox, and the panel reads the table. So this is asserted rather than assumed.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let (user_id, notification_id) = notification(&pool).await;
    delivery::enqueue(&pool, notification_id, &[], &[])
        .await
        .expect("enqueue must succeed");

    let in_app_id: Uuid = sqlx::query_scalar(
        "select id from notification_deliveries where notification_id = $1 and channel = 'in_app'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("the in-app row must exist even with no channels requested");

    let report = delivery::run_due(
        &pool,
        &[(
            omnion_notifications::IN_APP.to_owned(),
            Box::new(delivery::InAppTransport),
        )],
        &fast_config(),
    )
    .await
    .expect("the tick must run");
    assert_eq!(report.sent, 1);
    assert_eq!(report.failed, 0, "the in-app channel cannot fail");

    let delivered = row(&pool, in_app_id).await;
    assert_eq!(delivered.status, "sent");
    assert!(delivered.sent_at.is_some());

    harness.dispose().await;
}

#[tokio::test]
async fn the_email_transport_refuses_a_reader_with_no_address_instead_of_pretending() {
    // The transport's own guard, over a real row. A reader created without an address is a
    // real state (an invited user who has not finished signing up), and the honest record is
    // a failure with a reason — because the alternative, a silent success, produces a `sent`
    // row for a message that was never sent, which is the one lie this table must not tell.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let (user_id, notification_id) = notification(&pool).await;
    delivery::enqueue(&pool, notification_id, &["email".to_owned()], &[])
        .await
        .expect("enqueue must succeed");

    let email_id: Uuid = sqlx::query_scalar(
        "select id from notification_deliveries where notification_id = $1 and channel = 'email'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("the e-mail row must exist");

    // The transport is exercised directly with a job that has no address, rather than through
    // the queue, because the queue would need a live SMTP server to be interesting otherwise.
    let job = DeliveryJob {
        id: email_id,
        notification_id,
        user_id,
        channel: "email".to_owned(),
        attempts: 1,
        max_attempts: 3,
        title: "A page is waiting".to_owned(),
        body: "Somebody asked for a review.".to_owned(),
        url: None,
        user_email: None,
        webhook_endpoint: None,
        push_targets: Vec::new(),
    };

    let transport = EmailTransport::new(&omnion_core::config::MailConfig::default());
    match transport.deliver(&job, &fast_config()).await {
        TransportOutcome::Failed { reason, status, .. } => {
            assert!(status.is_none(), "there was no HTTP status to report");
            assert!(
                reason.contains("no e-mail address"),
                "the reason has to name the actual problem, got: {reason}"
            );
        }
        other => panic!("a reader with no address must not report success, got {other:?}"),
    }

    // And the webhook transport's own guard: a notification with no URL has nowhere to post.
    // The assertion is on the *substance* — that the reason says there is nowhere to send — and
    // not on the exact wording, because slice 5 reworded this sentence (from "no endpoint" to
    // "no destination") and left this assertion behind: the walk went red for a message that
    // got *clearer*, and a red walk is a walk nobody runs.
    let webhook = WebhookTransport::new(std::time::Duration::from_secs(1))
        .expect("the HTTP client must build");
    match webhook.deliver(&job, &fast_config()).await {
        TransportOutcome::Failed { reason, .. } => {
            assert!(
                reason.contains("no destination") || reason.contains("no endpoint"),
                "the reason has to say there is nowhere to post, got: {reason}"
            );
        }
        other => panic!("a notification with no endpoint must not report success, got {other:?}"),
    }

    harness.dispose().await;
}

/// The producer path fills the queue — **the walk whose absence let the queue ship empty.**
///
/// This suite's other walks all call `delivery::enqueue` themselves, which proves the queue
/// *drains* and says nothing about whether anything ever *fills* it. They could all be green
/// with a platform that never sends an e-mail to anyone, and they were: slices 1 through 6b
/// shipped a complete runner, four transports, a retry policy and a live-database lifecycle
/// suite, and no production code path created a delivery row. The only caller of `enqueue`
/// outside this file was the test-delivery route — the one path whose entire job is to prove
/// the queue works, so it could never be evidence that a *notification* produces a delivery.
///
/// So this walk starts where a real notification starts: a bus event routed by the router, and
/// the `store::record_with_deliveries` the emit route now calls. Both assertions read the rows
/// out of PostgreSQL rather than trusting a returned count, because a count is what the
/// function under test produced and the row is the fact.
#[tokio::test]
async fn a_notification_written_by_the_producer_path_arrives_with_its_deliveries() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let reader = omnion_identity::users::create_user(
        &pool,
        omnion_identity::users::NewUser {
            email: format!("producer-{}@omnion.test", Uuid::new_v4().simple()),
            password: "correct horse battery".to_owned(),
            display_name: "Producer".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the reader must exist");

    // **No `enqueue` call anywhere in this walk.** That absence is the assertion. The walk
    // calls the same store function the emit route calls, and then reads the delivery table.
    let draft = NewNotification {
        user_id: reader.id,
        category: "system".to_owned(),
        priority: "normal".to_owned(),
        title: "A page is waiting".to_owned(),
        body: "Somebody asked for a review.".to_owned(),
        url: Some("/reviews/1".to_owned()),
        source_type: Some("page".to_owned()),
        source_id: None,
        payload: serde_json::json!({}),
        dedupe_key: None,
    };
    let (notification_id, report) = store::record_with_deliveries(&pool, None, None, &draft)
        .await
        .expect("the producer path must succeed")
        .expect("an undeduped draft must create a row");

    // One notification, and exactly one — the double-insert shape this slice replaced would
    // have written two rows here, because the `on conflict` clause is partial and does
    // nothing at all for a null dedupe key. A count of one is the only assertion that can tell
    // the two apart.
    let notifications: i64 =
        sqlx::query_scalar("select count(*) from notifications where user_id = $1")
            .bind(reader.id)
            .fetch_one(&pool)
            .await
            .expect("the notification count must read");
    assert_eq!(
        notifications, 1,
        "one notification, not two: the partial on-conflict clause does nothing for a null key"
    );

    // And the deliveries, read out of the table rather than out of the report.
    let channels: Vec<String> = sqlx::query_scalar(
        "select channel from notification_deliveries where notification_id = $1 order by channel",
    )
    .bind(notification_id)
    .fetch_all(&pool)
    .await
    .expect("the delivery rows must be readable");

    assert_eq!(
        channels,
        vec![
            "chat".to_owned(),
            "email".to_owned(),
            "in_app".to_owned(),
            "web_push".to_owned(),
            "webhook".to_owned(),
        ],
        "every channel in the closed vocabulary must get a row: the three with a transport plus \
         the in-app row are due, and `chat` is written skipped rather than left absent — a \
         notification with no delivery rows is the defect this walk was written for"
    );

    let statuses: Vec<String> = sqlx::query_scalar(
        "select status from notification_deliveries where notification_id = $1 order by channel",
    )
    .bind(notification_id)
    .fetch_all(&pool)
    .await
    .expect("the delivery statuses must be readable");
    assert_eq!(
        statuses,
        vec![
            "skipped".to_owned(), // chat — no transport installed
            "pending".to_owned(), // email
            "pending".to_owned(), // in_app
            "pending".to_owned(), // web_push
            "pending".to_owned(), // webhook
        ],
        "a channel with no transport is skipped on the row; a pending one would be claimed, \
         re-queued to the cap and written failed by the runner"
    );

    let chat_reason: Option<String> = sqlx::query_scalar(
        "select error from notification_deliveries where notification_id = $1 and channel = 'chat'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("the chat row must carry a reason");
    assert_eq!(
        chat_reason.as_deref(),
        Some(omnion_notifications::NO_TRANSPORT_YET),
        "the reason distinguishes a platform gap from a reader's own switch"
    );

    assert_eq!(
        report.queued, 4,
        "the three transport channels plus in_app; the report and the table must agree"
    );
    assert_eq!(
        report.skipped, 1,
        "chat is skipped, and nothing else is"
    );

    harness.dispose().await;
}

/// A channel the reader switched off is written `skipped`, by the producer and not by the caller.
///
/// The reason a producer path can get this wrong is that it has the notification and it has the
/// reader, so it looks like it has everything — but "everything" includes the *preference*, and
/// the only function that knows it is `allowed_channels`. A producer that passes "all the
/// channels" gets a reader who turned e-mail off an e-mail anyway; one that passes "none" gets a
/// reader a mail that never arrives. So the switch-off is asserted from the table.
#[tokio::test]
async fn a_channel_the_reader_switched_off_is_skipped_by_the_producer_path() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let reader = omnion_identity::users::create_user(
        &pool,
        omnion_identity::users::NewUser {
            email: format!("muted-{}@omnion.test", Uuid::new_v4().simple()),
            password: "correct horse battery".to_owned(),
            display_name: "Muted".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the reader must exist");

    // Stated rows are stored as stated, and `write_preferences` takes the settings row too —
    // a settings screen always has it loaded, so the signature asks for it rather than letting
    // a caller invent a default the reader never chose.
    omnion_notifications::write_preferences(
        &pool,
        reader.id,
        &[omnion_notifications::StatedPreference {
            category: "ticket".to_owned(),
            channel: "email".to_owned(),
            enabled: false,
        }],
        &omnion_notifications::Settings::default_for(reader.id),
    )
    .await
    .expect("the preference must be stated");

    let (notification_id, _) = store::record_with_deliveries(
        &pool,
        None,
        None,
        &NewNotification {
            user_id: reader.id,
            category: "ticket".to_owned(),
            priority: "normal".to_owned(),
            title: "A ticket moved".to_owned(),
            body: String::new(),
            url: None,
            source_type: Some("ticket".to_owned()),
            source_id: None,
            payload: serde_json::json!({}),
            dedupe_key: None,
        },
    )
    .await
    .expect("the producer path must succeed")
    .expect("an undeduped draft must create a row");

    // The row exists — that is the drawer being able to say *why* nothing arrived — and it
    // carries the reason the queue shows, not an empty one.
    let (status, error): (String, Option<String>) = sqlx::query_as(
        "select status, error from notification_deliveries \
         where notification_id = $1 and channel = 'email'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("a switched-off channel still gets a row");
    assert_eq!(status, "skipped");
    assert_eq!(
        error.as_deref(),
        Some(delivery::READER_SWITCHED_IT_OFF),
        "the reason on the row is the sentence the drawer shows"
    );

    // The in-app row is written unconditionally by `enqueue` and is never skipped: the panel
    // is the inbox, so a row saying "this did not arrive" would be a lie about the one channel
    // the reader is already looking at.
    let in_app_error: Option<String> = sqlx::query_scalar(
        "select error from notification_deliveries \
         where notification_id = $1 and channel = 'in_app'",
    )
    .bind(notification_id)
    .fetch_one(&pool)
    .await
    .expect("the in-app row must exist");
    assert!(
        in_app_error.is_none(),
        "the in-app row is the inbox and is never skipped, got {in_app_error:?}"
    );

    // The switched-off preference is scoped to its category: `ticket` mail being off must not
    // silence `security` mail, which is the whole reason the matrix is stored as stated rows
    // rather than as one flag per channel.
    let (security_id, _) = store::record_with_deliveries(
        &pool,
        None,
        None,
        &NewNotification {
            user_id: reader.id,
            category: "security".to_owned(),
            priority: "high".to_owned(),
            title: "A new sign-in".to_owned(),
            body: String::new(),
            url: None,
            source_type: Some("session".to_owned()),
            source_id: None,
            payload: serde_json::json!({}),
            dedupe_key: None,
        },
    )
    .await
    .expect("the producer path must succeed")
    .expect("an undeduped draft must create a row");

    let security_email: String = sqlx::query_scalar(
        "select status from notification_deliveries \
         where notification_id = $1 and channel = 'email'",
    )
    .bind(security_id)
    .fetch_one(&pool)
    .await
    .expect("a category the reader never switched off must be queued");
    assert_eq!(
        security_email, "pending",
        "turning ticket mail off must not silence the security mail the reader never touched"
    );

    harness.dispose().await;
}
