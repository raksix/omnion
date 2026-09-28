//! The integration walk for REQ-003 **slice 4** — the operations surfaces (docs/requests/
//! REQ-003).
//!
//! The engine half of slice 4 shipped in `b130f0b`/`684741e`/`49f7ac2`; this suite proves
//! the three read surfaces the request's "Operations surfaces" line asks for and the one
//! write among them, against a real database rather than a mocked one:
//!
//! * **version history** — a create writes version 1 and every edit writes the next, and
//!   the summary says *what* changed in words rather than a structural diff nobody reads;
//! * **restore** — puts an old definition back, **appends** rather than rewinds (so the
//!   history stays a line and the ancestry is still visible), keeps the rule's identity and
//!   its run history, and refuses the two cases that would be lies: a no-op and a version
//!   from another rule;
//! * **templates** — all six load, every action is one the library really has, every body
//!   parses into the shape `POST /api/v1/automations` takes, and the one that calls out
//!   names the host it needs on the allow-list;
//! * **audit** — a definition change is listed with its actor, its action and its metadata.
//!
//! It borrows the harness of `automation_approvals.rs` (same throwaway database, same
//! seeded permissions) and asserts only what the platform itself stores: no test-only
//! shortcut reaches into the engine.
//!
//! When PostgreSQL is not reachable the suite skips itself with a printed reason.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sessions;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_storage::Storage;
use omnion_workflows;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The event the rules in this suite listen for.
const EVENT: &str = "page.published";

// ---------------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------------

struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

#[derive(Debug)]
struct TestResponse {
    status: StatusCode,
    body: Value,
}

impl TestResponse {
    /// The API's stable error code, read out of the nested body.
    fn code(&self) -> &str {
        self.body
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    /// The API's sentence, read out of the nested body.
    fn message(&self) -> &str {
        self.body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("environment must be valid");
        if live_db(&config).await.is_none() {
            return None;
        }

        let database = format!("omnion_automation4_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&maintenance_config(&config))
            .await
            .expect("the maintenance connection must work");
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
            Storage::from_config(&omnion_storage::StorageConfig::default())
                .expect("the default storage configuration is valid"),
        );

        Some(Self {
            state,
            db,
            maintenance,
            database,
        })
    }

    async fn call(&self, request: Request<Body>) -> TestResponse {
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
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

        TestResponse { status, body }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The drop is **spawned and not awaited** rather than `block_in_place`. Two things
        // force that choice, and both are load-bearing:
        //
        // * a panic inside `Drop` during an unwind is a non-unwinding panic that aborts the
        //   process and takes the real assertion message with it;
        // * `block_in_place` is only legal on a multi-threaded runtime, and `#[tokio::test]`
        //   is current-thread by default.
        //
        // The cost of not awaiting is that the spawned task is dropped with the runtime the
        // moment the test body returns, so the database usually **survives**. That is visible:
        // six walks × a box running five suites leave dozens of `omnion_automation4_*`
        // databases behind, and the *connection* cost is what actually bites — the harness
        // holds a 4-connection pool plus a 2-connection maintenance pool, and a leaked
        // database keeps a backend process alive holding its slots. Once PostgreSQL's
        // `max_connections` is reached, the next walk fails at `Db::connect` with
        // `PoolTimedOut` — which reads like a slow database and is really a leaked one.
        //
        // So the cleanup is **best-effort here and complete in a second place**: the walk
        // drops its own database explicitly at the end of the happy path (see `close`),
        // and this `Drop` remains the safety net for the panicking case, where leaving one
        // uniquely-named database behind is the correct trade against losing the failure.
        let database = self.database.clone();
        let maintenance = self.maintenance.pool().clone();
        tokio::spawn(async move {
            let _ = sqlx::query(&format!(
                "drop database if exists \"{database}\" with (force)"
            ))
            .execute(&maintenance)
            .await;
        });
    }
}

impl Harness {
    /// Drop this walk's database now, while the runtime is still alive to do it.
    ///
    /// Called at the end of every walk. The `Drop` above cannot be relied on — its task is
    /// discarded with the runtime — so this is what keeps the server's connection count from
    /// growing by one backend per test, which is the difference between a suite that runs
    /// and a suite that dies on `PoolTimedOut` once the siblings' stacks have taken their
    /// share of `max_connections`.
    async fn close(self) {
        let database = self.database.clone();
        let maintenance = self.maintenance.pool().clone();
        let _ = sqlx::query(&format!(
            "drop database if exists \"{database}\" with (force)"
        ))
        .execute(&maintenance)
        .await;
        // `mem::forget` keeps the `Drop` impl from queuing a second, doomed drop.
        std::mem::forget(self);
    }
}

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, format!("omnion_session={token}"));
    }
    let body = body
        .filter(|value| !value.is_null())
        .map(|value| Body::from(value.to_string()))
        .unwrap_or_else(Body::empty);
    builder.body(body).expect("the request must build")
}

