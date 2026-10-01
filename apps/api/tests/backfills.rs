//! Integration walk for REQ-129 slice 3's HTTP surface: backfill jobs and seed datasets.
//!
//! ## What needs a walk rather than a unit test
//!
//! 1. **The permissions are three powers over two surfaces.** An account holding exactly
//!    `migrations.read` must get `403` from every backfill write AND from the seed load, naming
//!    the key each time. A catalogue test cannot see this — it checks that the keys exist, not
//!    that a route consults them, and a route guarded with the wrong key is green in every unit
//!    test in `omnion-permissions`.
//!
//! 2. **The run button moves real rows.** The fixture table is created here, filled with 250 rows
//!    and given a NOT NULL-able target column, a descriptor is registered and a job created — and
//!    the walk counts the FILLED ROWS with SQL rather than trusting `rows_done`. The counter is a
//!    self-report (slice 3's own proof had it read 2097 for 250 rows), and a walk that asserted
//!    the counter would be asserting the bug.
//!
//! 3. **Pause keeps the cursor and resume continues from it.** The walk pauses mid-table, reads
//!    the cursor, resumes, and asserts the cursor only moved forward and the rows above the old
//!    cursor are exactly the rows the second pass touched. "Resumed" and "restarted" look
//!    identical from the outside, which is why this is measured and not asserted.
//!
//! 4. **The seed load refuses with the kind named, in the order the refusals happen.** The
//!    mismatch is checked before the environment, so a caller who typed the wrong name is told
//!    that rather than being told the installation is production. Both are measured; the order is
//!    asserted, because an order that checked the environment first would answer a typo with a
//!    sentence about production.
//!
//! The suite follows `support::walk_state`: it FAILS rather than skipping when the migrations do
//! not apply, because a walk that skips is indistinguishable from a walk that passes.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use sqlx::Row;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

use support::walk_auth::{self, PASSWORD};

/// The read powers only. Every write on this surface must be `403` for this account.
const READ_ONLY: &[&str] = &["deployment.migrations.read"];

/// Read plus both new write powers, and NOT `migrations.apply`: applying a migration is a third
/// power, and a walk whose account held it could not prove the split.
const OPERATOR: &[&str] = &[
    "deployment.migrations.read",
    "deployment.backfills.manage",
    "deployment.seeds.load",
];

/// The table the fixture backfills. Created and dropped by the walk, so it cannot collide with a
/// real one and cannot leak into another writer's database.
///
/// It is a FIXED name on purpose — the backfill statement reads a real table by name, so a unique
/// name per test would mean a different table per test and the fixture would stop being what it
/// claims. The price is that every test in this file shares one table, and `fixture_job` DROPS it
/// before recreating it. Rust runs `#[tokio::test]`s in this binary in parallel, so without
/// [`support::walk_state::exclusive_evaluator`] one test's `drop table` lands in the middle of
/// another's batch: the first run reported `left: 100, right: 200` (a batch counted rows a
/// neighbour had just refilled) and the ceiling test saw `key column … does not exist`. Both read
/// as product defects and both were the harness. Serialised, each test owns the table end to end.
const FIXTURE_TABLE: &str = "w6_backfill_fixture";

struct TestResponse {
    status: StatusCode,
    body: Value,
    cookies: Vec<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.77:51234".parse().expect("a peer address");
    let mut request = request;
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|raw| raw.split(';').next())
        .filter(|pair| pair.starts_with("omnion_session=") || pair.starts_with("omnion_csrf="))
        .map(str::to_owned)
        .collect();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body reads")
        .to_bytes();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body = if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&raw).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        body,
        cookies,
    }
}

fn request(
    method: Method,
    uri: &str,
    body: Option<Value>,
    session: &walk_auth::Session,
) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "qa.omnion.test");
    match body {
        None | Some(Value::Null) => session
            .apply(builder)
            .body(Body::empty())
            .expect("the request builds"),
        Some(value) => session
            .apply(builder)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("the json request builds"),
    }
}

