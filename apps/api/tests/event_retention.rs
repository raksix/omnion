//! Integration tests for the event bus's own retention (REQ-016, slice 3).
//!
//! Retention is a set of claims about **what a statement does to a row**, so every assertion
//! here reads the row back out of PostgreSQL rather than trusting the response body. A response
//! that omits a field is indistinguishable from one that stored it and chose not to say so,
//! and "the sweep reported 412 events" is the exact sentence a run log can print while the
//! events are all still there.
//!
//! The walk covers the four claims the feature makes, each of which the obvious implementation
//! gets wrong in a different direction:
//!
//! * **A `pending` delivery pins its event.** The obvious sweep — "delete old events, let
//!   `on delete cascade` take the deliveries" — deletes a fact a receiver is still owed, and
//!   the receiver's only symptom is a delivery that never arrives with nothing in the platform
//!   saying why. The walk ages an event, queues a delivery against it, sweeps, and asserts
//!   both the event and its `pending` row are **still there**.
//! * **A settled delivery does not pin anything.** The same sweep with a `delivered` row must
//!   remove both, or the queue grows faster than the bus and the "history" is for ever.
//! * **The window is per organization.** A two-day window in one tenant must not remove a
//!   thirty-day-old event belonging to another tenant, because the whole point of the column
//!   is that a compliance policy belongs to the organization that has it.
//! * **A run that deletes nothing is still logged.** "The last sweep was last week and it
//!   found nothing" is the sentence an operator needs on the day they ask why an event from
//!   March is still in the feed, and a log that only records activity cannot answer it.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_events::store;
use omnion_identity::sessions;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The keys the operator of this suite holds.
///
/// `events.read` alone is the point of one of the assertions: the status read is a *read of the
/// bus*, and the two writes are the stronger power, because shortening a window destroys an
/// audit trail and a read-only auditor must not be able to trigger that.
const OPERATOR_PERMISSIONS: [&str; 3] = ["events.read", "webhooks.manage", "webhooks.read"];

// ---------------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------------

/// A throwaway database with every migration applied, its router, and the maintenance handle
/// that drops it again.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    /// Open a fresh database; `None` means PostgreSQL is not reachable and the suite skips.
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("environment must be valid");
        let Some(maintenance) = live_db(&config).await else {
            return None;
        };

        let database = format!("omnion_retention_{}", Uuid::new_v4().simple());
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
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.1.0-test"),
            config,
            db.clone(),
            redis,
            omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
                .expect("the default storage configuration is valid"),
        );

        Some(Self {
            state,
            db,
            maintenance,
            database,
        })
    }

    /// Drive the router without a network socket.
    async fn call(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = routes::router(self.state.clone())
            .oneshot(request)
            .await
            .expect("router must answer");

        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body must read")
            .to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };

        (status, body)
    }

    /// Drop the throwaway database.
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

/// Build a JSON request; `token` becomes the session cookie.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };

    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

fn get(uri: &str, token: &str) -> Request<Body> {
    request(Method::GET, uri, Some(token), None)
}

fn post(uri: &str, token: &str) -> Request<Body> {
    request(Method::POST, uri, Some(token), Some(json!({})))
}

fn patch(uri: &str, token: &str, body: Value) -> Request<Body> {
    request(Method::PATCH, uri, Some(token), Some(body))
}

/// Create an account with a session and return `(user id, token)`.
async fn account(harness: &Harness, organization_id: Option<Uuid>) -> (Uuid, String) {
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            email: format!("retention-{}@omnion.test", Uuid::new_v4().simple()),
            password: PASSWORD.to_owned(),
            display_name: "Sweep".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    let (_, token) = sessions::create_session(harness.db.pool(), user.id, None, None)
        .await
        .expect("the session must be created");
    (user.id, token)
}