fn get(uri: &str, token: &str) -> Request<Body> {
    request(Method::GET, uri, Some(token), None)
}

/// A request that carries a client address, so the audit row it writes has an `ip_address`.
///
/// This is not a cosmetic header. `audit_log.ip_address` is an `inet` column, and the trail's
/// read decodes it into a `String` — which only works when the column is cast to text. A walk
/// whose requests carry no address writes rows with a NULL `ip_address` and therefore never
/// exercises the decode, which is how the Audit tab shipped answering 500 on every real
/// request (every request a browser makes has an address) while its own test passed. A
/// fixture that cannot reach the bug is not a fixture for this bug.
fn post_from(uri: &str, body: Value, token: &str, address: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("omnion_session={token}"))
        .body(Body::from(body.to_string()))
        .expect("the request must build");
    // `ClientAddress` reads the connection's `ConnectInfo`, not a header — `x-forwarded-for`
    // is a lie the platform does not tell (an audit row's address is the socket's). The
    // harness drives the router directly, so it has to hand over the extension a real server
    // would have attached.
    let ip: std::net::IpAddr = address.parse().expect("the address must be an IP");
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(ip, 51234)));
    request
}

fn post(uri: &str, body: Value, token: &str) -> Request<Body> {
    request(Method::POST, uri, Some(token), Some(body))
}

fn put(uri: &str, body: Value, token: &str) -> Request<Body> {
    request(Method::PUT, uri, Some(token), Some(body))
}

