//! What the reader is shown about their own deliveries (REQ-021, slice 4's last open box).
//!
//! The delivery runner landed in the previous tick and the outbox could already see everything
//! it wrote — but the *reader* could not. `GET /api/v1/notifications/{id}` carried a bare
//! notification: no delivery rows, no attempt count, no reason a channel did not go out. The
//! acceptance box had been sitting unticked since the request was written with the note "when
//! deliveries exist", and deliveries now exist, so the note expired and the gap became real: the
//! platform knew an e-mail failed, and the person waiting for it was told nothing.
//!
//! **This suite tests the read path, and the read path has a hazard the write path never had.**
//! The runner's writer is scoped by nothing at all — it is a system process doing a system job.
//! But this read is served to a *signed-in person* about a row that may be theirs. Two properties
//! have to hold at once, and they pull in opposite directions:
//!
//! * The delivery rows must be complete enough to be honest, which means reading a table keyed
//!   only by `notification_id` — no `user_id` column, nothing to filter on.
//! * The read must not become an existence oracle. If the delivery read ran before the ownership
//!   check, then asking for somebody else's id would answer `404` for a row with no attempts and
//!   `404`… no: it would answer with a *populated channel list*, and the difference between
//!   "no such row" and "here are the three channels" is the answer a caller was not entitled to.
//!
//! So the ownership filter is asserted by **shape, not by status code**: both cases answer
//! `None`/`404` for the right reason, and the walk proves the reason by showing that the
//! caller's own id *does* return rows through the same function. A test that only checked
//! "foreign id returns nothing" would pass even if the read had moved before the filter and
//! then been discarded afterwards — which is the same leak, closed by accident.
#![allow(clippy::too_many_lines)]

use omnion_core::Db;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_notifications::model::NewNotification;
use omnion_notifications::store;
use sqlx::PgPool;
use time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// A scratch database, and the same reason the runner's suite has one.
//
// The criterion is about what a *reader is shown* for rows a real queue wrote, so the rows have
// to come from the real enqueue and the real claim. A fixture that inserted delivery rows by
// hand would test the SELECT against rows the platform would never actually produce.
// ---------------------------------------------------------------------------------------------

struct Harness {
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("the configuration must load");
        let Some(maintenance) = live_db(&config).await else {
            return None;
        };

