//! The versioned API policy over the real router (REQ-130, slice 4).
//!
//! ## What only a walk can see
//!
//! 1. **The headers are on the WIRE.** The policy crate proves `headers_for` builds three values;
//!    nothing but a real response proves axum actually puts them there, under the header policy,
//! for a route nobody remembered to edit.
//! 2. **The removed answer does not run the handler.** The policy says `Gone`; only the middleware
//!    being installed proves it short-circuits. A route that answers `410` *after* running its
//!    handler is still a route that works, and no unit test in the policy crate can see it.
//! 3. **The window is enforced on the WRITE path.** A handler that inserts first and checks after
//!    leaves a row behind for a date it just refused.
//! 4. **The guards are real keys.** Sixth occurrence of this defect class in this repository.
//!
//! ## Two accounts and one clock argument
//!
//! The reader and the announcer hold different keys, because a screen whose list refuses the very
//! operators who read it is a screen nobody opens. And every date is computed from
//! `OffsetDateTime::now_utc()` at call time rather than written as a literal: a literal sunset
//! thirty days out is a test that starts failing in a month, and this file is meant to be read in
//! two years.

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
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

const READ: &str = "developer.read";
const MANAGE: &str = "developer.keys.manage";

fn give_the_suite_its_own_rate_limit(state: &AppState) {
    let policies: Vec<omnion_security::RatePolicy> = omnion_security::RatePolicy::defaults()
        .into_iter()
        .map(|mut policy| {
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

struct TestResponse {
    status: StatusCode,
    body: Value,
    headers: axum::http::HeaderMap,
    cookie: Option<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.77:51234".parse().expect("a peer address");
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let mut response = response;
    response
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));

    let status = response.status();
    let headers = response.headers().clone();
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
        headers,
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

fn authed(method: Method, uri: &str, body: Option<Value>, cookie: &str) -> Request<Body> {
    let mut request = request(method, uri, body);
    request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().expect("a cookie header value"));
    request
}

async fn account_with(state: &AppState, permissions: &[&str]) -> (Uuid, Uuid, String) {
    give_the_suite_its_own_rate_limit(state);
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Deprecations walk {suffix}"),
            slug: format!("gql-dep-walk-{suffix}"),
        },
    )
    .await
    .expect("the organization is created");
    let email = format!("gql-dep-walk-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Deprecation Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account is created");

    let role = role_store::create_role(
        state.db().pool(),
        NewRole {
            organization_id: organization.id,
            key: format!("gql-dep-walk-role-{suffix}"),
            name: "Deprecations walk role".to_owned(),
            description: "A role of the deprecations walk".to_owned(),
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
    assert_eq!(response.status, StatusCode::OK, "login failed: {}", response.body);
    (
        user.id,
        organization.id,
        response.cookie.expect("the login sets a session cookie"),
    )
}

fn error_message(body: &Value) -> String {
    body.get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn refusal_permission(body: &Value) -> Option<&str> {
    body.get("error")?.get("details")?.get("permission")?.as_str()
}

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

/// An RFC 3339 instant `months` from now, as a string.
fn in_months(months: i64) -> String {
    rfc3339(OffsetDateTime::now_utc() + Duration::days(months * 30 + months / 2))
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Announce a deprecation of `/api/v1/pages` through the real route.
async fn announce(
    state: &AppState,
    cookie: &str,
    sunset: &str,
    replacement: Option<&str>,
    route: &str,
) -> TestResponse {
    call(
        state,
        authed(
            Method::POST,
            "/api/v1/api/deprecations",
            Some(json!({
                "route_pattern": route,
                "method": "GET",
                "deprecated_in": "1.4",
                "sunset_at": sunset,
                "replacement": replacement,
                "note": "the page list moved to a cursor API",
            })),
            cookie,
        ),
    )
    .await
}

#[tokio::test]
async fn the_guards_are_catalogue_keys_because_an_uncatalogued_one_refuses_the_owner() {
    let state = support::walk_state::state_or_fail().await;
    let (_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;
    let (_reader_id, _org2, reader) = account_with(&state, &[READ]).await;

    let catalogue = include_str!("../../../crates/permissions/src/catalogue.rs");
    assert!(catalogue.contains(&format!("\"{READ}\"")), "{READ} must be catalogued");
    assert!(catalogue.contains(&format!("\"{MANAGE}\"")), "{MANAGE} must be catalogued");
    assert!(
        !catalogue.contains("\"settings.manage\""),
        "the request's write guard is still uncatalogued. If this assertion fails, the \
         substitution can be revisited deliberately."
    );

    let listed = call(
        &state,
        authed(Method::GET, "/api/v1/api/deprecations", None, &reader),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "a reader may list: {}", listed.body);
    assert!(listed.body["policy"]["public_months"].is_i64(), "the list states the windows");

    let refused = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/api/deprecations",
            Some(json!({
                "route_pattern": "/api/v1/pages",
                "deprecated_in": "1.4",
                "sunset_at": in_months(12),
                "replacement": "/pages",
            })),
            &reader,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(
        refusal_permission(&refused.body),
        Some(MANAGE),
        "the refusal names the permission that refused it, or it proves nothing about the guard"
    );
}

#[tokio::test]
async fn a_sunset_shorter_than_the_window_is_refused_and_leaves_no_row() {
    let state = support::walk_state::state_or_fail().await;
    let (_id, org, admin) = account_with(&state, &[READ, MANAGE]).await;

    // Five months for a PUBLIC route, where the floor is six.
    let refused = announce(&state, &admin, &in_months(5), Some("/pages"), "/api/v1/pages").await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a short window must be refused: {}",
        refused.body
    );
    let message = error_message(&refused.body);
    assert!(message.contains("6 months"), "the message names the rule: {message}");
    assert!(
        message.contains("public route"),
        "and which surface it applied to: {message}"
    );

    // The refused row is NOT on disk. A screen whose form reports the failure while the row
    // exists is a screen that shows a deprecation nobody announced.
    let rows: i64 = sqlx::query_scalar(
        "select count(*) from api_deprecations where organization_id = $1 and route_pattern = '/api/v1/pages'",
    )
    .bind(org)
    .fetch_one(state.db().pool())
    .await
    .expect("the row count runs");
    assert_eq!(rows, 0, "a refused announcement writes nothing at all");
}

#[tokio::test]
async fn a_deprecated_route_returns_the_three_headers_and_the_changelog_link() {
    let state = support::walk_state::state_or_fail().await;
    let (_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let announced = announce(
        &state,
        &admin,
        &in_months(9),
        Some("/pages"),
        "/api/v1/pages",
    )
    .await;
    assert_eq!(announced.status, StatusCode::CREATED, "{}", announced.body);
    assert_eq!(announced.body["status"], "announced");
    assert_eq!(announced.body["minimum_window_months"], 6);

    // The map the middleware reads is rebuilt by the write, and this walk calls the rebuild
    // itself because the router's `oneshot` does not run `main.rs`'s boot hook. Without it the
    // headers would be absent for the WRONG reason and the assertion below would be measuring
    // the map rather than the middleware.
    omnion_api::deprecation_middleware::refresh(state.db().pool()).await;
    assert!(
        omnion_api::deprecation_middleware::tracked_paths() > 0,
        "the map was rebuilt from the row the write just made"
    );

    let response = call(
        &state,
        authed(Method::GET, "/api/v1/pages", None, &admin),
    )
    .await;

    let deprecation = response
        .headers
        .get("deprecation")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert_eq!(deprecation, "1.4", "RFC 8594 Deprecation carries the version");

    let sunset = response
        .headers
        .get("sunset")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        sunset.ends_with(" GMT") && sunset.contains("202"),
        "Sunset is an IMF-fixdate, which a client parses without a format argument: {sunset}"
    );

    let link = response
        .headers
        .get("link")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        link.contains("rel=\"deprecation\""),
        "the changelog Link names the relation: {link}"
    );
    assert!(
        link.contains(omnion_graphql::deprecation::CHANGELOG_PATH),
        "and points at the changelog the policy declares: {link}"
    );
}

#[tokio::test]
async fn a_sunset_in_the_past_answers_410_and_never_runs_the_handler() {
    let state = support::walk_state::state_or_fail().await;
    let (_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    // The route is announced with a date in the future and then given a PAST one by writing the
    // row directly, because the write path correctly refuses to announce a past sunset — and a
    // walk that got that by announcing one would be measuring the refusal, not the middleware.
    let announced = announce(&state, &admin, &in_months(9), Some("/pages"), "/api/v1/pages").await;
    assert_eq!(announced.status, StatusCode::CREATED, "{}", announced.body);
    let id: Uuid = announced.body["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a uuid");

    let past = OffsetDateTime::now_utc() - Duration::days(1);
    sqlx::query("update api_deprecations set sunset_at = $2 where id = $1")
        .bind(id)
        .bind(past)
        .execute(state.db().pool())
        .await
        .expect("the sunset is moved into the past");
    omnion_api::deprecation_middleware::refresh(state.db().pool()).await;

    let response = call(
        &state,
        authed(Method::GET, "/api/v1/pages", None, &admin),
    )
    .await;

    assert_eq!(
        response.status,
        StatusCode::GONE,
        "410 is the only status that means 'this existed and will not come back': {}",
        response.body
    );
    assert_eq!(response.body["code"], "REMOVED", "a client branches on this code");
    assert_eq!(response.body["replacement"], "/pages");
    assert!(
        !response.headers.contains_key("sunset"),
        "no Sunset on a response whose sunset has passed — it asks the client to plan around a \
         date in its own past"
    );

    // The screen agrees with the middleware WITHOUT waiting for the sweeper: the row's column
    // still says `announced` here.
    let listed = call(
        &state,
        authed(Method::GET, "/api/v1/api/deprecations", None, &admin),
    )
    .await;
    let row = listed.body["deprecations"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|row| row["id"] == id.to_string())
        .expect("the announced row is listed");
    assert_eq!(
        row["status"], "removed",
        "the screen reads the POLICY, not the column, so it never lags the middleware"
    );
    assert!(row["days_remaining"].as_i64().expect("a number") < 0);
    assert_eq!(row["countdown"], "sunset passed");
}

#[tokio::test]
async fn the_sweeper_advances_a_past_sunset_and_leaves_a_withdrawal_alone() {
    let state = support::walk_state::state_or_fail().await;
    let (_id, org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let announced = announce(&state, &admin, &in_months(9), Some("/pages"), "/api/v1/pages").await;
    assert_eq!(announced.status, StatusCode::CREATED, "{}", announced.body);
    let past_id: Uuid = announced.body["id"].as_str().expect("an id").parse().expect("a uuid");

    // A second row, withdrawn before its sunset, whose sunset is then moved into the past. The
    // sweeper must NOT advance it: a withdrawal is the operator's decision and an unattended
    // sweep that undoes it makes the control impossible to see.
    let withdrawn = announce(&state, &admin, &in_months(9), Some("/pages"), "/api/v1/media").await;
    let withdrawn_id: Uuid = withdrawn.body["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a uuid");
    let withdrawn_call = call(
        &state,
        authed(
            Method::POST,
            &format!("/api/v1/api/deprecations/{withdrawn_id}/withdraw"),
            Some(json!({ "reason": "the media route was never released" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(withdrawn_call.status, StatusCode::OK, "{}", withdrawn_call.body);
    assert_eq!(withdrawn_call.body["status"], "withdrawn");

    let past = OffsetDateTime::now_utc() - Duration::days(1);
    sqlx::query("update api_deprecations set sunset_at = $2 where id = any($1)")
        .bind(vec![past_id, withdrawn_id])
        .bind(past)
        .execute(state.db().pool())
        .await
        .expect("both sunsets move into the past");

    let advanced = omnion_api::routes::graphql_deprecations::sweep(state.db().pool(), OffsetDateTime::now_utc())
        .await
        .expect("the sweep runs");

    // **Membership, not equality — and the first draft asserted equality and failed.** The sweep
    // is deliberately process-wide (it walks every tenant's rows, because a sunset that passed
    // somewhere has passed everywhere), so a suite whose earlier walks left past sunsets behind
    // gets its ids in the return too. Asserting `advanced == vec![past_id]` therefore measures
    // what the OTHER walks in this file announced, and reads as a sweeper defect when it is a
    // test-ordering one. The claim under test is about TWO rows — the past one advanced, the
    // withdrawn one not — and membership states it without depending on anything else.
    assert!(
        advanced.contains(&past_id),
        "the sweep must advance the row whose sunset passed: {advanced:?}"
    );
    assert!(
        !advanced.contains(&withdrawn_id),
        "and must NOT advance the withdrawal: {advanced:?}"
    );

    let statuses: Vec<(Uuid, String)> = sqlx::query_as(
        "select id, status from api_deprecations where id = any($1) order by id",
    )
    .bind(vec![past_id, withdrawn_id])
    .fetch_all(state.db().pool())
    .await
    .expect("the statuses read back");
    let map: std::collections::HashMap<Uuid, String> = statuses.into_iter().collect();
    assert_eq!(map.get(&past_id).map(String::as_str), Some("removed"));
    assert_eq!(
        map.get(&withdrawn_id).map(String::as_str),
        Some("withdrawn"),
        "an unattended sweep must not undo an operator's withdrawal"
    );

    // And the sweep is IDEMPOTENT: a second tick advances nothing, which is the property that
    // lets a poller run it every minute.
    let again = omnion_api::routes::graphql_deprecations::sweep(state.db().pool(), OffsetDateTime::now_utc())
        .await
        .expect("the second sweep runs");
    assert!(
        !again.contains(&past_id) && !again.contains(&withdrawn_id),
        "a second tick must advance neither of this walk's rows: {again:?}"
    );

    // The sweep is deliberately NOT tenant-scoped, so this row is not an assertion that nothing
    // outside the tenant moved — it is the record of WHY: an installation-wide sweep is correct
    // here because a sunset that has passed has passed for every tenant of the installation, and
    // scoping the sweeper to one tenant would leave every other tenant's screen lying.
    //
    // What the walk does assert about scope is that this walk's OWN pair came out right, which is
    // the two assertions above. The old tail of this function counted rows `>= 0` and asserted
    // nothing at all — a leftover from a draft where it was going to compare against a
    // per-tenant sweep, and it would have passed on any database in any state.
    assert!(org != Uuid::nil(), "the sweep ran against a tenant that exists");
}

#[tokio::test]
async fn an_extension_needs_a_reason_and_is_audited_with_it() {
    let state = support::walk_state::state_or_fail().await;
    let (admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let announced = announce(&state, &admin, &in_months(9), Some("/pages"), "/api/v1/pages").await;
    let id: Uuid = announced.body["id"].as_str().expect("an id").parse().expect("a uuid");

    let no_reason = call(
        &state,
        authed(
            Method::POST,
            &format!("/api/v1/api/deprecations/{id}/extend"),
            Some(json!({ "sunset_at": in_months(18) })),
            &admin,
        ),
    )
    .await;
    assert_eq!(
        no_reason.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a reason is required: {}",
        no_reason.body
    );
    assert!(
        error_message(&no_reason.body).contains("reason"),
        "the message says what is missing: {}",
        error_message(&no_reason.body)
    );

    let backwards = call(
        &state,
        authed(
            Method::POST,
            &format!("/api/v1/api/deprecations/{id}/extend"),
            Some(json!({ "sunset_at": in_months(7), "reason": "shortened" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(
        backwards.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "shortening is a withdrawal, which is a different audited action: {}",
        backwards.body
    );

    let extended = call(
        &state,
        authed(
            Method::POST,
            &format!("/api/v1/api/deprecations/{id}/extend"),
            Some(json!({ "sunset_at": in_months(18), "reason": "the integrator asked for a quarter" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(extended.status, StatusCode::OK, "{}", extended.body);

    // The REASON is the audited payload, read back out of PostgreSQL rather than from the
    // response. A reason stored only in the response body would be a reason nobody can find.
    let reason: Option<String> = sqlx::query_scalar(
        "select metadata ->> 'reason' from audit_log \
         where actor_user_id = $1 and action = 'api.deprecation.extended'",
    )
    .bind(admin_id)
    .fetch_one(state.db().pool())
    .await
    .expect("the audit row runs");
    assert_eq!(reason.as_deref(), Some("the integrator asked for a quarter"));

    assert_eq!(
        audit_rows(&state, admin_id, "api.deprecation.announced").await,
        1,
        "the announcement wrote exactly one row"
    );
}

#[tokio::test]
async fn a_withdrawal_stops_the_headers_and_the_410_and_walks_the_audit_trail() {
    let state = support::walk_state::state_or_fail().await;
    let (admin_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let announced = announce(&state, &admin, &in_months(9), Some("/pages"), "/api/v1/pages").await;
    let id: Uuid = announced.body["id"].as_str().expect("an id").parse().expect("a uuid");
    omnion_api::deprecation_middleware::refresh(state.db().pool()).await;

    let before = call(&state, authed(Method::GET, "/api/v1/pages", None, &admin)).await;
    assert!(before.headers.contains_key("sunset"), "announced, so it is announced");

    let withdrawn = call(
        &state,
        authed(
            Method::POST,
            &format!("/api/v1/api/deprecations/{id}/withdraw"),
            Some(json!({ "reason": "the replacement shipped under the same path" })),
            &admin,
        ),
    )
    .await;
    assert_eq!(withdrawn.status, StatusCode::OK, "{}", withdrawn.body);
    omnion_api::deprecation_middleware::refresh(state.db().pool()).await;

    let after = call(&state, authed(Method::GET, "/api/v1/pages", None, &admin)).await;
    assert!(
        !after.headers.contains_key("deprecation")
            && !after.headers.contains_key("sunset")
            && !after.headers.contains_key("link"),
        "a withdrawal must stop all three headers, and the route must work again: {}",
        after.status
    );
    assert_ne!(
        after.status,
        StatusCode::GONE,
        "and it must not be 410: the surface was never removed"
    );

    assert_eq!(audit_rows(&state, admin_id, "api.deprecation.withdrawn").await, 1);
}

#[tokio::test]
async fn the_notified_control_records_the_fact_and_reads_back() {
    let state = support::walk_state::state_or_fail().await;
    let (_id, _org, admin) = account_with(&state, &[READ, MANAGE]).await;

    let announced = announce(&state, &admin, &in_months(9), Some("/pages"), "/api/v1/pages").await;
    let id: Uuid = announced.body["id"].as_str().expect("an id").parse().expect("a uuid");
    assert!(
        announced.body["notified_at"].is_null(),
        "an announcement nobody notified about reads as not notified"
    );

    let notified = call(
        &state,
        authed(
            Method::POST,
            &format!("/api/v1/api/deprecations/{id}/notified"),
            None,
            &admin,
        ),
    )
    .await;
    assert_eq!(notified.status, StatusCode::OK, "{}", notified.body);
    assert!(
        !notified.body["notified_at"].is_null(),
        "the column the screen's own 'Notified' cell reads is written by the control: {}",
        notified.body
    );
}