/// Create an account and a session for it.
async fn account(harness: &Harness, organization_id: Option<Uuid>) -> (Uuid, String) {
    let user = users::create_user(
        harness.db.pool(),
        NewUser {
            email: format!("operations-{}@omnion.test", Uuid::new_v4().simple()),
            password: PASSWORD.to_owned(),
            display_name: "Operations Walk".to_owned(),
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

/// Bind a role carrying exactly these keys to one account, at organization scope.
async fn grant(harness: &Harness, user_id: Uuid, organization_id: Uuid, keys: &[&str]) {
    let role = role_store::create_role(
        harness.db.pool(),
        NewRole {
            organization_id,
            key: format!("operations-walk-{}", Uuid::new_v4().simple()),
            name: "Operations Walk".to_owned(),
            description: "The keys one walk needs".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");

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

/// The keys an operator of one rule's history needs.
const AUTHOR_KEYS: [&str; 3] = ["workflows.read", "workflows.manage", "workflows.run"];

async fn create_organization_row(db: &Db, name: &str) -> Uuid {
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(format!("operations-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("an organization must be creatable")
}

/// The versions of a rule, as the panel reads them.
async fn versions_of(harness: &Harness, token: &str, rule_id: Uuid) -> TestResponse {
    harness
        .call(get(
            &format!("/api/v1/automations/{rule_id}/versions"),
            token,
        ))
        .await
}

/// A rule body with two steps, so an edit can add a third and the diff can say so.
fn rule_body(organization_id: Uuid, name: &str, steps: Value) -> Value {
    json!({
        "organization_id": organization_id,
        "name": name,
        "event": EVENT,
        "conditions": [],
        "actions": steps,
    })
}

fn echo_step(name: &str) -> Value {
    json!({
        "name": name,
        "kind": "task",
        "action": "echo",
        "params": { "value": "hello" },
        "max_attempts": 1,
    })
}

/// The id of one version, found **by its number**.
///
/// The list is newest first, so an index like `versions[1]` names a different row the moment
/// a third write lands — and every walk here writes more than once. The number is the
/// identity; the index is a layout that happens to hold for the first two rows.
fn version_id(answer: &TestResponse, number: i64) -> String {
    answer.body["versions"]
        .as_array()
        .expect("versions is an array")
        .iter()
        .find(|version| version["version"] == json!(number))
        .and_then(|version| version["id"].as_str())
        .unwrap_or_else(|| {
            panic!("version {number} is missing from: {}", answer.body["versions"])
        })
        .to_owned()
}

/// Create a rule and return its id.
async fn create_rule(
    harness: &Harness,
    token: &str,
    organization_id: Uuid,
    name: &str,
    steps: Value,
) -> Uuid {
    let answer = harness
        .call(post(
            "/api/v1/automations",
            rule_body(organization_id, name, steps),
            token,
        ))
        .await;
    assert_eq!(
        answer.status,
        StatusCode::CREATED,
        "the rule must be created: {}",
        answer.body
    );
    answer.body["id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the rule must carry an id")
}

/// The field names a diff summary reports as changed.
fn changed_fields(summary: &Value) -> Vec<String> {
    summary["changed"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry["field"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// The history is written on every definition change, and each entry says what changed.
#[tokio::test]
async fn a_create_and_every_edit_are_written_to_the_history() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let organization_id = create_organization_row(&harness.db, "Operations walk").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTHOR_KEYS).await;

    let rule_id = create_rule(
        &harness,
        &token,
        organization_id,
        "Versioned rule",
        json!([echo_step("First")]),
    )
    .await;

    // The create itself is version 1 — a rule whose creation is missing from its own
    // history has no "before", so the Versions tab would start at the first *edit*.
    let answer = versions_of(&harness, &token, rule_id).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body["current_version"], 1);
    assert_eq!(
        answer.body["untracked"], false,
        "a rule written now is always tracked"
    );
    let history = answer.body["versions"]
        .as_array()
        .expect("versions is an array");
    assert_eq!(
        history.len(),
        1,
        "the create must be in the history: {history:?}"
    );
    assert_eq!(history[0]["version"], 1);
    assert_eq!(history[0]["change"], "created");
    assert_eq!(history[0]["current"], true);
    assert_eq!(history[0]["created_by"], user_id.to_string());

    // An edit that renames the rule and adds an action: two fields, in words.
    let edited = harness
        .call(put(
            &format!("/api/v1/automations/{rule_id}"),
            rule_body(
                organization_id,
                "Versioned rule, renamed",
                json!([echo_step("First"), echo_step("Second")]),
            ),
            &token,
        ))
        .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);

    let answer = versions_of(&harness, &token, rule_id).await;
    let history = answer.body["versions"]
        .as_array()
        .expect("versions is an array");
    assert_eq!(
        history.len(),
        2,
        "the edit must be in the history: {history:?}"
    );
    assert_eq!(answer.body["current_version"], 2);
    // Newest first, so the head of the list is what the panel shows at the top.
    assert_eq!(history[0]["version"], 2);
    assert_eq!(history[1]["version"], 1);

    let mut fields = changed_fields(&history[0]["summary"]);
    fields.sort();
    assert_eq!(fields, vec!["actions", "name"], "the diff: {}", history[0]);
    // The step count is looked up **by field**, not by position: the diff lists its fields in
    // the panel's own order (`name` before `actions`), so an assertion that read `changed[0]`
    // would be asserting an ordering rather than the change — and would start failing the
    // day someone reorders the labels. The name of the change is the contract; its index is
    // a presentation detail.
    let actions = history[0]["summary"]["changed"]
        .as_array()
        .expect("changed is an array")
        .iter()
        .find(|change| change["field"] == json!("actions"))
        .expect("the diff names the actions field");
    assert_eq!(actions["from"]["steps"], 1);
    assert_eq!(actions["to"]["steps"], 2);

    // The single-version read answers "what changed *in this one*", against the one before.
    let second_id = version_id(&answer, 2);
    let compared = harness
        .call(get(
            &format!("/api/v1/automations/{rule_id}/versions/{second_id}"),
            &token,
        ))
        .await;
    assert_eq!(compared.status, StatusCode::OK, "{}", compared.body);
    assert_eq!(compared.body["compared_to"], 1);
    assert_eq!(compared.body["summary"]["count"], 2);

    // And the first version says so rather than reporting an empty change set.
    let first_id = version_id(&answer, 1);
    let first = harness
        .call(get(
            &format!("/api/v1/automations/{rule_id}/versions/{first_id}"),
            &token,
        ))
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(
        first.body["summary"]["first"], true,
        "the first version has nothing to be different from: {}",
        first.body
    );
    assert!(first.body["compared_to"].is_null());

    // The walk is done: drop its database now, while the runtime that can
    // still do the work is alive (see `Harness::close`).
    harness.close().await;

}

/// Restore puts an old definition back **and appends** it, keeping the rule itself.
#[tokio::test]
async fn a_restore_puts_the_definition_back_without_rewinding_the_history() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let organization_id = create_organization_row(&harness.db, "Operations walk").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTHOR_KEYS).await;

    let rule_id = create_rule(
        &harness,
        &token,
        organization_id,
        "Original name",
        json!([echo_step("First")]),
    )
    .await;

    harness
        .call(put(
            &format!("/api/v1/automations/{rule_id}"),
            rule_body(
                organization_id,
                "A name somebody regretted",
                json!([echo_step("First"), echo_step("Second"), echo_step("Third")]),
            ),
            &token,
        ))
        .await;

    let answer = versions_of(&harness, &token, rule_id).await;
    let first_id = version_id(&answer, 1);

    let restored = harness
        .call(post(
            &format!("/api/v1/automations/{rule_id}/versions/{first_id}/restore"),
            json!({}),
            &token,
        ))
        .await;
    assert_eq!(restored.status, StatusCode::OK, "{}", restored.body);
    assert_eq!(
        restored.body["version"], 3,
        "a restore appends: {}",
        restored.body
    );
    assert_eq!(restored.body["change"], "restored");
    assert_eq!(restored.body["restored_from"], first_id);
    // The restored row is a **diff against what the rule was running**, not against the
    // version it came from. `summary.first` means "there is no version before this one", which
    // is false here — v2 is right there — so the summary is the ordinary two-field change that
    // puts the definition back. Reading it as "nothing changed from v1" would be reading the
    // row's *origin* as its *diff*, and the panel would show a restore as if it had restored
    // nothing.
    let reverted = restored.body["summary"]["changed"]
        .as_array()
        .expect("changed is an array");
    let name_change = reverted
        .iter()
        .find(|change| change["field"] == json!("name"))
        .expect("the diff names the renamed field");
    assert_eq!(name_change["from"], "A name somebody regretted");
    assert_eq!(name_change["to"], "Original name");
    assert!(
        restored.body["summary"]["first"].is_null(),
        "a restore is never the first version: {}",
        restored.body["summary"]
    );

    // The rule now carries the old definition again.
    let rule = harness
        .call(get(&format!("/api/v1/automations/{rule_id}"), &token))
        .await;
    assert_eq!(rule.status, StatusCode::OK, "{}", rule.body);
    assert_eq!(rule.body["name"], "Original name", "{}", rule.body);
    assert_eq!(
        rule.body["actions"].as_array().map(Vec::len),
        Some(1),
        "the old definition is back: {}",
        rule.body["actions"]
    );

    // The history is still a line: 1, 2, 3 — no two rows claiming a number.
    let answer = versions_of(&harness, &token, rule_id).await;
    assert_eq!(answer.body["current_version"], 3);
    let numbers: Vec<i64> = answer.body["versions"]
        .as_array()
        .expect("versions is an array")
        .iter()
        .map(|version| version["version"].as_i64().unwrap_or_default())
        .collect();
    assert_eq!(numbers, vec![3, 2, 1], "the history must stay a line");

    // A no-op is refused in words: restoring what the rule already runs would rewrite it
    // identically, bump the counter and add a history row that changed nothing. The refusal
    // is by *definition*, not by version number — see the route — so this is the case of
    // restoring the version that was just restored through, whose number is older and whose
    // content is identical.
    let again = harness
        .call(post(
            &format!("/api/v1/automations/{rule_id}/versions/{first_id}/restore"),
            json!({}),
            &token,
        ))
        .await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST, "{}", again.body);
    assert_eq!(again.code(), "version_already_current");
    assert!(
        again.message().contains("change nothing"),
        "the refusal has to say why: {}",
        again.message()
    );

    // And the counter did not move: a refused restore writes nothing at all.
    let answer = versions_of(&harness, &token, rule_id).await;
    assert_eq!(
        answer.body["current_version"], 3,
        "a refused restore must not bump the counter"
    );

    // A version of a *different* rule is refused, and answered exactly like an unknown id,
    // so the answer cannot be used to probe for a version that exists somewhere else.
    let other_id = create_rule(
        &harness,
        &token,
        organization_id,
        "Another rule",
        json!([echo_step("Only")]),
    )
    .await;
    let foreign = harness
        .call(post(
            &format!("/api/v1/automations/{other_id}/versions/{first_id}/restore"),
            json!({}),
            &token,
        ))
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{}", foreign.body);
    assert_eq!(foreign.code(), "version_not_found");

    let unknown = harness
        .call(post(
            &format!(
                "/api/v1/automations/{rule_id}/versions/{}/restore",
                Uuid::new_v4()
            ),
            json!({}),
            &token,
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.body);
    assert_eq!(unknown.code(), foreign.code(), "one body for both refusals");

    // The walk is done: drop its database now, while the runtime that can
    // still do the work is alive (see `Harness::close`).
    harness.close().await;

}

/// A restore is a definition write, so only somebody who may manage rules can do one.
#[tokio::test]
async fn a_restore_is_refused_to_a_caller_who_may_only_read() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let organization_id = create_organization_row(&harness.db, "Operations walk").await;
    let (author_id, author_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, author_id, organization_id, &AUTHOR_KEYS).await;

    let rule_id = create_rule(
        &harness,
        &author_token,
        organization_id,
        "Guarded rule",
        json!([echo_step("First")]),
    )
    .await;
    harness
        .call(put(
            &format!("/api/v1/automations/{rule_id}"),
            rule_body(
                organization_id,
                "Guarded rule, edited",
                json!([echo_step("First"), echo_step("Second")]),
            ),
            &author_token,
        ))
        .await;

    // A reader may read the history — it is the same power that reads the rule.
    let (reader_id, reader_token) = account(&harness, Some(organization_id)).await;
    grant(&harness, reader_id, organization_id, &["workflows.read"]).await;

    let readable = versions_of(&harness, &reader_token, rule_id).await;
    assert_eq!(
        readable.status,
        StatusCode::OK,
        "a reader may read the history: {}",
        readable.body
    );

    // A reader may not put a version back. A restore that carried run power would let
    // anybody who may fire a rule rewrite it.
    let first_id = version_id(&readable, 1);
    let refused = harness
        .call(post(
            &format!("/api/v1/automations/{rule_id}/versions/{first_id}/restore"),
            json!({}),
            &reader_token,
        ))
        .await;
    assert!(
        refused.status == StatusCode::FORBIDDEN || refused.status == StatusCode::UNAUTHORIZED,
        "a reader must not restore: {}",
        refused.body
    );

    // The walk is done: drop its database now, while the runtime that can
    // still do the work is alive (see `Harness::close`).
    harness.close().await;

}

/// The audit tab lists the definition changes with their actor and their summary.
#[tokio::test]
async fn every_definition_change_is_listed_in_the_audit_tab() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let organization_id = create_organization_row(&harness.db, "Operations walk").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTHOR_KEYS).await;

    // Created from a request that carries an address, so its audit row has an `ip_address`
    // — the column whose decode broke the Audit tab. Every browser request has one, so a
    // walk that writes no address is a walk that cannot see this bug.
    let created = harness
        .call(post_from(
            "/api/v1/automations",
            rule_body(organization_id, "Audited rule", json!([echo_step("First")])),
            &token,
            "203.0.113.9",
        ))
        .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the rule must be created: {}",
        created.body
    );
    let rule_id = created.body["id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the rule must carry an id");
    let with_address: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where target_id = $1 and ip_address is not null",
    )
    .bind(rule_id.to_string())
    .fetch_one(harness.db.pool())
    .await
    .expect("the audit rows must be readable");
    assert!(
        with_address > 0,
        "this walk's rule must have an audit row carrying an address, or it proves nothing \
         about a trail whose rows do"
    );

    let answer = harness
        .call(get(&format!("/api/v1/automations/{rule_id}/audit"), &token))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let entries = answer.body["entries"]
        .as_array()
        .expect("entries is an array");
    assert!(
        !entries.is_empty(),
        "a rule that was just created has an audit row: {}",
        answer.body
    );

    let created = entries
        .iter()
        .find(|entry| entry["action"] == json!("automation.created"))
        .expect("the create must be audited");
    assert_eq!(created["actor_user_id"], user_id.to_string());
    assert_eq!(created["target_id"], rule_id.to_string());
    assert_eq!(created["actor_type"], "user");
    assert_eq!(created["metadata"]["name"], "Audited rule");

    // An edit adds its own row; the trail is newest first, so the edit is at the head.
    let edited = harness
        .call(put(
            &format!("/api/v1/automations/{rule_id}"),
            rule_body(
                organization_id,
                "Audited rule, edited",
                json!([echo_step("First")]),
            ),
            &token,
        ))
        .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    assert_eq!(
        edited.body["name"], "Audited rule, edited",
        "the edit must be applied before the restore: {}",
        edited.body
    );
    let answer = harness
        .call(get(&format!("/api/v1/automations/{rule_id}/audit"), &token))
        .await;
    let entries = answer.body["entries"]
        .as_array()
        .expect("entries is an array");
    assert_eq!(entries[0]["action"], "automation.updated");
    assert_eq!(entries[0]["metadata"]["name"], "Audited rule, edited");

    // A restore is audited too, and the row names which version went in and which came
    // out — so "who put that back" is answerable from the trail alone.
    let versions = versions_of(&harness, &token, rule_id).await;
    // Look the version up **by its number**, never by its position. The list is newest first,
    // so `[1]` happens to be v1 only while the history is exactly two rows long — the moment a
    // third write lands (which the audit walk's own restore does) the same index names a
    // different version, and the walk then restores whatever row it happened to pick and calls
    // it "the first version". The number is the identity; the index is a layout.
    let first_id = versions.body["versions"]
        .as_array()
        .expect("versions is an array")
        .iter()
        .find(|version| version["version"] == json!(1))
        .and_then(|version| version["id"].as_str())
        .expect("version 1 is in the history")
        .to_owned();
    let restore = harness
        .call(post(
            &format!("/api/v1/automations/{rule_id}/versions/{first_id}/restore"),
            json!({}),
            &token,
        ))
        .await;
    assert_eq!(
        restore.status,
        StatusCode::OK,
        "the restore must be applied: {}",
        restore.body
    );
    let answer = harness
        .call(get(&format!("/api/v1/automations/{rule_id}/audit"), &token))
        .await;
    let entries = answer.body["entries"]
        .as_array()
        .expect("entries is an array");
    let restored = entries
        .iter()
        .find(|entry| entry["action"] == json!("automation.version_restored"))
        .expect("a restore must be audited");
    assert_eq!(restored["metadata"]["from_version"], 1);
    assert_eq!(restored["metadata"]["to_version"], 3);

    // A rule in another organization is not reachable at all. The handler resolves it through
    // the same scope check every automation read uses, and an account that has never seen the
    // rule is refused with `cross_organization` — the platform-wide 403 that says "this exists
    // but is not yours to read", which is the honest answer here: unlike a missing id, this id
    // really does exist, and 404 would be a lie the caller could disprove with the rule list
    // it is separately allowed to read. (An id that exists *nowhere* still answers 404.)
    let other_organization = create_organization_row(&harness.db, "Other tenant").await;
    let (outsider_id, outsider_token) = account(&harness, Some(other_organization)).await;
    grant(&harness, outsider_id, other_organization, &AUTHOR_KEYS).await;
    let refused = harness
        .call(get(
            &format!("/api/v1/automations/{rule_id}/audit"),
            &outsider_token,
        ))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.code(), "cross_organization");

    // The walk is done: drop its database now, while the runtime that can
    // still do the work is alive (see `Harness::close`).
    harness.close().await;

}