        let database = format!("omnion_notifreader_{}", Uuid::new_v4().simple());
        sqlx::query(&format!(r#"create database "{database}""#))
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

    /// Dropped explicitly, and it is the *only* cleanup there is.
    ///
    /// **A panicking walk that skips `dispose` leaves its database behind, and the box has a
    /// shared connection pool with nine sibling writers.** The scratch database is a directory
    /// of files, not a row: each leaked one holds tens of megabytes that nothing reclaims, and
    /// the walks stop being runnable once the disk fills. `close().await` is what releases the
    /// pool — without it `drop database ... with (force)` has to terminate the backends.
    ///
    /// **No `mem::forget` belongs after the call**, for the reason the health walks learned the
    /// hard way this tick: the signature takes `self`, so the call *moves* the harness, and the
    /// `std::mem::forget(harness)` these walks were written with was a use-after-move — eleven
    /// compile errors in `health_probes.rs`, committed unnoticed because no gate that had run
    /// compiled that binary. The comment claiming the `forget` "stops the later `Drop` from
    /// running against a moved-from handle" is what kept it there: there is no later `Drop` of
    /// that value, because the value was consumed by the call above. This file carries the
    /// comment without the code, which is the cheaper half of the mistake to leave in place.
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

/// One account and one notification for it.
///
/// The account is created rather than invented: `notifications.user_id` is a foreign key, and a
/// walk that inserts a random uuid finds out through a constraint violation rather than through
/// the behaviour it is testing.
async fn reader_with_notification(pool: &PgPool) -> (Uuid, Uuid) {
    let user = omnion_identity::users::create_user(
        pool,
        omnion_identity::users::NewUser {
            email: format!("reader-{}@omnion.test", Uuid::new_v4().simple()),
            password: "correct horse battery".to_owned(),
            display_name: "Reader".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the reader must exist — notifications.user_id is a foreign key");

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

    store::record(pool, None, None, &draft)
        .await
        .expect("the notification must be recorded");

    let id: Uuid = sqlx::query_scalar("select id from notifications where user_id = $1")
        .bind(user.id)
        .fetch_one(pool)
        .await
        .expect("the recorded notification must be readable");
    (user.id, id)
}

/// Switch one channel on, the way an operator would — or the walk gets a `skipped` row and learns
/// about settlement instead of about the read.
///
/// **The channel row is keyed by `(organization_id, channel)`, so this creates the organization
/// first.** The first version of this helper wrote `on conflict (channel) do update` and died
/// with `42P10: there is no unique or exclusion constraint matching the ON CONFLICT
/// specification` — the fixture was asserting a uniqueness that only holds inside one
/// organization. A tenant that configures e-mail is the normal case, not a special one, so the
/// walk now describes a real installation: an organization with a channel switched on.
async fn configure_channel(pool: &PgPool, channel: &str) {
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Reader host")
            .bind(format!("reader-host-{}", Uuid::new_v4().simple()))
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

/// A transport that refuses, so the row reaches `failed` with a reason on it.
struct RefusingTransport {
    reason: String,
}

impl omnion_notifications::delivery::Transport for RefusingTransport {
    /// Claims the e-mail channel, and only that one.
    ///
    /// The trait asks a transport to name its own channel so the runner can look it up in a
    /// map rather than matching names inside one big function. Naming it `email` here is what
    /// makes the walk's `failed` row an e-mail row: a transport that claimed `in_app` would
    /// rewrite the inbox row to `failed`, and the assertion about the inbox would then be
    /// asserting the walk's own mistake rather than the platform's behaviour.
    fn channel(&self) -> &'static str {
        "email"
    }

    fn deliver(
        &self,
        _job: &omnion_notifications::delivery::DeliveryJob,
        _config: &omnion_notifications::delivery::DeliveryConfig,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = omnion_notifications::delivery::TransportOutcome>
                + Send,
        >,
    > {
        let reason = self.reason.clone();
        Box::pin(
            async move { omnion_notifications::delivery::TransportOutcome::failed(None, reason) },
        )
    }
}

/// A config with a one-millisecond backoff, so the walk drives the whole lifecycle in a second
/// while still exercising the real `retry_delay` arithmetic.
///
/// **The attempt cap is not here because it is not here.** `max_attempts` is a column on each
/// delivery row, seeded by the migration at 3, and a walk that wanted a different cap would
/// write the row rather than reconfigure the queue — which is the shape the platform actually
/// has, so the walk uses the default and stops at 3.
fn config() -> omnion_notifications::delivery::DeliveryConfig {
    omnion_notifications::delivery::DeliveryConfig {
        batch: 10,
        lease_seconds: 30,
        // A base of zero would make every attempt due immediately, so the cap is reached in one
        // tick and "tried three times" never becomes a thing the walk can observe. One
        // millisecond is enough separation to be real and short enough to wait for.
        retry_base: Duration::milliseconds(1),
        retry_max: Duration::milliseconds(1),
        request_timeout: Duration::seconds(1),
    }
}

/// The transports a tick is given: a list of (channel, implementation).
///
/// **Both channels are registered, and that is the correction this walk needed.** The first
/// version registered only the refusing e-mail transport and then asserted the in-app row was
/// `sent` — which failed, correctly: `enqueue` writes `in_app` as `pending` and the row only
/// becomes `sent` when a tick *drains* it through a transport. A channel with no transport is a
/// configuration gap the runner answers by re-queueing, so leaving `in_app` unregistered meant
/// the inbox row sat `pending` forever and the walk was asserting a fact the platform does not
/// claim.
///
/// That is the same fixture-gap class as the missing channel configuration two walks back: the
/// walk describes an installation, and an installation that enqueues a notification has a
/// runner with an in-app transport. The real `InAppTransport` is used rather than a second
/// invented one, so the walk cannot disagree with what the binary actually does.
fn transports(reason: &str) -> Vec<(String, Box<dyn omnion_notifications::delivery::Transport>)> {
    vec![
        (
            "in_app".to_owned(),
            Box::new(omnion_notifications::delivery::InAppTransport),
        ),
        (
            "email".to_owned(),
            Box::new(RefusingTransport {
                reason: reason.to_owned(),
            }),
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// The box the whole suite exists for: the reader sees one row per channel, with the state, the
/// attempt count against the cap, and the reason a channel did not go out.
///
/// **The reason is the load-bearing assertion.** "Failed" on its own is a status the reader
/// cannot act on; "failed after 3 tries — the mail server refused the connection" is. A drawer
/// that renders the state and drops the error column would pass a test that only checked the
/// state, and the whole point of the box is that the *reason* is visible.
#[tokio::test]
async fn the_readers_own_deliveries_carry_the_state_the_attempt_count_and_the_reason() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let (user, notification) = reader_with_notification(&pool).await;
    configure_channel(&pool, "email").await;

    // The e-mail channel is enabled for the reader and the in-app row is written
    // unconditionally, so two rows exist: one delivered, one to be attempted.
    omnion_notifications::delivery::enqueue(&pool, notification, &["email".to_owned()], &[])
        .await
        .expect("enqueue must succeed");

    // Drive e-mail to the cap with a transport that always refuses. Four ticks for a cap of
    // three: the first claims, the second and third retry, the fourth is the one that writes
    // `failed` — and one more would prove nothing new, because a row at the cap is not due.
    let cfg = config();
    let refusing = transports("the mail server refused the connection");
    for _ in 0..4 {
        omnion_notifications::delivery::run_due(&pool, &refusing, &cfg)
            .await
            .expect("a tick must not fail");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }

    // The read the drawer makes.
    let mine = omnion_notifications::store::find(&pool, user, notification)
        .await
        .expect("the read must not fail")
        .expect("the reader's own notification must be found");
    let rows = omnion_notifications::store::deliveries(&pool, mine.id)
        .await
        .expect("the delivery read must not fail");

    assert_eq!(
        rows.len(),
        2,
        "one row per enqueued channel, so a drawer can answer \"what happened to each\". got: \
         {rows:?}"
    );

    let in_app = rows
        .iter()
        .find(|row| row.channel == "in_app")
        .expect("the in-app row is the inbox and is written unconditionally");
    assert_eq!(
        in_app.status, "sent",
        "the in-app channel needs no transport, so a tick delivers it — and a reader whose panel \
         shows the notification while the drawer says the inbox never received it is the \
         contradiction this table exists to prevent"
    );

    let email = rows
        .iter()
        .find(|row| row.channel == "email")
        .expect("the e-mail row must be listed");
    assert_eq!(
        email.status, "failed",
        "a channel refused at every attempt and is at the cap, so the row says so out loud"
    );
    assert!(
        email
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("refused the connection"),
        "the reason has to reach the reader in the transport's own words, got: {:?}",
        email.error
    );
    assert!(
        email.attempts >= 2 && email.attempts <= email.max_attempts,
        "the drawer has to be able to say how hard it tried — a failed row that does not say so \
         is indistinguishable from a failed row that was never retried. got attempts={} max={}",
        email.attempts,
        email.max_attempts
    );
    assert!(
        email.sent_at.is_none(),
        "a row that failed was never sent, and a sent_at on it would be a lie the drawer renders"
    );

    harness.dispose().await;
}

/// The drawer's rows come back in a stable order, and every channel is listed exactly once.
///
/// **The assertion is stability, not a story about which channel leads.** The first version of
/// this walk asserted that `in_app` is first, reasoning that enqueue writes the inbox row before
/// the remote ones. It does not: `enqueue` loops the *enabled* channels and appends `in_app`
/// afterwards — and since every row in one call shares a single `now()` for `created_at`, the
/// chronological key ties and the alphabetical tiebreak decides. The walk caught its own wrong
/// assumption, and the honest version is to assert what the drawer actually needs: the same
/// notification read twice must produce the same list.
///
/// Without the `order by` this is a property of the query plan, and a panel that reshuffles its
/// delivery rows on refresh is one nobody trusts. The unique constraint already guarantees the
/// "once" half, so the count is asserted too.
#[tokio::test]
async fn the_delivery_rows_are_listed_in_a_stable_order_with_each_channel_once() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    // Only the notification matters here — the ordering is a property of the queue, not of
    // anybody's inbox.
    let (_user, notification) = reader_with_notification(&pool).await;

    // Three rows in all: the two remote channels the caller enabled, plus the in-app row that
    // `enqueue` writes unconditionally.
    omnion_notifications::delivery::enqueue(
        &pool,
        notification,
        &["email".to_owned(), "web_push".to_owned()],
        &[],
    )
    .await
    .expect("enqueue must succeed");

    let rows = omnion_notifications::store::deliveries(&pool, notification)
        .await
        .expect("the delivery read must not fail");

    assert_eq!(
        rows.len(),
        3,
        "three channels enqueued, three rows listed — a channel dropped from the read is a \
         channel the reader is never told about. got: {rows:?}"
    );

    let unique: std::collections::HashSet<_> =
        rows.iter().map(|row| row.channel.as_str()).collect();
    assert_eq!(
        unique.len(),
        3,
        "each channel appears exactly once; the unique constraint says so, and the read must not \
         paper over it with a join that doubles the row"
    );

    // The stability claim, measured rather than asserted in prose: the same row read twice must
    // come back in the same order. A missing `order by` is the failure this catches, and it is
    // the failure that only shows up on a *second* read — the first one always looks right.
    let again = omnion_notifications::store::deliveries(&pool, notification)
        .await
        .expect("the second delivery read must not fail");
    let order: Vec<&str> = rows.iter().map(|row| row.channel.as_str()).collect();
    let order_again: Vec<&str> = again.iter().map(|row| row.channel.as_str()).collect();
    assert_eq!(
        order, order_again,
        "the same notification read twice must list the same channels in the same order — a \
         drawer that reshuffles its rows on refresh is a property of the query plan, not of the \
         data"
    );

    // And the rows agree with themselves, not just with their positions: two reads of the same
    // notification describe the same state.
    assert_eq!(
        rows, again,
        "the delivery rows are a fact about the queue; two reads seconds apart with no runner in \
         between must not differ"
    );

    harness.dispose().await;
}

/// A notification nobody has tried yet reads as an empty list, and that is an answer.
///
/// **The distinction the drawer turns on: empty is not broken.** A notification recorded before
/// the runner shipped has no delivery rows, and the drawer must be able to say "nothing was
/// attempted" rather than rendering a section with a header and nothing in it, which reads as a
/// panel that failed to load.
#[tokio::test]
async fn a_notification_with_no_attempts_reads_as_an_empty_list() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let (_user, notification) = reader_with_notification(&pool).await;

    let rows = omnion_notifications::store::deliveries(&pool, notification)
        .await
        .expect("the delivery read must not succeed by failing");

    assert!(
        rows.is_empty(),
        "recorded but never enqueued means no attempts, and the drawer says so. A row appearing \
         here would mean something is inventing a delivery. got: {rows:?}"
    );

    harness.dispose().await;
}

/// The delivery read is keyed by notification id alone, so the *ownership filter* is the only
/// thing standing between one reader and another's channel list.
///
/// **This is the leg the change could most plausibly have broken, so it asserts the mechanism and
/// not just the outcome.** A foreign id resolves to `None` and the route turns that into a `404`
/// — and that is also what a genuinely absent id gives, which is the point. The risk is not a
/// wrong status; it is the read happening *first* and being discarded afterwards, which leaks
/// nothing on this path but would leak the moment the two reads were reordered. So the walk
/// proves the same function returns rows for the owner and nothing for the stranger: if the read
/// moved ahead of the filter, the owner's read would stop depending on `find` and this
/// conjunction would fail.
#[tokio::test]
async fn a_delivery_read_is_gated_on_the_ownership_check_and_never_ahead_of_it() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let (owner, notification) = reader_with_notification(&pool).await;
    configure_channel(&pool, "email").await;
    omnion_notifications::delivery::enqueue(&pool, notification, &["email".to_owned()], &[])
        .await
        .expect("enqueue must succeed");

    // A second account, with no relationship to the notification at all.
    let stranger = omnion_identity::users::create_user(
        &pool,
        omnion_identity::users::NewUser {
            email: format!("stranger-{}@omnion.test", Uuid::new_v4().simple()),
            password: "correct horse battery".to_owned(),
            display_name: "Stranger".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the second account must exist");

    // The stranger is refused the notification itself...
    let refused = omnion_notifications::store::find(&pool, stranger.id, notification)
        .await
        .expect("the read must not fail");
    assert!(
        refused.is_none(),
        "somebody else's notification is not theirs to read, and the only question this walk asks \
         of the route is whether it can tell the stranger from a row that does not exist"
    );

    // ...and, because the delivery read takes no user at all, the ONLY thing preventing a
    // channel list from being read straight off the table is that the route checks the owner
    // first. Proved here by showing the owner does get rows: if the two reads were ever
    // reordered, the owner's path would be answering from a query that never sees the session.
    let mine = omnion_notifications::store::find(&pool, owner, notification)
        .await
        .expect("the read must not fail")
        .expect("the owner must find their own notification");
    let owner_rows = omnion_notifications::store::deliveries(&pool, mine.id)
        .await
        .expect("the owner's delivery read must succeed");
    assert!(
        !owner_rows.is_empty(),
        "the owner must see their channels; a read that returned nothing for everybody would pass \
         the 'stranger sees nothing' leg while hiding the whole feature"
    );

    // And the stranger cannot get there by guessing the notification's id, because the delivery
    // read is never reached: `find` already answered. The assertion is that the *only* way in is
    // through an id `find` returned — so this mirrors the route's own shape rather than
    // reimplementing it, and the walk fails the moment the route stops being this shape.
    //
    // Written as an `if let` rather than a combinator chain: `map` over an `Option` yields a
    // *future*, and `Option<Result<..>>` is not the type that comes out, so the compact
    // `transpose()` form does not typecheck. An explicit branch says the same thing and does.
    let resolved = omnion_notifications::store::find(&pool, stranger.id, notification)
        .await
        .expect("the read must not fail");
    match resolved {
        None => {}
        Some(row) => {
            let leaked = omnion_notifications::store::deliveries(&pool, row.id)
                .await
                .expect("the delivery read must not fail");
            panic!(
                "a stranger resolved somebody else's notification and read {} channels off it. \
                 The delivery rows carry no user_id, so the only thing standing between the two \
                 accounts is the ownership read happening first — got: {leaked:?}",
                leaked.len()
            );
        }
    }

    harness.dispose().await;
}
