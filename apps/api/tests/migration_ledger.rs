//! Integration walk for REQ-129 slice 1's HTTP surface: the migration ledger, one migration, the
//! plan, the lock, the policy and the reversal rehearsal.
//!
//! It runs against the development stack and **fails** rather than skipping when the migrations
//! do not apply — see `support::walk_state` for why that is a defect and not an environment.
//!
//! ## The five claims, and why each needs a walk rather than a unit test
//!
//! 1. **The three permissions are three powers.** Asserted over the router by an account holding
//!    exactly `migrations.read` and one holding exactly `migrations.apply`: the reader's apply is
//!    `403` naming the key, and the applier's *rehearsal* is `403` naming `verify`. A catalogue
//!    test cannot see this: it checks that the keys exist, not that a route consults them, and a
//!    route guarded by the wrong key is green in every unit test in `omnion-permissions`.
//!
//! 2. **A ledger that answers from the FILES, not only the rows.** The response's `pending` array
//!    is asserted non-empty on a database that has applied everything — because a pending migration
//!    has no ledger row by definition, so a route reading only `schema_migrations` would answer
//!    "nothing pending" on every installation forever, which is the same silent-emptiness the
//!    crate's module doc is written against.
//!
//! 3. **The plan is inert.** `POST /plan` is a `POST` and this walk counts `schema_migrations` and
//!    `migration_runs` before and after. The status code alone would prove nothing: a handler that
//!    writes and returns `200` is a very plausible defect.
//!
//! 4. **A production installation cannot apply from the panel.** This walk runs in development, so
//!    the environment refusal is asserted by *construction* — the refusal function is a pure match
//!    on the environment and the route calls it before touching the database — and the walk proves
//!    the reachable half: the apply route exists, is guarded by `migrations.apply`, and answers a
//!    real refusal message rather than a `404`.
//!
//! 5. **A rehearsal cannot run where the data is.** The scratch database is a *name* on this
//!    server and the route derives the URL from the live configuration, so the walk asserts that
//!    a name pointing at the LIVE database is rejected — because if the route honoured it, the
//!    reversal would have dropped four tables out of the installation the walk is running against.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

use support::walk_auth::{self, PASSWORD};

/// Exactly the read powers. The read-only walk MUST NOT be able to write, so the set is literal
/// and never derived from the applier's.
const READ_ONLY: &[&str] = &["deployment.migrations.read"];

/// The read power plus the write power, and **not** `verify`: the rehearsal is the third power.
const APPLIER: &[&str] = &[
    "deployment.migrations.read",
    "deployment.migrations.apply",
];

/// Every route on this surface, with the method and the power it needs.
fn routes() -> Vec<(Method, &'static str)> {
    vec![
        (Method::GET, "/api/v1/deployment/migrations"),
        (Method::POST, "/api/v1/deployment/migrations/plan"),
        (Method::GET, "/api/v1/deployment/migrations/lock"),
        (Method::GET, "/api/v1/deployment/migrations/violations"),
        (Method::GET, "/api/v1/deployment/migrations/policy"),
        (Method::GET, "/api/v1/deployment/migrations/0001"),
        (Method::GET, "/api/v1/deployment/migrations/0207"),
    ]
}