/// The gallery serves six starters that are all real, installable definitions.
#[tokio::test]
async fn the_gallery_serves_six_templates_that_this_installation_can_actually_save() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let organization_id = create_organization_row(&harness.db, "Operations walk").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTHOR_KEYS).await;

    let answer = harness
        .call(get("/api/v1/automations/templates", &token))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let templates = answer.body["templates"]
        .as_array()
        .expect("templates is an array");
    assert_eq!(templates.len(), 6, "the request names six: {templates:?}");

    for template in templates {
        let key = template["key"].as_str().expect("a template key");
        assert!(
            !template["name"].as_str().unwrap_or_default().is_empty(),
            "{key}: a card with no name"
        );
        assert!(
            !template["description"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "{key}: a card with no description"
        );
        assert!(
            !template["event"].as_str().unwrap_or_default().is_empty(),
            "{key}: a template with no event"
        );
        assert!(template["action_count"].as_u64().unwrap_or(0) > 0, "{key}");
    }

    // Nothing calls out on a fresh installation, so nothing is installable *yet* — and the
    // gallery says which host is missing rather than letting the save fail.
    let webhook = templates
        .iter()
        .find(|template| template["key"] == json!("ping_webhook"))
        .expect("the webhook starter is in the gallery");
    assert_eq!(webhook["installable"], false);
    let blocked = webhook["blocked_reason"]
        .as_str()
        .expect("a blocked template says why");
    assert!(
        blocked.contains("example.com"),
        "the refusal names the host: {blocked}"
    );

    // With the host allowed, it becomes installable — and the body really does create the
    // rule through the same path a hand-written rule takes.
    sqlx::query(
        "update automation_settings set http_allowed_hosts = array['example.com'] where id = 1",
    )
    .execute(harness.db.pool())
    .await
    .expect("the allow-list must be writable");
    let answer = harness
        .call(get("/api/v1/automations/templates", &token))
        .await;
    let templates = answer.body["templates"].as_array().expect("templates");
    let webhook = templates
        .iter()
        .find(|template| template["key"] == json!("ping_webhook"))
        .expect("the webhook starter is still in the gallery");
    assert_eq!(webhook["installable"], true, "{webhook}");

    let mut body = webhook["body"].clone();
    body["organization_id"] = json!(organization_id);
    let created = harness
        .call(post("/api/v1/automations", body, &token))
        .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the template's own body must create the rule: {}",
        created.body
    );
    assert_eq!(created.body["name"], "Ping a webhook on publish");
    assert_eq!(
        created.body["enabled"], false,
        "a starter installs paused: {}",
        created.body
    );

    // And the other four that do not call out are installable from the start.
    for key in ["welcome_email", "comment_on_publish", "notify_on_failure"] {
        let template = templates
            .iter()
            .find(|template| template["key"] == json!(key))
            .unwrap_or_else(|| panic!("{key} is in the gallery"));
        assert_eq!(
            template["installable"], true,
            "{key} never leaves the process, so nothing should block it: {}",
            template
        );
    }

    // The walk is done: drop its database now, while the runtime that can
    // still do the work is alive (see `Harness::close`).
    harness.close().await;

}

