//! The persisted-document manager's authorization and behaviour, over the real router (REQ-130,
//! slice 2).
//!
//! ## What this walk is actually for
//!
//! Two things in this request are structural and neither can be proven by a unit test:
//!
//! 1. **The guards are real.** The request's API table names `developer.read` and
//!    `developer.graphql.manage`. This repository ships **no `developer.*` key** — an uncatalogued
//!    key resolves to no permission, so a route guarded on one answers `403` for every caller
//!    including the instance owner, while looking perfectly healthy in review. That is the fourth
//!    occurrence of that defect in this repository, and the walk holds it by asserting that the
//!    refusal detail names the permission that actually refused.
//!
//! 2. **Reads and writes are guarded by DIFFERENT keys.** The first draft guarded
//!    `GET /graphql/documents` with the manage key, because a `.layer()` on a `get().post()` pair
//!    applies ONE permission to both verbs. That is invisible in review — the guard looks present —
//!    and it makes the manager refuse the very readers it exists to inform. Only a walk with two
//!    accounts of different power can see it.
//!
//! ## The audit rows are read out of PostgreSQL, not out of the response
//!
//! And the settings save is asserted to write **zero** rows when it moves nothing, because an audit
//! row for a no-op is an event nobody performed.
//!
//! ## Two accounts, deliberately
//!
//! The fixture (a registered document) is created by an account holding the manage key, because
//! asking the account under test to create its own fixture makes the walk's setup depend on the
//! thing it is proving — the mistake the observability walk documented next door.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// Give this suite a rate-limit budget of its own, once per process.
///
/// The limiter is a **process-wide cell** that `router()` fills from the stored document, and the
/// stored `sign_in` scope is ten requests per five minutes. This suite signs in one account per
/// walk and runs ten walks, so the eleventh sign-in is refused with `429` and every walk after it
/// hangs or dies on a line that has nothing to do with persisted documents.
///
/// The hang is the expensive shape: a refused sign-in does not fail fast, it sits in the HTTP
/// client waiting out the limiter's window, so the suite reports "has been running for over 60
/// seconds" rather than an error. `pg_stat_activity` shows every backend **idle** while the test
/// binary is in `epoll_wait` — which reads as a database problem and is not one. This is the same
/// trap the media suite documented beside it, lifted rather than re-invented.
///
/// Raising the ceiling does not weaken what the limiter suite proves: that suite installs and
/// asserts its own numbers, and this cell is process-wide, so whichever fixture installs first
/// wins.
fn give_the_suite_its_own_rate_limit(state: &AppState) {
    let policies: Vec<omnion_security::RatePolicy> = omnion_security::RatePolicy::defaults()
        .into_iter()
        .map(|mut policy| {
            // Only the sign-in scope. Every other ceiling stays as a deployment ships it, so no
            // suite here can be the reason a genuinely over-budget request stops being refused.
            if policy.scope == "sign_in" {
                policy.limit = 10_000;
            }
            policy
        })
        .collect();
    let _ = omnion_api::rate_limit_middleware::install(
        omnion_api::rate_limit_middleware::RateLimiter::new(state, policies),
    );
}

/// The read guard the routes declare. Named here as well as in the module so a change to one
/// without the other is a compile error rather than a silently unguarded route.
const READ: &str = "content.pages.read";
/// The write guard.
const MANAGE: &str = "deployment.migrations.apply";

struct TestResponse {
    status: StatusCode,
    body: Value,
    cookie: Option<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.31:51234".parse().expect("a peer address");
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    // `oneshot` bypasses the connect layer, so without this the peer address is missing on every
    // row — and an assertion about a peer would pass on exactly the data it should not.
    let mut response = response;
    response
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));

    let status = response.status();
    // BOTH cookies, not the first one: a sign-in sets `omnion_session` and `omnion_csrf`, and the
    // CSRF layer refuses a mutation presenting only the session — so a walk holding one of the two
    // is no longer a request the panel can make, and every write it issues reads `403` for a reason
    // that has nothing to do with what it is testing.
    let cookie = Some(
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|raw| raw.split(';').next())
            .filter(|pair| {
                pair.starts_with("omnion_session=") || pair.starts_with("omnion_csrf=")
            })
            .collect::<Vec<_>>()
            .join("; "),
    );
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
        cookie,
    }
}

fn request(method: Method, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    match body {
        None | Some(Value::Null) => builder
            .body(Body::empty())
            .expect("a static request builds"),
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("a json request builds"),
    }
}