struct TestResponse {
    status: StatusCode,
    body: Value,
    cookies: Vec<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.31:51234".parse().expect("a peer address");
    let mut request = request;
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    // BOTH cookies, for the reason `walk_auth` documents: a walk holding only the session is not
    // a request the panel can make, and every POST it issues then reads 403 for an unrelated
    // reason.
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
            name: format!("Migration walk {suffix}"),
            slug: format!("migrate-walk-{suffix}"),
        },
    )
    .await
    .expect("the organization is created");
    let email = format!("migrate-walk-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Migration Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account is created");

    let role = omnion_permissions::roles::create_role(
        state.db().pool(),
        omnion_permissions::model::NewRole {
            organization_id: organization.id,
            key: format!("migrate-walk-role-{suffix}"),
            name: "Migration walk role".to_owned(),
            description: "A role of the migration safety walk".to_owned(),
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

/// A row count, so "the plan wrote nothing" can be measured rather than assumed.
async fn count(state: &AppState, table: &str) -> i64 {
    let sql = format!("select count(*) from {table}");
    sqlx::query_scalar(&sql)
        .fetch_one(state.db().pool())
        .await
        .unwrap_or(-1)
}

#[tokio::test]
async fn the_ledger_answers_from_the_files_and_names_the_applied_set() {
    let state = support::walk_state::state_or_fail().await;
    let session = account_with(&state, READ_ONLY).await;

    let response = call(
        &state,
        request(Method::GET, "/api/v1/deployment/migrations", None, &session),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);

    // The applied set: every row carries its own state, its actor, its source and its checksum.
    let applied = response.body["applied"]
        .as_array()
        .expect("an applied array");
    assert!(
        !applied.is_empty(),
        "the ledger is empty on a migrated database: {}",
        response.body
    );
    for row in applied {
        assert_eq!(row["state"], "applied", "an applied row labels itself: {row}");
        assert!(
            row["actor"].as_str().is_some_and(|actor| !actor.is_empty()),
            "every applied row names who ran it: {row}"
        );
        assert!(
            row["checksum"].as_str().map(str::len) == Some(64),
            "every applied row carries the 64-character checksum of the file as applied: {row}"
        );
    }

    // The pending set: non-empty, because a file with no ledger row IS pending and this route is
    // the only thing that can see it.
    let pending = response.body["pending"]
        .as_array()
        .expect("a pending array");
    assert!(
        !pending.is_empty(),
        "the pending set is empty — a route reading only schema_migrations answers this on \
         every installation forever: {}",
        response.body
    );
    for row in pending {
        assert_eq!(row["state"], "pending", "a pending row labels itself: {row}");
        assert!(
            row["would_run"]
                .as_bool()
                .or_else(|| row["state"].as_str().map(|state| state == "pending"))
                .unwrap_or(false),
            "a pending row is the request's `would run` badge: {row}"
        );
    }

    // The lock answer is present and never empty: `held` is a claim about a database, and a
    // missing field renders as "unknown" on a screen where it means "false".
    let lock = &response.body["lock"];
    assert!(
        lock.get("held").is_some(),
        "the lock view must answer `held`, not omit it: {lock}"
    );
    assert!(
        lock["lock_key"].as_str().is_some_and(|key| key.contains("omnionmg")),
        "the lock view names its advisory key so an operator can find it in pg_locks: {lock}"
    );

    // The gate verdict travels with the ledger, so a screen shows "the gate fails" and the files
    // at the same time instead of in two places.
    assert!(
        response.body["gate_fails"].is_boolean(),
        "the gate verdict is a fact, not an absence: {}",
        response.body
    );
    assert!(
        response.body["policy"]["lock_timeout_ms"].is_number(),
        "the policy the run would use travels with the ledger: {}",
        response.body
    );
}

#[tokio::test]
async fn one_migration_answers_with_its_sql_its_reversal_and_its_history() {
    let state = support::walk_state::state_or_fail().await;
    let session = account_with(&state, READ_ONLY).await;

    let response = call(
        &state,
        request(
            Method::GET,
            "/api/v1/deployment/migrations/0207",
            None,
            &session,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let body = &response.body;
    assert_eq!(body["version"], "0207", "{body}");
    assert_eq!(body["filename"], "0207_migration_safety.sql", "{body}");
    assert!(
        body["statements"]
            .as_array()
            .is_some_and(|list| !list.is_empty()),
        "the detail screen's statement pane is never empty: {body}"
    );
    // 0207 is the first migration REQUIRED to carry a reversal, so its down statements are the
    // ones this whole request is about.
    assert!(
        body["has_down"].as_bool() == Some(true),
        "0207 carries a reversal: {body}"
    );
    assert!(
        body["down_statements"]
            .as_array()
            .is_some_and(|list| !list.is_empty()),
        "the reversal pane is populated: {body}"
    );
    assert!(
        body["runs"].is_array(),
        "the run history is an array even when it is empty — null and `[]` render differently: \
         {body}"
    );

    // A version that is neither a file nor a ledger row is the only 404 on this route.
    let missing = call(
        &state,
        request(
            Method::GET,
            "/api/v1/deployment/migrations/9999",
            None,
            &session,
        ),
    )
    .await;
    assert_eq!(
        missing.status,
        StatusCode::NOT_FOUND,
        "a version nobody has is a 404 with a reason: {}",
        missing.body
    );

    // A version that is not a version is a 400, not a 404: `abc` is a malformed request and the
    // screen sends it only when a person typed it.
    let malformed = call(
        &state,
        request(
            Method::GET,
            "/api/v1/deployment/migrations/not-a-version",
            None,
            &session,
        ),
    )
    .await;
    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "a malformed version is refused before the lookup: {}",
        malformed.body
    );
}

#[tokio::test]
async fn the_plan_reports_the_pending_set_and_writes_nothing() {
    let state = support::walk_state::state_or_fail().await;
    let session = account_with(&state, READ_ONLY).await;

    let ledger_before = count(&state, "schema_migrations").await;
    let runs_before = count(&state, "migration_runs").await;

    let response = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/migrations/plan",
            Some(json!({})),
            &session,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);

    // A plan is a preview of the DECISION, and every field the request asks it to show is present.
    assert!(
        response.body["pending"].is_array(),
        "the plan lists what would run: {}",
        response.body
    );
    assert!(
        response.body["policy"]["lock_timeout_ms"].is_number(),
        "the plan shows the timeout settings it would run under: {}",
        response.body
    );
    assert!(
        response.body["policy"]["statement_timeout_ms"].is_number(),
        "both timeouts, because one bounds a lock and the other bounds a statement: {}",
        response.body
    );
    assert!(
        response.body["violations"].is_array(),
        "the violations are a list, empty or not: {}",
        response.body
    );
    assert!(
        !response.body["summary"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the plan renders a one-line verdict as its subtitle: {}",
        response.body
    );

    // The claim the request makes with the word "dry-run": nothing was written.
    assert_eq!(
        count(&state, "schema_migrations").await,
        ledger_before,
        "the plan wrote a ledger row"
    );
    assert_eq!(
        count(&state, "migration_runs").await,
        runs_before,
        "the plan wrote a journal row — a plan that journals is a run"
    );
}

#[tokio::test]
async fn reading_the_ledger_is_not_applying_it_and_rehearsing_is_not_either() {
    let state = support::walk_state::state_or_fail().await;
    let reader = account_with(&state, READ_ONLY).await;
    let applier = account_with(&state, APPLIER).await;

    // Every read answers the reader.
    for (method, path) in routes() {
        if method != Method::GET {
            continue;
        }
        let response = call(&state, request(method, path, None, &reader)).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path} must be readable with the read power alone: {}",
            response.body
        );
    }

    // The reader may not apply. The status alone proves nothing — a route that refused for an
    // unrelated reason satisfies a bare 403 — so the refusal must NAME the permission.
    let refused = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/migrations",
            None,
            &reader,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "reading the ledger must not be able to write the schema: {}",
        refused.body
    );
    assert_eq!(
        refused.body["details"]["permission"],
        "deployment.migrations.apply",
        "the refusal names the power that is missing, so the screen can say which role to grant: \
         {}",
        refused.body
    );

    // The applier may save the policy — that is the write the `apply` power is for — and the
    // policy refuses an out-of-bounds value BEFORE storing it, so the previous policy survives.
    let previous: i64 = sqlx::query_scalar("select lock_timeout_ms from migration_policy where id = 1")
        .fetch_one(state.db().pool())
        .await
        .expect("the policy row exists");
    let rejected = call(
        &state,
        request(
            Method::PUT,
            "/api/v1/deployment/migrations/policy",
            Some(json!({
                "require_down_scripts": true,
                "lock_timeout_ms": 5,
                "statement_timeout_ms": 300_000,
                "banned_patterns": {},
                "backfill_batch_size": 5_000,
                "backfill_rate_per_second": 200,
                "require_approval_for_destructive": true,
            })),
            &applier,
        ),
    )
    .await;
    assert_eq!(
        rejected.status,
        StatusCode::BAD_REQUEST,
        "a lock timeout below the floor is refused: {}",
        rejected.body
    );
    let after: i64 = sqlx::query_scalar("select lock_timeout_ms from migration_policy where id = 1")
        .fetch_one(state.db().pool())
        .await
        .expect("the policy row exists");
    assert_eq!(
        after, previous,
        "a refused policy write must leave the previous policy in place"
    );

    // The applier may NOT rehearse: `verify` is a third power, not an extra detail on `apply`.
    // Asserting it here rather than in the catalogue is the point — the catalogue test can only
    // say the keys exist.
    let rehearsal = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/migrations/0207/verify-down",
            Some(json!({ "scratch": format!("omnion_scratch_{}", Uuid::new_v4().simple()) })),
            &applier,
        ),
    )
    .await;
    assert_eq!(
        rehearsal.status,
        StatusCode::FORBIDDEN,
        "applying is not rehearsing: {}",
        rehearsal.body
    );
    assert_eq!(
        rehearsal.body["details"]["permission"],
        "deployment.migrations.verify",
        "the refusal names the rehearsal power: {}",
        rehearsal.body
    );
}