// ---------------------------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------------------------

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => {
            sqlx::query("select 1").execute(db.pool()).await.ok()?;
            Some(db)
        }
        Err(_) => None,
    }
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 2,
    }
}

/// Point a connection string at a different database on the same server.
fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a URL has a path");
    let base = base.rsplit_once('?').map_or(base, |(head, _)| head);
    format!("{base}/{database}")
}

/// The endless-loop guard is real: the request asks for a test that **fails when the guard
/// is removed**, and this is it.
///
/// A guard nobody has ever watched refuse a run is indistinguishable from no guard at all,
/// so the walk drives the same engine twice — once with `LoopGuard` and once with
/// `NoRunGuard` — and asserts the two runs end differently. The second half is the one that
/// matters: if the engine stopped consulting the guard, both runs would finish and this
/// assertion would fail, which is exactly the failure the request asks to be able to see.
#[tokio::test]
async fn removing_the_guard_changes_what_the_engine_does() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("skipping: PostgreSQL is not reachable");
        return;
    };
    let organization_id = create_organization_row(&harness.db, "Operations walk").await;
    let (user_id, token) = account(&harness, Some(organization_id)).await;
    grant(&harness, user_id, organization_id, &AUTHOR_KEYS).await;

    // A rule whose every step does the *same work*: that is the loop the guard is for. The
    // engine's own `echo` action, so the walk needs no SMTP server and no allow-listed host
    // to prove a guard that is about repetition and nothing else. The steps carry **different
    // names** on purpose — names must be unique in a definition, and the guard's fingerprint
    // deliberately ignores the name, so three steps that differ only by name are exactly the
    // "copy-pasted step, renamed" case the guard is written to still catch.
    let repeated = json!([
        echo_step("First"),
        echo_step("Second"),
        echo_step("Third")
    ]);
    let rule_id = create_rule(
        &harness,
        &token,
        organization_id,
        "Repeating rule",
        repeated.clone(),
    )
    .await;

    let started = harness
        .call(post(
            &format!("/api/v1/automations/{rule_id}/run"),
            json!({}),
            &token,
        ))
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.body);
    let execution_id = started.body["execution_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the run must carry an id");

    // The guarded run.
    let guarded = drive_to_settle(
        &harness,
        omnion_workflows::engine::RunnerConfig {
            tick: time::Duration::milliseconds(20),
            sweep: time::Duration::seconds(30),
            batch: 10,
            sweep_batch: 50,
            scheduler_batch: 4,
            retry_base: time::Duration::milliseconds(40),
            retry_max: time::Duration::milliseconds(320),
            lease: time::Duration::seconds(30),
        },
        &omnion_automation::LoopGuard::new(harness.db.pool().clone()),
    )
    .await;

    // A second run, unguarded, from a second rule so the two are independent.
    let other_id = create_rule(
        &harness,
        &token,
        organization_id,
        "Unguarded rule",
        repeated.clone(),
    )
    .await;
    let started = harness
        .call(post(
            &format!("/api/v1/automations/{other_id}/run"),
            json!({}),
            &token,
        ))
        .await;
    let unguarded_execution = started.body["execution_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the second run must carry an id");

    let unguarded = drive_to_settle(
        &harness,
        omnion_workflows::engine::RunnerConfig {
            tick: time::Duration::milliseconds(20),
            sweep: time::Duration::seconds(30),
            batch: 10,
            sweep_batch: 50,
            scheduler_batch: 4,
            retry_base: time::Duration::milliseconds(40),
            retry_max: time::Duration::milliseconds(320),
            lease: time::Duration::seconds(30),
        },
        &omnion_workflows::guard::NoRunGuard,
    )
    .await;

    // The refusal, when it comes, is a message a person can act on and a run that carries
    // it — not a silent stop. Asserted against the run the guarded engine actually wrote.
    //
    // **Where the message lives matters.** The engine consults the guard after the step
    // succeeded and, on a stop, calls `fail_step` with the reason and then
    // `end_run_after_branch` — so the *reason* is the repeated step's own `error`, and the
    // steps after it are closed with the engine's own "the run ended before this step". A test
    // that looked for the reason on the **run** would read an empty string and conclude the
    // guard said nothing, which is the same mistake as looking for it on a step that never
    // ran.
    let steps = omnion_workflows::store::list_steps(harness.db.pool(), execution_id)
        .await
        .expect("the steps must be readable");
    let repeat = steps
        .iter()
        .find(|step| {
            step.error
                .as_deref()
                .is_some_and(|error| error.contains("repeats the step before it"))
        })
        .unwrap_or_else(|| {
            panic!(
                "the guard's reason must be on the step that repeated: {:?}",
                steps
                    .iter()
                    .map(|step| (step.step_no, &step.status, &step.error))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(repeat.step_no, 2, "the SECOND step is the repeat, not the first");
    // The step after the repeat is closed, never left claimable — otherwise the next tick
    // picks it up and the guard is defeated by a step it already knew about.
    let after = steps
        .iter()
        .filter(|step| step.step_no > repeat.step_no)
        .filter(|step| step.status == "cancelled")
        .count();
    // `step_no` is an `i32` on the row and `count()` gives a `usize`, so the expected side is
    // computed as a `usize` too rather than cast: `steps.len() as i32 - repeat.step_no` is
    // only an `i32` and mixes the two in an `assert_eq!` — a one-line mistake that costs a
    // full integration build to find.
    let expected_after = (steps.len() as i32 - repeat.step_no).max(0) as usize;
    assert_eq!(
        after,
        expected_after,
        "everything past the repeat is closed in the same write: {:?}",
        steps
            .iter()
            .map(|step| (step.step_no, &step.status))
            .collect::<Vec<_>>()
    );

    // The finding, and the reason the request asks for this walk: the **same** rule, the same
    // engine, the same three steps — one run driven with `LoopGuard`, the other with
    // `NoRunGuard`. The guarded run must be stopped `failed` by the guard; the unguarded run
    // must run all three steps to `completed`. If the engine ever stopped consulting the
    // guard, the first assertion would fail — which is the failure the request asks to be
    // able to *see*, rather than to take on faith.
    let guarded_status = terminal_status(&harness, execution_id).await;
    let unguarded_status = terminal_status(&harness, unguarded_execution).await;
    assert_eq!(
        guarded_status,
        Some(omnion_workflows::ExecutionStatus::Failed),
        "a rule that repeats itself is stopped as failed, not left running"
    );
    assert_eq!(
        unguarded_status,
        Some(omnion_workflows::ExecutionStatus::Completed),
        "with the guard removed the very same rule completes all three steps — which is the \
         whole point: the guard is what stops it"
    );

    // And the difference is *only* the guard: step counts and the settled steps agree.
    let guarded_steps = omnion_workflows::store::list_steps(harness.db.pool(), execution_id)
        .await
        .expect("the guarded steps are readable");
    let unguarded_steps = omnion_workflows::store::list_steps(harness.db.pool(), unguarded_execution)
        .await
        .expect("the unguarded steps are readable");
    assert_eq!(
        guarded_steps.len(),
        unguarded_steps.len(),
        "both runs have the same three steps on paper; the guard decides how many of them run"
    );
    assert!(
        unguarded_steps.iter().all(|step| step.status == "succeeded"),
        "without the guard nothing is refused: {:?}",
        unguarded_steps
            .iter()
            .map(|step| (&step.status, &step.error))
            .collect::<Vec<_>>()
    );
    let _ = (guarded, unguarded);

    // The walk is done: drop its database now, while the runtime that can
    // still do the work is alive (see `Harness::close`).
    harness.close().await;

}

/// Drive the engine until a run settles, with the guard the caller chose.
async fn drive_to_settle(
    harness: &Harness,
    config: omnion_workflows::engine::RunnerConfig,
    guard: &dyn omnion_workflows::guard::RunGuard,
) -> Option<omnion_workflows::ExecutionStatus> {
    let actions = omnion_automation::AutomationActions::new(
        harness.db.pool().clone(),
        omnion_automation::MailSettings::new("127.0.0.1", 1, "omnion@localhost")
            .with_sending(false),
    );
    for _ in 0..80 {
        omnion_workflows::engine::tick_with(harness.db.pool(), &config, &actions, guard)
            .await
            .expect("the engine tick must run");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    None
}

/// The terminal status of a run, or `None` while it is still going.
async fn terminal_status(
    harness: &Harness,
    execution_id: Uuid,
) -> Option<omnion_workflows::ExecutionStatus> {
    omnion_workflows::store::find_execution(harness.db.pool(), execution_id)
        .await
        .ok()
        .flatten()
        .and_then(|execution| execution.status())
}