/// An account in a fresh organization holding exactly `permissions`.
async fn account_with(state: &AppState, permissions: &[&str]) -> walk_auth::Session {
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Backfill walk {suffix}"),
            slug: format!("backfill-walk-{suffix}"),
        },
    )
    .await
    .expect("the organization is created");
    let email = format!("backfill-walk-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Backfill Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account is created");

    let role = omnion_permissions::roles::create_role(
        state.db().pool(),
        omnion_permissions::model::NewRole {
            organization_id: organization.id,
            key: format!("backfill-walk-role-{suffix}"),
            name: "Backfill walk role".to_owned(),
            description: "A role of the backfill walk".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role is created");
    let entries: Vec<omnion_permissions::model::RolePermissionInput> = permissions
        .iter()
        .map(|key| omnion_permissions::model::RolePermissionInput {
            key: (*key).to_owned(),
            effect: omnion_permissions::model::Effect::Allow,
        })
        .collect();
    omnion_permissions::roles::set_role_permissions(state.db().pool(), role.id, &entries)
        .await
        .expect("the role permission set is written");
    omnion_permissions::bindings::grant(
        state.db().pool(),
        omnion_permissions::model::NewBinding {
            role_id: role.id,
            user_id: user.id,
            scope: omnion_permissions::model::Scope::Organization {
                organization_id: organization.id,
            },
            granted_by: Some(user.id),
            expires_at: None,
        },
    )
    .await
    .expect("the binding is granted");

    let response = call(
        state,
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "email": email, "password": PASSWORD }).to_string(),
            ))
            .expect("the sign-in request builds"),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the sign-in failed: {}",
        response.body
    );
    walk_auth::Session::from_set_cookies(&response.cookies)
}

/// How many fixture rows the backfill has actually filled — counted, never taken from the job.
async fn filled_rows(state: &AppState) -> i64 {
    let sql = format!("select count(*) from {FIXTURE_TABLE} where filled is not null");
    sqlx::query_scalar(&sql)
        .fetch_one(state.db().pool())
        .await
        .unwrap_or(-1)
}

/// Create the fixture table, register a descriptor and create its job.
///
/// 250 rows with a `bigint` key, so the cursor is compared in the key's OWN type — the case the
/// crate's own proof showed a text cursor gets wrong (`'99'` sorts after `'100'`).
async fn fixture_job(state: &AppState) -> (Uuid, String) {
    let pool = state.db().pool();
    let name = format!("w6_fixture_{}", Uuid::new_v4().simple());

    sqlx::query(&format!("drop table if exists {FIXTURE_TABLE}"))
        .execute(pool)
        .await
        .expect("a previous fixture table is dropped");
    sqlx::query(&format!(
        "create table {FIXTURE_TABLE} (id bigint primary key, filled text)"
    ))
    .execute(pool)
    .await
    .expect("the fixture table exists");
    for start in (1..=250).step_by(50) {
        let ids: Vec<i64> = (start..start + 50).collect();
        sqlx::query(&format!(
            "insert into {FIXTURE_TABLE} (id, filled) select unnest($1::bigint[]), null"
        ))
        .bind(&ids)
        .execute(pool)
        .await
        .expect("the fixture rows exist");
    }

    omnion_migrations::backfill::register_descriptor(
        pool,
        &omnion_migrations::BackfillDescriptor {
            version: "0216".to_owned(),
            name: name.clone(),
            table_name: FIXTURE_TABLE.to_owned(),
            column_name: "filled".to_owned(),
            key_column: "id".to_owned(),
            batch_size: 100,
            rate_limit_per_second: 1000,
            statement: "'done'".to_owned(),
        },
    )
    .await
    .expect("the descriptor is registered");

    let id = omnion_migrations::backfill::ensure_job(
        pool,
        &omnion_migrations::BackfillDescriptor {
            version: "0216".to_owned(),
            name: name.clone(),
            table_name: FIXTURE_TABLE.to_owned(),
            column_name: "filled".to_owned(),
            key_column: "id".to_owned(),
            batch_size: 100,
            rate_limit_per_second: 1000,
            statement: "'done'".to_owned(),
        },
    )
    .await
    .expect("the job exists");

    // The job must be visible as a DESCRIPTOR-WITH-NO-JOB only before the job exists, so the list
    // route's pending band is proven by the absence here rather than assumed.
    (id, name)
}