/// Bind a role with exactly these permission keys to one account, at organization scope.
async fn grant(harness: &Harness, user_id: Uuid, organization_id: Uuid, keys: &[&str]) {
    let role = role_store::create_role(
        harness.db.pool(),
        NewRole {
            organization_id,
            key: format!("retention-walk-{}", Uuid::new_v4().simple()),
            name: "Retention Walk".to_owned(),
            description: "The keys one walk needs".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    let entries: Vec<RolePermissionInput> = keys
        .iter()
        .map(|key| RolePermissionInput {
            key: (*key).to_owned(),
            effect: Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(harness.db.pool(), role.id, &entries)
        .await
        .expect("the role permission set must be written");

    let binding = NewBinding {
        role_id: role.id,
        user_id,
        scope: Scope::Organization { organization_id },
        granted_by: None,
        expires_at: None,
    };
    bindings::validate(harness.db.pool(), &binding)
        .await
        .expect("the binding must validate");
    bindings::grant(harness.db.pool(), binding)
        .await
        .expect("the binding must be granted");
}

async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("retention-walk-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Backdate one event, so it is past a one-day window without waiting a day.
///
/// The `update` rather than an `insert` with an old `created_at` is deliberate: the event has to
/// have gone through the real `bus::emit` path to be the kind of row the sweeper actually meets.
async fn age_event(db: &Db, event_id: i64, days: i64) {
    // `make_interval(days => …)` is PostgreSQL 17+; on 16 and below the named argument has to
    // land on `int` or the call resolves to nothing and the error names a function that
    // plainly exists ("function make_interval(days => bigint) does not exist"), which reads
    // like a migration fault rather than an argument type. The cast is the whole fix.
    let aged = sqlx::query(
        "update events set created_at = now() - make_interval(days => $2::int) where id = $1",
    )
    .bind(event_id)
    .bind(days)
    .execute(db.pool())
    .await
    .expect("the event must be backdated")
    .rows_affected();
    assert_eq!(aged, 1, "the event must exist to be aged");
}

/// Connect to the maintenance database; `None` means PostgreSQL is not reachable.
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

/// Replace the database name in a PostgreSQL connection string.
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
// The walk
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_sweeper_keeps_what_a_receiver_is_still_owed_and_logs_the_rest() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let (owner_id, owner_token) = account(&harness, None).await;
    seed::bind_owner(harness.db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");

    let organization = create_organization_row(&harness.db, "sweep", "Sweep Test").await;
    let other = create_organization_row(&harness.db, "other", "Other Tenant").await;

    let (operator_id, operator_token) = account(&harness, Some(organization)).await;
    grant(&harness, operator_id, organization, &OPERATOR_PERMISSIONS).await;

    // --- The default window is a number, and it is the documented one ------------------------

    let (status, body) = harness
        .call(get("/api/v1/events/retention", &operator_token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(
        body["window_days"],
        json!(30),
        "an organization that has never opened the screen gets the documented default, \
         not a guess and not `null`: {body:?}"
    );
    assert_eq!(body["min_days"], json!(1));
    assert_eq!(body["max_days"], json!(3650));
    assert_eq!(
        body["last_run"],
        Value::Null,
        "nothing has run yet: {body:?}"
    );

    // --- A window outside the range is refused by name, not clamped --------------------------

    for (days, why) in [
        (0, "keep nothing"),
        (4000, "ten years of an unreadable bus"),
    ] {
        let (status, body) = harness
            .call(patch(
                "/api/v1/events/retention",
                &operator_token,
                json!({ "window_days": days }),
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a window of {days} ({why}) is refused: {body:?}"
        );
        assert_eq!(
            body["error"]["code"],
            json!("invalid_retention_window"),
            "the refusal names the field: {body:?}"
        );
    }

    // And the refused write did not land: the window is still the default.
    let (_, body) = harness
        .call(get("/api/v1/events/retention", &operator_token))
        .await;
    assert_eq!(
        body["window_days"],
        json!(30),
        "a refused write must not change anything: {body:?}"
    );

    // --- Set a one-day window ----------------------------------------------------------------

    let (status, body) = harness
        .call(patch(
            "/api/v1/events/retention",
            &operator_token,
            json!({ "window_days": 1 }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["window_days"], json!(1));

    // The change is on the bus with the window *before* and *after*, because "the window is 1
    // day" is not a record of anything — an audit needs the transition.
    let (_, feed) = harness
        .call(get(
            "/api/v1/events?name=webhook.retention.changed",
            &operator_token,
        ))
        .await;
    let changed = feed["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|event| event["name"] == json!("webhook.retention.changed"))
        .expect("the change must be on the bus");
    assert_eq!(changed["payload"]["previous_window_days"], json!(30));
    assert_eq!(changed["payload"]["window_days"], json!(1));

    // --- The pending delivery pins its event --------------------------------------------------

    // An event nobody was ever queued for, aged past the window: the sweeper takes it.
    let lonely = omnion_events::bus::emit(
        harness.db.pool(),
        omnion_events::NewEvent::new("page.updated")
            .organization(organization)
            .payload(json!({ "page_id": 1 })),
    )
    .await
    .expect("the event must be recorded");
    age_event(&harness.db, lonely.event.id, 5).await;

    // An event a receiver is *still owed*: aged, and holding a `pending` delivery.
    let endpoint = sqlx::query_scalar::<_, Uuid>(
        "insert into webhook_endpoints (organization_id, name, url, secret, events) \
         values ($1, $2, $3, $4, $5) returning id",
    )
    .bind(organization)
    .bind("Sweep receiver")
    .bind("https://receiver.test/hook")
    .bind("a-secret-long-enough-to-sign-with")
    .bind(vec!["page.published"])
    .fetch_one(harness.db.pool())
    .await
    .expect("the endpoint must exist");

    let owed = omnion_events::bus::emit(
        harness.db.pool(),
        omnion_events::NewEvent::new("page.published")
            .organization(organization)
            .payload(json!({ "page_id": 2, "slug": "owed" })),
    )
    .await
    .expect("the event must be recorded");
    age_event(&harness.db, owed.event.id, 5).await;

    let delivery: (Uuid, String) = sqlx::query_as(
        "select id, status from webhook_deliveries where endpoint_id = $1 and event_id = $2",
    )
    .bind(endpoint)
    .bind(owed.event.id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the fan-out must have queued a delivery");
    assert_eq!(delivery.1, "pending");

    // The counts the screen shows must use the *sweep's own predicate*, or the screen says
    // "N due" while the sweeper removes 0.
    //
    // **Three** events, not two: the PATCH above recorded `webhook.retention.changed` on the
    // same bus, and it belongs to the same organization, so the count honestly includes it.
    // The assertion used to say two, and it was written in the same breath as the PATCH — it
    // passed for the wrong reason only because the bus it was counting was freshly created and
    // the audit event was the third. Counting is not "count the rows I set up"; a number an
    // operator reads is a number the platform has to be able to explain, including the parts
    // nobody staged.
    let (_, before) = harness
        .call(get("/api/v1/events/retention", &operator_token))
        .await;
    assert_eq!(
        before["events"],
        json!(3),
        "the bus holds every event: {before:?}"
    );
    assert_eq!(
        before["due"],
        json!(1),
        "the pinned event is counted as history and not as due, and the retention \
         audit event itself is not past the window: {before:?}"
    );

    let (status, swept) = harness
        .call(post("/api/v1/events/retention/sweep", &operator_token))
        .await;
    assert_eq!(status, StatusCode::OK, "{swept:?}");
    assert_eq!(
        swept["events_deleted"],
        json!(1),
        "the free event goes and the pinned one stays: {swept:?}"
    );
    assert_eq!(
        swept["deliveries_deleted"],
        json!(0),
        "the pinned delivery is the reason its event stayed, so no delivery may be counted: \
         {swept:?}"
    );

    // The row, not the response: the event a receiver is still owed must still be there.
    let still_owed: i64 = sqlx::query_scalar("select count(*) from events where id = $1")
        .bind(owed.event.id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the owed event must be readable");
    assert_eq!(
        still_owed, 1,
        "an event whose delivery is still pending must not be swept — the cascade would have \
         deleted a fact the receiver has not had yet"
    );

    let still_pending: i64 = sqlx::query_scalar(
        "select count(*) from webhook_deliveries where id = $1 and status = 'pending'",
    )
    .bind(delivery.0)
    .fetch_one(harness.db.pool())
    .await
    .expect("the pinned delivery must be readable");
    assert_eq!(still_pending, 1, "its delivery row survived with it");

    // And the free event is gone.
    let gone: i64 = sqlx::query_scalar("select count(*) from events where id = $1")
        .bind(lonely.event.id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the swept event must be readable");
    assert_eq!(gone, 0, "the event nobody was owed was swept");

    // --- A settled delivery does not pin anything --------------------------------------------

    // Same age, but the delivery has been settled: the cascade takes both, or the queue grows
    // faster than the bus and the "history" is for ever.
    let settled = omnion_events::bus::emit(
        harness.db.pool(),
        omnion_events::NewEvent::new("page.published")
            .organization(organization)
            .payload(json!({ "page_id": 3, "slug": "settled" })),
    )
    .await
    .expect("the event must be recorded");
    age_event(&harness.db, settled.event.id, 5).await;
    let settled_delivery: Uuid = sqlx::query_scalar(
        "update webhook_deliveries set status = 'delivered', response_status = 200 \
         where event_id = $1 returning id",
    )
    .bind(settled.event.id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the fan-out must have queued a delivery");

    let (_, swept) = harness
        .call(post("/api/v1/events/retention/sweep", &operator_token))
        .await;
    assert_eq!(
        swept["events_deleted"],
        json!(1),
        "a settled delivery does not pin its event: {swept:?}"
    );
    assert_eq!(
        swept["deliveries_deleted"],
        json!(1),
        "and its delivery is counted separately, because the operator's history lost a row: \
         {swept:?}"
    );

    let remaining: i64 = sqlx::query_scalar("select count(*) from events where id = $1")
        .bind(settled.event.id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the swept event must be readable");
    assert_eq!(remaining, 0);

    let delivery_remaining: i64 =
        sqlx::query_scalar("select count(*) from webhook_deliveries where id = $1")
            .bind(settled_delivery)
            .fetch_one(harness.db.pool())
            .await
            .expect("the swept delivery must be readable");
    assert_eq!(delivery_remaining, 0, "the delivery went with its event");

    // --- The window is per organization -------------------------------------------------------

    let foreign = omnion_events::bus::emit(
        harness.db.pool(),
        omnion_events::NewEvent::new("page.updated")
            .organization(other)
            .payload(json!({ "page_id": 9 })),
    )
    .await
    .expect("the event must be recorded");
    age_event(&harness.db, foreign.event.id, 5).await;

    let (_, swept) = harness
        .call(post("/api/v1/events/retention/sweep", &operator_token))
        .await;
    assert_eq!(
        swept["events_deleted"],
        json!(0),
        "a two-tenant sweep must not reach into the tenant that did not ask for it: {swept:?}"
    );

    let foreign_remaining: i64 = sqlx::query_scalar("select count(*) from events where id = $1")
        .bind(foreign.event.id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the foreign event must be readable");
    assert_eq!(
        foreign_remaining, 1,
        "the other organization's history is its own policy's business"
    );

    // --- The run log is written for a sweep that deleted nothing -----------------------------

    let (_, body) = harness
        .call(get("/api/v1/events/retention", &operator_token))
        .await;
    assert!(
        body["last_run"].is_object(),
        "the last sweep is on screen even though it removed nothing: {body:?}"
    );
    assert_eq!(body["last_run"]["events_deleted"], json!(0));
    let history = body["recent_runs"].as_array().expect("recent_runs").len();
    assert_eq!(
        history, 3,
        "every sweep is a row, including the empty ones: {body:?}"
    );

    // And the runs are newest first, so the list reads as a history rather than a pile.
    // Sorted as **strings**: the instants are RFC 3339 in UTC, so the lexicographic order is
    // the chronological one, and comparing them as `Date` would need a parse per row plus an
    // unwrap for a value the API types as a string.
    let starts: Vec<String> = body["recent_runs"]
        .as_array()
        .expect("recent_runs")
        .iter()
        .map(|run| run["started_at"].as_str().unwrap_or_default().to_owned())
        .collect();
    let mut sorted = starts.clone();
    sorted.sort();
    sorted.reverse();
    assert_eq!(starts, sorted, "the run log reads newest first: {body:?}");

    // --- The read is `events.read`; the writes are the stronger power -------------------------

    let (reader_id, reader_token) = account(&harness, Some(organization)).await;
    grant(&harness, reader_id, organization, &["events.read"]).await;

    let (status, body) = harness
        .call(get("/api/v1/events/retention", &reader_token))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "reading how much history is kept is reading the bus: {body:?}"
    );

    for (label, request) in [
        (
            "the window",
            patch(
                "/api/v1/events/retention",
                &reader_token,
                json!({ "window_days": 400 }),
            ),
        ),
        (
            "a sweep",
            post("/api/v1/events/retention/sweep", &reader_token),
        ),
    ] {
        let (status, body) = harness.call(request).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a read-only auditor must not be able to trigger {label} — it destroys history: \
             {body:?}"
        );
    }

    // --- The sweeper's own work list ---------------------------------------------------------

    // `organizations_with_events` is what the background worker walks, and it is the piece no
    // route exercises. It has to name every organization with rows and the *right* window for
    // each — reading the default for everybody is a bug that would sweep a 1-day tenant on the
    // 30-day schedule and sweep nobody on the 1-day one.
    let queue = store::organizations_with_events(harness.db.pool(), 50)
        .await
        .expect("the work list must read");
    let mine = queue
        .iter()
        .find(|(id, _)| *id == Some(organization))
        .expect("this organization has events and must be in the work list");
    assert_eq!(
        mine.1, 1,
        "the work list carries the window the operator set"
    );
    assert!(
        queue.iter().any(|(id, _)| *id == Some(other)),
        "a tenant with events is in the work list whoever swept last"
    );

    // --- A platform account has no window of its own ------------------------------------------

    let (status, body) = harness
        .call(get("/api/v1/events/retention", &owner_token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let (status, body) = harness
        .call(patch(
            "/api/v1/events/retention",
            &owner_token,
            json!({ "window_days": 7 }),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a platform account has no window to set, and saying so is better than a 200 that \
         changed nothing: {body:?}"
    );
    assert_eq!(body["error"]["code"], json!("retention_scope_required"));

    harness.dispose().await;
}