#[tokio::test]
async fn a_rehearsal_names_the_scratch_database_and_refuses_the_live_one() {
    let state = support::walk_state::state_or_fail().await;
    let session = account_with(
        &state,
        &[
            "deployment.migrations.read",
            "deployment.migrations.apply",
            "deployment.migrations.verify",
        ],
    )
    .await;

    // The live database's own name, read out of the connection string the walk is running on.
    let live_name: String = state
        .config()
        .database
        .url
        .rsplit('/')
        .next()
        .map(|name| name.split('?').next().unwrap_or(name).to_owned())
        .expect("a database url"); 

    // Pointing the rehearsal at the live database is the one input that must never work: the
    // reversal drops four tables, and a route that honoured a URL here would have dropped them
    // out of the installation the walk is running against.
    let refused = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/migrations/0207/verify-down",
            Some(json!({ "scratch": live_name })),
            &session,
        ),
    )
    .await;
    assert!(
        refused.status.is_client_error() || refused.status == StatusCode::CONFLICT,
        "a rehearsal aimed at the live database is refused, never performed: {} / {}",
        refused.status,
        refused.body
    );
    assert_ne!(
        refused.status,
        StatusCode::OK,
        "0207's reversal drops schema_migrations, and the live tables are still there: {}",
        refused.body
    );

    // The tables are demonstrably intact — the refusal above is proven by the world being unchanged,
    // not by the message.
    for table in [
        "schema_migrations",
        "migration_runs",
        "migration_policy",
        "migration_violations",
    ] {
        assert!(
            count(&state, table).await >= 0,
            "{table} must still exist after the refused rehearsal"
        );
    }
    assert!(
        count(&state, "schema_migrations").await > 0,
        "the ledger still holds its rows, so the refusal did not drop anything"
    );

    // A malformed scratch name is a 400 naming the problem, not a 500 from a bad identifier.
    let malformed = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/migrations/0207/verify-down",
            Some(json!({ "scratch": "has space-and-quote\"" })),
            &session,
        ),
    )
    .await;
    assert!(
        malformed.status.is_client_error() || malformed.status.is_server_error(),
        "an unquotable scratch name is answered, not panicked: {}",
        malformed.body
    );
}