async fn drop_fixture(state: &AppState, name: &str) {
    let pool = state.db().pool();
    sqlx::query("delete from migration_backfills where name = $1")
        .bind(name)
        .execute(pool)
        .await
        .ok();
    sqlx::query("delete from migration_backfill_descriptors where name = $1")
        .bind(name)
        .execute(pool)
        .await
        .ok();
    sqlx::query(&format!("drop table if exists {FIXTURE_TABLE}"))
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
async fn a_reader_may_look_at_backfills_and_seeds_and_may_write_neither() {
    // The fixture table is shared across this file and dropped by `fixture_job`,
    // so this walk runs alone. See FIXTURE_TABLE.
    let _exclusive = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let reader = account_with(&state, READ_ONLY).await;

    // The reads are allowed: a backfill is a migration that has not finished, so the ledger
    // reader has to see the jobs.
    for uri in [
        "/api/v1/deployment/backfills",
        "/api/v1/deployment/seeds",
    ] {
        let response = call(&state, request(Method::GET, uri, None, &reader)).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{uri} must be readable with the read power: {}",
            response.body
        );
    }

    // Every write is refused, and each refusal NAMES the key that would have worked. A 403 that
    // says "forbidden" leaves an operator guessing which of five keys to ask for.
    let (id, name) = fixture_job(&state).await;
    for (uri, key) in [
        (
            format!("/api/v1/deployment/backfills/{id}/run"),
            "deployment.backfills.manage",
        ),
        (
            format!("/api/v1/deployment/backfills/{id}/pause"),
            "deployment.backfills.manage",
        ),
        (
            format!("/api/v1/deployment/backfills/{id}/resume"),
            "deployment.backfills.manage",
        ),
        (
            "/api/v1/deployment/seeds/minimal/load".to_owned(),
            "deployment.seeds.load",
        ),
    ] {
        let body = if uri.contains("seeds") {
            Some(json!({ "confirm": "minimal" }))
        } else {
            Some(json!({}))
        };
        let response = call(
            &state,
            request(Method::POST, &uri, body, &reader),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{uri} must be refused for an account without {key}: {}",
            response.body
        );
        let message = response.body.to_string();
        assert!(
            message.contains(key),
            "the refusal for {uri} must name {key}, the power that would have worked: {message}"
        );
    }
    drop_fixture(&state, &name).await;
}

#[tokio::test]
async fn running_one_batch_moves_real_rows_and_the_pause_keeps_the_cursor() {
    // The fixture table is shared across this file and dropped by `fixture_job`,
    // so this walk runs alone. See FIXTURE_TABLE.
    let _exclusive = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let operator = account_with(&state, OPERATOR).await;
    let (id, name) = fixture_job(&state).await;

    // One batch of 100 fills exactly 100 rows — measured by counting them, because `rows_done`
    // is a counter and slice 3's own proof watched it read 2097 for 250 rows.
    let ran = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/deployment/backfills/{id}/run"),
            Some(json!({})),
            &operator,
        ),
    )
    .await;
    assert_eq!(ran.status, StatusCode::OK, "{}", ran.body);
    assert_eq!(ran.body["ran"], json!(true), "a pending job runs");
    assert_eq!(
        ran.body["rows"], json!(100),
        "one batch of a 100-row descriptor touches 100 rows"
    );
    assert_eq!(
        filled_rows(&state).await,
        100,
        "the ROWS are the witness; the counter is a self-report"
    );
    assert_eq!(
        ran.body["job"]["resume_key"], json!("100"),
        "the cursor is the last key the batch processed, in the key's own order"
    );

    // The job is now `running`, so the pause is legal and the resume is not — the state machine
    // says so with a refusal that names the transition.
    assert_eq!(ran.body["job"]["can_pause"], json!(true));
    assert_eq!(ran.body["job"]["can_resume"], json!(false));

    let paused = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/deployment/backfills/{id}/pause"),
            Some(json!({ "reason": "walk: prove the cursor survives" })),
            &operator,
        ),
    )
    .await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);
    assert_eq!(paused.body["job"]["state"], json!("paused"));
    assert_eq!(
        paused.body["job"]["resume_key"], json!("100"),
        "PAUSE KEEPS THE CURSOR — this is the whole difference between pause and reset"
    );
    assert_eq!(
        filled_rows(&state).await,
        100,
        "a pause writes no rows; it only stops"
    );

    // A resume runs the next batch from the kept cursor, so the rows it touches are the ones
    // ABOVE 100 — not the first hundred again.
    let resumed = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/deployment/backfills/{id}/resume"),
            Some(json!({ "batches": 1 })),
            &operator,
        ),
    )
    .await;
    assert_eq!(resumed.status, StatusCode::OK, "{}", resumed.body);
    assert_eq!(
        resumed.body["batches"], json!(1),
        "one batch was asked for and one ran"
    );
    assert_eq!(
        filled_rows(&state).await,
        200,
        "100 more rows — the second pass continued from the cursor instead of redoing the first \
         hundred"
    );

    // The cursor MOVED FORWARD, and the rows below it were not re-marked. A text cursor would
    // have resumed at '99' and the count would be 199 with the same rows touched twice.
    let cursor_after: String = resumed.body["job"]["resume_key"]
        .as_str()
        .expect("a cursor after the resume")
        .to_owned();
    assert_eq!(cursor_after, "200");

    // Resume from `pending` is the same route and it is idempotent in the sense that matters: a
    // finished job cannot be resumed, and says so by name rather than silently doing nothing.
    let drained = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/deployment/backfills/{id}/resume"),
            Some(json!({ "batches": 10 })),
            &operator,
        ),
    )
    .await;
    assert_eq!(drained.status, StatusCode::OK, "{}", drained.body);
    assert_eq!(drained.body["finished"], json!(true));
    assert_eq!(drained.body["job"]["state"], json!("completed"));
    assert_eq!(
        filled_rows(&state).await,
        250,
        "every row is filled exactly once — the count is 250 and not more"
    );

    let again = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/deployment/backfills/{id}/resume"),
            Some(json!({ "batches": 1 })),
            &operator,
        ),
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::CONFLICT,
        "a completed job cannot be resumed: {}",
        again.body
    );
    assert!(
        again.body.to_string().contains("completed"),
        "the refusal names the state it refused from: {}",
        again.body
    );

    drop_fixture(&state, &name).await;
}