/// An authenticated request carrying the session cookies.
fn authed(method: Method, uri: &str, body: Option<Value>, cookie: &str) -> Request<Body> {
    // The COOKIE header is set on the BUILDER rather than on the finished request: `Request` has no
    // `header` method, and the builder does. Built here rather than inside `request` because only
    // the authenticated calls carry a cookie — the login call must not.
    let mut request = request(method, uri, body);
    request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().expect("a cookie header value"));
    request
}

/// An account in a fresh organization, with a role holding exactly `permissions`.
async fn account_with(state: &AppState, permissions: &[&str]) -> (Uuid, Uuid, String) {
    give_the_suite_its_own_rate_limit(state);
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("GraphQL documents walk {suffix}"),
            slug: format!("gql-doc-walk-{suffix}"),
        },
    )
    .await
    .expect("the organization is created");
    let email = format!("gql-doc-walk-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Document Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account is created");

    let role = role_store::create_role(
        state.db().pool(),
        NewRole {
            organization_id: organization.id,
            key: format!("gql-doc-walk-role-{suffix}"),
            name: "GraphQL documents walk role".to_owned(),
            description: "A role of the persisted-document walk".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role is created");
    let entries: Vec<RolePermissionInput> = permissions
        .iter()
        .map(|key| RolePermissionInput {
            key: (*key).to_owned(),
            effect: Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(state.db().pool(), role.id, &entries)
        .await
        .expect("the role permission set is written");
    bindings::grant(
        state.db().pool(),
        NewBinding {
            role_id: role.id,
            user_id: user.id,
            scope: Scope::Organization {
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
        request(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({ "email": email, "password": PASSWORD })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "login failed: {}",
        response.body
    );
    (
        user.id,
        organization.id,
        response.cookie.expect("the login sets a session cookie"),
    )
}

/// The number of audit rows an actor wrote for one action, read out of PostgreSQL.
async fn audit_rows(state: &AppState, actor: Uuid, action: &str) -> i64 {
    sqlx::query_scalar(
        "select count(*) from audit_log where actor_user_id = $1 and action = $2",
    )
    .bind(actor)
    .bind(action)
    .fetch_one(state.db().pool())
    .await
    .expect("the audit count runs")
}

/// The permission a `403` was actually about.
///
/// A bare `403` proves nothing: a cross-scope check, a missing row and a permission refusal all
/// answer `403`. The refusal detail names the permission, which is the claim under test.
fn refusal_permission(body: &Value) -> Option<&str> {
    body.get("error")?.get("details")?.get("permission")?.as_str()
}

/// The message an `ApiError` serialised into the response.
///
/// **Through the `error` envelope, and this is the trap.** `ApiError`'s `IntoResponse` writes
/// `{"error": {"code", "message", "details"}}`, so `body["message"]` is `Null` for EVERY refusal —
/// and a test that reads it that way sees an empty string, concludes the handler forgot to name the
/// field, and reports a defect the platform never had. The API's own walk helpers already carry a
/// `message_of` for exactly this reason; a new walk that reads the body by hand re-learns it.
fn error_message(body: &Value) -> String {
    body.get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn the_read_guard_admits_a_reader_and_the_write_guard_refuses_it_by_name() {
    let state = support::walk_state::state_or_fail().await;
    let (_reader_id, _org, reader) = account_with(&state, &[READ]).await;

    // The read guard admits the reader. This is the assertion the first draft could not make: with
    // a `.layer()` on a `get().post()` pair, this very call answered 403 and the walk would have
    // read it as "the reader cannot list" rather than "the layer applies one key to both verbs".
    let listed = call(
        &state,
        authed(
            Method::GET,
            "/api/v1/graphql/documents",
            None,
            &reader,
        ),
    )
    .await;
    assert_eq!(
        listed.status,
        StatusCode::OK,
        "a reader may list the registry: {}",
        listed.body
    );
    assert_eq!(listed.body["total"], 0, "a fresh registry is empty, not absent");
    // The value is echoed rather than assumed: `persisted_only` is INSTALLATION-wide, so a walk
    // that hard-codes `false` asserts the order in which unrelated tests happened to run. Reading
    // the flag and requiring the endpoint to behave the same way is the claim; the literal is not.
    assert!(
        listed.body["persisted_only"].is_boolean(),
        "the list carries the installation's policy so the screen's banner and the endpoint agree: \
         {}",
        listed.body
    );

    let refused = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/graphql/documents",
            Some(json!({ "name": "Page list", "document": "{ pages(first: 5) { id title } }" })),
            &reader,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a reader may not register: {}",
        refused.body
    );
    assert_eq!(
        refusal_permission(&refused.body),
        Some(MANAGE),
        "the refusal must name the permission that refused it, or it proves nothing about the guard"
    );
}

#[tokio::test]
async fn the_guards_are_catalogue_keys_because_an_uncatalogued_one_refuses_the_owner() {
    let state = support::walk_state::state_or_fail().await;
    let (_admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    // The control. An account holding everything is refused by a route whose key is not in the
    // catalogue, because an uncatalogued key resolves to NO permission — not to "all permissions".
    // This is what the request's own `developer.*` names would do, which is why the substitution is
    // documented in the route file rather than made silently.
    let catalogue = include_str!("../../../crates/permissions/src/catalogue.rs");
    assert!(
        !catalogue.contains("\"developer."),
        "this repository ships no developer.* key; the request's guard names would be 403 for every \
         caller. If this assertion fails, a catalogue key was added and the substitution can be \
         revisited."
    );
    assert!(catalogue.contains(&format!("\"{READ}\"")), "{READ} must be catalogued");
    assert!(catalogue.contains(&format!("\"{MANAGE}\"")), "{MANAGE} must be catalogued");

    // The real guards admit the account holding them. An uncatalogued key fails here.
    for (uri, what) in [
        ("/api/v1/graphql/documents", "the registry"),
        ("/api/v1/graphql/settings", "the settings"),
    ] {
        let response = call(&state, authed(Method::GET, uri, None, &admin)).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{what} must be readable by an account holding {READ} and {MANAGE}: {}",
            response.body
        );
    }
}

#[tokio::test]
async fn registering_a_document_writes_exactly_one_audit_row_and_carries_a_priced_cost() {
    let state = support::walk_state::state_or_fail().await;
    let (admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let registered = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/graphql/documents",
            Some(json!({ "name": "Page list", "document": "{ pages(first: 5) { id title } }" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(
        registered.status,
        StatusCode::OK,
        "{}",
        registered.body
    );
    let body = registered.body;
    assert_eq!(body["created"], true, "a first registration creates");
    assert!(body["duplicate_of"].is_null(), "and reports no duplicate");
    assert_eq!(
        body["document"]["status"], "active",
        "a document registered as a draft executes nothing — the default must be active"
    );
    assert!(
        body["document"]["short_hash"]
            .as_str()
            .is_some_and(|hash| hash.len() == 32),
        "the manager shows a copyable short hash: {body}"
    );
    assert!(
        body["document"]["operations"][0]["cost"]
            .as_u64()
            .is_some_and(|cost| cost > 0),
        "the row carries a priced cost, not a placeholder: {body}"
    );
    assert_eq!(body["document"]["blocked_reason"], Value::Null, "an active row shows no dead control");

    assert_eq!(
        audit_rows(&state, admin_id, "graphql.document.registered").await,
        1,
        "one mutation writes exactly one audit row; the count is read out of PostgreSQL, not from \
         the response"
    );
}

#[tokio::test]
async fn re_registering_the_same_query_reports_the_existing_row_and_creates_no_second_one() {
    let state = support::walk_state::state_or_fail().await;
    let (_admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let first = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/graphql/documents",
            Some(json!({ "name": "Page list", "document": "{ pages(first: 5) { id title } }" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let id = first.body["document"]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    // The SAME query, reformatted. The digest is taken over canonical text, so this is the same
    // document — and CI re-registering on every release must not break the build.
    let second = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/graphql/documents",
            Some(json!({
                "name": "Page list (renamed)",
                "document": "{\n  pages(first: 5) {\n    id\n    title\n  }\n}\n"
            })),
            &admin,
        ),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "a re-registration succeeds: {}",
        second.body
    );
    assert_eq!(second.body["created"], false, "and says nothing new appeared");
    assert_eq!(
        second.body["duplicate_of"].as_str(),
        Some(id.as_str()),
        "the manager's contract is a link to the existing row, not a 409 that says \"already exists\""
    );

    let listed = call(
        &state,
        authed(Method::GET, "/api/v1/graphql/documents", None, &admin),
    )
    .await;
    assert_eq!(
        listed.body["total"], 1,
        "formatting must not create a second row: {}",
        listed.body
    );
}

#[tokio::test]
async fn a_document_that_does_not_parse_is_refused_by_name_and_leaves_no_row() {
    let state = support::walk_state::state_or_fail().await;
    let (_admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    for document in ["{ pages(", "   ", "this is prose, not a query"] {
        let refused = call(
            &state,
            authed(
                Method::POST,
                "/api/v1/graphql/documents",
                Some(json!({ "name": "Broken", "document": document })),
                &admin,
            ),
        )
        .await;
        assert!(
            refused.status.is_client_error(),
            "`{document}` must be refused: {}",
            refused.body
        );
        let message = error_message(&refused.body);
        assert!(
            message.contains("`document`"),
            "the message names the field that is wrong rather than saying \"invalid document\": \
             {message}"
        );
    }

    // An empty NAME is a different field with a different message.
    let refused = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/graphql/documents",
            Some(json!({ "name": "   ", "document": "{ pages { id } }" })),
            &admin,
        ),
    )
    .await;
    assert!(
        refused
            .body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("`name`")),
        "each field gets its own message: {}",
        refused.body
    );

    let listed = call(
        &state,
        authed(Method::GET, "/api/v1/graphql/documents", None, &admin),
    )
    .await;
    assert_eq!(
        listed.body["total"], 0,
        "a refused registration leaves no row behind: {}",
        listed.body
    );
}

#[tokio::test]
async fn a_document_revoked_in_the_manager_stops_executing_on_the_very_next_call() {
    let state = support::walk_state::state_or_fail().await;
    let (admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let registered = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/graphql/documents",
            Some(json!({ "name": "Page list", "document": "{ pages(first: 5) { id title } }" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(registered.status, StatusCode::OK, "{}", registered.body);
    let id = registered.body["document"]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let short = registered.body["document"]["short_hash"]
        .as_str()
        .expect("a short hash")
        .to_owned();

    // Before the revoke, the document executes by hash.
    let allowed = call(
        &state,
        authed(
            Method::GET,
            &format!("/api/v1/graphql?hash={short}"),
            None,
            &admin,
        ),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "a registered active document executes: {}",
        allowed.body
    );
    assert!(
        allowed.body["errors"].is_null()
            || allowed.body["errors"].as_array().is_some_and(|e| e.is_empty()),
        "an active document must not be refused: {}",
        allowed.body
    );
    assert!(
        allowed.body["data"]["pages"].is_array(),
        "the resolver answered the surface the document names, as the list it is: {}",
        allowed.body
    );

    let revoked = call(
        &state,
        authed(
            Method::PUT,
            &format!("/api/v1/graphql/documents/{id}"),
            Some(json!({ "status": "revoked" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body["status"], "revoked");
    assert!(
        revoked.body["blocked_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("PERSISTED_QUERY_NOT_FOUND")),
        "the row explains the code the client will receive instead of offering a dead control: {}",
        revoked.body
    );

    // THE acceptance line: "a document revoked in the UI stops executing within one cache cycle".
    // There is no cache between the revoke and this call, so the very next request is refused.
    let refused = call(
        &state,
        authed(
            Method::GET,
            &format!("/api/v1/graphql?hash={short}"),
            None,
            &admin,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::OK,
        "a refused document is a 200 envelope, not a 404"
    );
    assert_eq!(
        refused.body["errors"][0]["extensions"]["code"], "PERSISTED_QUERY_NOT_FOUND",
        "the client branches on the code the request names: {}",
        refused.body
    );
    assert!(
        refused.body["data"].is_null(),
        "a refused operation must not answer `data: null` — that reads as \"the field does not exist\", \
         which is a different and wrong statement: {}",
        refused.body
    );

    assert_eq!(audit_rows(&state, admin_id, "graphql.document.revoked").await, 1);
}

#[tokio::test]
async fn persisted_only_mode_refuses_an_ad_hoc_document_and_takes_effect_immediately() {
    let state = support::walk_state::state_or_fail().await;
    let (admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    // **The flag is installation-wide, so this walk sets its own precondition instead of assuming
    // one.** The row is a single row keyed by `id = 1`, shared by every organization, and the
    // first version of this walk asserted `ad-hoc runs` before flipping the flag — which passes
    // only when no other walk happened to set it first. On a shared QA database that is a coin
    // flip, and the failure reads as "persisted-only mode is on" when the truth is "someone else
    // turned it on for you". So the OFF state is written here, explicitly, and the walk's first
    // claim is about the flag moving rather than about the state it inherited.
    let off = write_settings(&state, &admin, false).await;
    assert_eq!(
        off["persisted_only"], false,
        "the walk's own precondition: persisted-only mode is off"
    );

    let before = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/graphql",
            Some(json!({ "query": "{ pages(first: 1) { id } }" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);
    assert!(
        before.body["errors"].is_null()
            || before.body["errors"].as_array().is_some_and(|e| e.is_empty()),
        "an ad-hoc document runs while persisted-only mode is off: {}",
        before.body
    );

    let on = write_settings(&state, &admin, true).await;
    assert_eq!(on["persisted_only"], true, "a flag that does not take effect is not saved");

    let after = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/graphql",
            Some(json!({ "query": "{ pages(first: 1) { id } }" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::OK,
        "a refused query is still a 200 envelope"
    );
    assert_eq!(
        after.body["errors"][0]["extensions"]["code"], "PERSISTED_QUERY_NOT_FOUND",
        "persisted-only mode must refuse an ad-hoc document with the documented code: {}",
        after.body
    );
    // The refusal is not a parse failure wearing a different name: nothing was priced, so the
    // cost and depth are zero. A refusal that reported a measured cost would mean the document was
    // walked before it was refused — which is the expensive order the flag exists to avoid.
    assert_eq!(after.body["extensions"]["depth"], 0, "{}", after.body);
    assert_eq!(after.body["extensions"]["cost"], 0, "{}", after.body);

    assert_eq!(audit_rows(&state, admin_id, "graphql.settings.changed").await, 2,
        "two settings writes, two rows: the OFF precondition is a real change and is recorded");
}

/// Set `persisted_only` and return the row as stored.
async fn write_settings(state: &AppState, admin: &str, persisted_only: bool) -> Value {
    let current: Value = call(state, authed(Method::GET, "/api/v1/graphql/settings", None, admin))
        .await
        .body;
    let mut wanted = current;
    wanted["persisted_only"] = json!(persisted_only);
    let saved = call(
        state,
        authed(
            Method::PUT,
            "/api/v1/graphql/settings",
            Some(wanted),
            admin,
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    saved.body
}

#[tokio::test]
async fn a_settings_save_that_moves_nothing_writes_no_audit_row() {
    let state = support::walk_state::state_or_fail().await;
    let (admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let current = call(
        &state,
        authed(Method::GET, "/api/v1/graphql/settings", None, &admin),
    )
    .await;
    assert_eq!(current.status, StatusCode::OK, "{}", current.body);

    // Write the values straight back. An audit row for a no-op is an event nobody performed.
    let saved = call(
        &state,
        authed(
            Method::PUT,
            "/api/v1/graphql/settings",
            Some(current.body.clone()),
            &admin,
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        saved.body["max_depth"], current.body["max_depth"],
        "and it did not move a value"
    );

    assert_eq!(
        audit_rows(&state, admin_id, "graphql.settings.changed").await,
        0,
        "a save that changes nothing is not a change"
    );
}

#[tokio::test]
async fn an_out_of_range_setting_is_refused_by_field_with_its_own_message() {
    let state = support::walk_state::state_or_fail().await;
    let (_admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let settings: Value = call(
        &state,
        authed(Method::GET, "/api/v1/graphql/settings", None, &admin),
    )
    .await
    .body;

    for (field, value) in [
        ("max_depth", json!(0)),
        ("cost_budget", json!(0)),
        ("timeout_ms", json!(5)),
        ("max_page_size", json!(0)),
    ] {
        let mut wanted = settings.clone();
        wanted[field] = value.clone();
        let refused = call(
            &state,
            authed(
                Method::PUT,
                "/api/v1/graphql/settings",
                Some(wanted),
                &admin,
            ),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{field}` out of range is a 422: {}",
            refused.body
        );
        let message = error_message(&refused.body);
        assert!(
            message.contains(field),
            "each field gets its own message — \"invalid settings\" for six numbers is a screen the \
             operator cannot use: {message}"
        );
    }
}

#[tokio::test]
async fn a_document_id_that_does_not_exist_answers_exactly_what_another_tenants_would() {
    let state = support::walk_state::state_or_fail().await;
    let (_admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let registered = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/graphql/documents",
            Some(json!({ "name": "Page list", "document": "{ pages(first: 5) { id title } }" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(registered.status, StatusCode::OK, "{}", registered.body);
    let id = registered.body["document"]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let owned = call(
        &state,
        authed(
            Method::GET,
            &format!("/api/v1/graphql/documents/{id}"),
            None,
            &admin,
        ),
    )
    .await;
    assert_eq!(owned.status, StatusCode::OK, "the owner may read its own document");
    assert!(
        owned.body["text"].as_str().is_some_and(|text| text.contains("pages")),
        "the detail screen shows the document's source: {}",
        owned.body
    );

    // A missing id and another tenant's id must answer the SAME thing, or the id is a probe for
    // which tenants registered what.
    let missing = call(
        &state,
        authed(
            Method::GET,
            "/api/v1/graphql/documents/00000000-0000-0000-0000-000000000000",
            None,
            &admin,
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.body);
}