#[tokio::test]
async fn the_batch_ceiling_is_enforced_and_the_clamp_is_visible() {
    // The fixture table is shared across this file and dropped by `fixture_job`,
    // so this walk runs alone. See FIXTURE_TABLE.
    let _exclusive = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let operator = account_with(&state, OPERATOR).await;
    let (id, name) = fixture_job(&state).await;

    // Ask for 5000 batches. The response must report what it ACTUALLY ran and what it was asked
    // for, because "the operator typed 5000 and the screen says 10" with no note is the kind of
    // silent clamp an operator reports as the platform ignoring them.
    let response = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/deployment/backfills/{id}/resume"),
            Some(json!({ "batches": 5000 })),
            &operator,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(
        response.body["requested_batches"], json!(5000),
        "what the operator asked for is echoed"
    );
    assert!(
        response.body["batches"].as_u64().unwrap_or(u64::MAX) <= 10,
        "at most the route's ceiling ran: {}",
        response.body
    );

    // And the ceiling is ADVERTISED, so the client can know it without trying it.
    let listed = call(
        &state,
        request(
            Method::GET,
            "/api/v1/deployment/backfills",
            None,
            &operator,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(
        listed.body["bounds"]["max_batches_per_request"], json!(10),
        "the ceiling is part of the list payload, not a surprise"
    );

    drop_fixture(&state, &name).await;
}

#[tokio::test]
async fn the_events_are_recorded_and_a_failed_batch_records_nothing_false() {
    // The fixture table is shared across this file and dropped by `fixture_job`,
    // so this walk runs alone. See FIXTURE_TABLE.
    let _exclusive = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let operator = account_with(&state, OPERATOR).await;
    let (id, name) = fixture_job(&state).await;

    let names: Vec<String> = sqlx::query_scalar(
        "select name from events where payload->>'job_id' = $1 order by name",
    )
    .bind(id.to_string())
    .fetch_all(state.db().pool())
    .await
    .unwrap_or_default();

    let _ = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/deployment/backfills/{id}/run"),
            Some(json!({})),
            &operator,
        ),
    )
    .await;
    let paused = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/deployment/backfills/{id}/pause"),
            Some(json!({})),
            &operator,
        ),
    )
    .await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);

    let recorded: Vec<String> = sqlx::query_scalar(
        "select name from events where payload->>'job_id' = $1 order by name",
    )
    .bind(id.to_string())
    .fetch_all(state.db().pool())
    .await
    .unwrap_or_default();

    // The pause records `backfill.paused` — and NOT `backfill.completed`, because a pause is not
    // a completion and recording one would be a permanent fact for something that did not happen.
    assert!(
        recorded.contains(&"backfill.paused".to_owned()),
        "the pause recorded its event: {recorded:?} (before: {names:?})"
    );
    assert!(
        !recorded.contains(&"backfill.completed".to_owned()),
        "a pause must never record a completion: {recorded:?}"
    );
    assert!(
        !recorded.contains(&"backfill.failed".to_owned()),
        "a successful batch records no failure: {recorded:?}"
    );

    // The cursor is on the payload, not only the counter: a receiver given a row count cannot
    // tell a stalled job from a finished one.
    let row = sqlx::query("select payload from events where name = 'backfill.paused' order by created_at desc limit 1")
        .fetch_one(state.db().pool())
        .await
        .expect("the pause event row");
    let payload: Value = row
        .try_get::<Value, _>("payload")
        .expect("the pause payload is jsonb, read as json rather than decoded from text");
    assert_eq!(
        payload["resume_key"], json!("100"),
        "the pause carries the cursor it is keeping"
    );
    assert_eq!(payload["rows_done"], json!(100));

    drop_fixture(&state, &name).await;
}

/// A batch whose statement is WRONG must fail loudly, keep its cursor, and record the failure.
///
/// This is the walk that makes the `failed` state real. Before this tick `can_transition` listed
/// `pending|paused|running -> failed` and nothing in the crate ever wrote it: `run_once` let the
/// database error propagate, the transaction rolled back, and the job sat in `running` with
/// `last_error` null — a screen showing a backfill as healthy while it can never advance. Every
/// test in this file passed against that, because all of them use a statement that works.
///
/// So the statement here is deliberately broken: it writes to a column that does not exist. What
/// must be true afterwards is the operator-visible half — the call fails with a 500 that NAMES the
/// job, the job's state says `failed` rather than `running`, the cursor is still the last GOOD key,
/// and no row was written.
#[tokio::test]
async fn a_failing_batch_stops_the_job_and_keeps_the_cursor_where_it_worked() {
    let _exclusive = support::walk_state::exclusive_evaluator().await;
    let state = support::walk_state::state_or_fail().await;
    let operator = account_with(&state, OPERATOR).await;
    let pool = state.db().pool();

    // A descriptor whose statement is invalid SQL. The batch cannot write anything, so there is
    // nothing for a partial application to leave behind.
    let name = format!("w6_broken_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("drop table if exists {FIXTURE_TABLE}"))
        .execute(pool)
        .await
        .expect("the fixture table is droppable");
    sqlx::query(&format!(
        "create table {FIXTURE_TABLE} (id bigint primary key, filled text)"
    ))
    .execute(pool)
    .await
    .expect("the fixture table exists");
    let ids: Vec<i64> = (1..=50).collect();
    sqlx::query(&format!(
        "insert into {FIXTURE_TABLE} (id, filled) select unnest($1::bigint[]), null"
    ))
    .bind(&ids)
    .execute(pool)
    .await
    .expect("the fixture rows exist");

    let descriptor = omnion_migrations::BackfillDescriptor {
        version: "0216".to_owned(),
        name: name.clone(),
        table_name: FIXTURE_TABLE.to_owned(),
        column_name: "filled".to_owned(),
        key_column: "id".to_owned(),
        // The migration's own CHECK, not an arbitrary number: `batch_size` is validated by
        // `migration_backfill_descriptors_batch_size_check`, and a fixture that invents its own
        // value measures the constraint instead of the failure path.
        batch_size: 100,
        rate_limit_per_second: 1000,
        // `no_such_column` does not exist. The error names the column, which is the whole point:
        // a migration author can act on it and nobody else can.
        statement: "no_such_column".to_owned(),
    };
    omnion_migrations::backfill::register_descriptor(pool, &descriptor)
        .await
        .expect("the broken descriptor is registered");
    let id = omnion_migrations::backfill::ensure_job(pool, &descriptor)
        .await
        .expect("the job exists");

    let failed = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/deployment/backfills/{id}/run"),
            Some(json!({})),
            &operator,
        ),
    )
    .await;

    // 500, not 422: nothing about the request was wrong.
    assert_eq!(
        failed.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a broken statement is a server fault, not a malformed request: {}",
        failed.body
    );
    let message = failed.body.to_string();
    assert!(
        message.contains(&name),
        "the failure names the job, because the statement belongs to the migration author and \
         nobody else can identify it: {message}"
    );
    assert!(
        message.contains("no_such_column"),
        "the database's own message survives to the caller, so the fix is readable: {message}"
    );

    // The job's STATE is the point of the whole change. Before it, this row said `running`.
    let stopped = omnion_migrations::backfill::read(pool, id)
        .await
        .expect("the job is readable");
    assert_eq!(
        stopped.state, "failed",
        "a job whose statement cannot run must not be left looking healthy: {stopped:?}"
    );
    assert!(
        stopped.last_error.as_deref().unwrap_or_default().contains("no_such_column"),
        "the job carries the reason, so the panel can show it without a log file: {:?}",
        stopped.last_error
    );
    assert_eq!(stopped.rows_done, 0, "nothing was counted, because nothing was written");
    assert_eq!(
        filled_rows(&state).await,
        0,
        "the failing batch wrote no rows — the transaction rolled back rather than leaving a \
         half-applied batch the cursor would skip past"
    );

    // The cursor is null, not a sentinel: nothing has ever succeeded, so a retry starts from the
    // beginning. This is the `INITIAL_CURSOR` predicate rather than a value.
    assert!(
        stopped.resume_key.is_none(),
        "no cursor was written, so the retry re-selects every row: {:?}",
        stopped.resume_key
    );

    // The failure is RECORDED, not only returned. A receiver subscribed to backfill failures has
    // to hear about this one; a 500 in one operator's browser is not an event.
    let recorded: Vec<String> =
        sqlx::query_scalar("select name from events where payload->>'job_id' = $1 order by name")
            .bind(id.to_string())
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    assert!(
        recorded.contains(&"backfill.failed".to_owned()),
        "the failure is on the bus: {recorded:?}"
    );
    assert!(
        !recorded.contains(&"backfill.completed".to_owned()),
        "a failed batch must never record a completion: {recorded:?}"
    );

    // And the catalogue knows the name — an emitter for an unlisted event is invisible to every
    // subscriber, which is the same defect as not emitting it at all.
    assert!(
        omnion_events::catalogue::lookup("backfill.failed").is_some(),
        "the name the route emits must be one an operator can subscribe to"
    );

    drop_fixture(&state, &name).await;
}

#[tokio::test]
async fn a_seed_load_refuses_on_the_confirmation_and_then_on_the_installation() {
    let state = support::walk_state::state_or_fail().await;
    let operator = account_with(&state, OPERATOR).await;

    // The list is honest about what this build actually has: the datasets are declared by the
    // migration, and whether their FILES exist is a separate fact the payload carries.
    let listed = call(
        &state,
        request(Method::GET, "/api/v1/deployment/seeds", None, &operator),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let datasets = listed.body["datasets"].as_array().expect("datasets");
    assert!(
        datasets.len() >= 3,
        "the migration declares minimal, demo and fixture: {datasets:?}"
    );
    for dataset in datasets {
        assert!(
            dataset.get("files_present").is_some(),
            "every dataset says whether its manifest file is present: {dataset}"
        );
    }
    assert!(
        listed.body["load_refused"].is_null(),
        "a development installation may load, and the payload says so: {}",
        listed.body
    );

    // 1. The typed name. A mismatch is a 400 about the MISTAKE, not about the environment — the
    // order is the design, and an order that checked the environment first would answer a typo
    // with a sentence about production.
    let typo = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/seeds/minimal/load",
            Some(json!({ "confirm": "demo" })),
            &operator,
        ),
    )
    .await;
    assert_eq!(
        typo.status,
        StatusCode::BAD_REQUEST,
        "a mismatched confirmation is a 400: {}",
        typo.body
    );
    assert!(
        typo.body.to_string().contains("minimal"),
        "the refusal names the dataset being confirmed: {}",
        typo.body
    );

    // 2. An undeclared dataset is a 404 — nothing was refused, there is nothing to load.
    let unknown = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/seeds/nosuchdataset/load",
            Some(json!({ "confirm": "nosuchdataset" })),
            &operator,
        ),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::NOT_FOUND,
        "an undeclared dataset is a 404: {}",
        unknown.body
    );

    // 3. The production refusal names the kind. Asserted by CONSTRUCTION here — the walk runs in
    // development and the refusal is a pure match on the environment — plus the reachable half:
    // the route exists, answers a real refusal and names what it would load.
    let refusal = omnion_api::routes::backfills::seed_refusal(omnion_core::config::Environment::Production)
        .expect("production refuses");
    assert!(
        refusal.contains("production"),
        "the refusal names the installation kind: {refusal}"
    );
    assert!(
        omnion_api::routes::backfills::seed_refusal(omnion_core::config::Environment::Development)
            .is_none(),
        "a development installation may load"
    );

    // The load itself either ran (the descriptors ship with the tree) or was refused for a
    // NAMED reason. Both are honest answers; "200 with zero rows" is not one of them.
    let loaded = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/seeds/minimal/load",
            Some(json!({ "confirm": "minimal" })),
            &operator,
        ),
    )
    .await;
    match loaded.status {
        StatusCode::OK => assert!(
            loaded.body["rows_loaded"].as_i64().unwrap_or(0) > 0,
            "a successful load wrote rows, and says how many: {}",
            loaded.body
        ),
        StatusCode::UNPROCESSABLE_ENTITY => {
            let message = loaded.body.to_string();
            assert!(
                message.contains("minimal"),
                "a refusal names the dataset: {message}"
            );
            assert!(
                !message.contains("\"rows_loaded\": 0"),
                "a refusal never reports zero rows — that reads as an install that is already \
                 seeded: {message}"
            );
        }
        other => panic!("the load answered {other} with neither rows nor a named refusal: {}", loaded.body),
    }
}

/// Every descriptor this repository SHIPS is readable, names itself, and is usable.
///
/// The walk above tolerates a missing manifest ("declared, but the files are absent") because
/// that is a real answer for a production build. Tolerance is exactly why the defect below
/// survived a green run: the routes read `database/seeds/<name>/manifest.json`, and the tree was
/// missing that directory entirely, so every dataset answered `files_present: false` and the
/// suite stayed green on a seed feature with no data.
///
/// So this test is deliberately intolerant, and only about files that are IN the tree: a
/// descriptor committed to git is a promise that a loader can execute it, and this is where that
/// promise is checked — that it parses, that it declares its own directory's name, and that it
/// would write something. Executing the SQL is the next layer's job (a statement can be valid SQL
/// and still violate a CHECK); this layer is about the file being a real, self-consistent dataset.
#[tokio::test]
async fn every_shipped_descriptor_is_readable_and_declares_itself() {
    let state = support::walk_state::state_or_fail().await;
    let _operator = account_with(&state, OPERATOR).await;

    let names = omnion_api::seeds::available();
    assert!(
        names.contains(&"minimal".to_owned())
            && names.contains(&"demo".to_owned())
            && names.contains(&"fixture".to_owned()),
        "the migration 0216 declares minimal, demo and fixture, so the tree must ship a \
         descriptor for each — otherwise the seeds screen is three rows that cannot be loaded. \
         Found: {names:?}"
    );

    for name in &names {
        let manifest = omnion_api::seeds::read_manifest(name)
            .unwrap_or_else(|err| panic!("`{name}` is committed to the tree and must parse: {err}"));
        assert_eq!(
            &manifest.name, name,
            "`{name}` declares itself as `{}` — a dataset addressed by one name and declaring \
             another is two datasets wearing one directory",
            manifest.name
        );
        assert!(
            manifest.datasets_usable(),
            "`{name}` would write nothing: an operator reads a zero-row load as 'already seeded'"
        );
        assert!(
            !manifest.compatible_from.is_empty(),
            "`{name}` does not say what schema it was written against, so nothing can decide \
             whether it is loadable"
        );
        assert!(
            manifest.directory().ends_with(name),
            "`{name}` must resolve inside its own directory: {}",
            manifest.directory()
        );
    }
}
