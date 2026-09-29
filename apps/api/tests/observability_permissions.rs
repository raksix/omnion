//! Integration walk for REQ-126 slice 4d: `observability.read` cannot write, and every mutation
//! leaves an audit row.
//!
//! It runs against the development stack and skips itself with a printed reason when PostgreSQL
//! is not reachable, exactly like the slices 1–4c suites beside it.
//!
//! ## What this walk is for, given that the acceptance line is one sentence
//!
//! **"`observability.read` cannot create exporters, rules or silences (`403`); every mutation
//! writes an audit row."** Two halves, and the second one is the interesting half — an audit
//! assertion that only checks the happy path is satisfied by *any* handler that records
//! something, so it cannot tell a covered mutation from an uncovered one.
//!
//! So this walk refuses in this order:
//!
//! 1. **Every mutating route, as a caller holding only `observability.read`.** One table of
//!    `(method, path, body)`, one loop, one `403` each — and the walk reads the refusal's
//!    `code`/`details.permission` rather than only the status, so a route that answered `403`
//!    for an unrelated reason would not pass.
//! 2. **The same table again, as an account holding `observability.manage`, asserting the audit
//!    table afterwards.** The audit assertion is per route *by name*, and the names are read
//!    back out of PostgreSQL: a route that recorded nothing is a failure here even though its
//!    own unit test passed and its own handler looks right.
//! 3. **The method the CLIENT sends, not the method the router happens to register.** The
//!    exporters screen's Edit button sends `PATCH`; the route registered only `put`, so the
//!    button answered `405` and no test anywhere could see it — a green suite is exactly what
//!    this class of defect looks like. The methods below are lifted from `apps/admin/lib/api.ts`,
//!    and a comment on each says so.
//!
//! ## The one thing this walk cannot assert
//!
//! That a *denied* attempt is recorded. It deliberately does not: the guard refuses before any
//! handler runs, so there is no row to write, and inventing one inside the guard would mean the
//! audit trail claims an action that a handler never performed. `secret.access.denied` (REQ-125
//! slice 4) is the pattern for a recorded refusal, and it lives where the refusal actually
//! happens — in the handler that refused, after it has decided to refuse.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use time::OffsetDateTime;
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// One mutating route, with the method the PANEL sends.
struct Mutation {
    method: Method,
    /// `{name}` is filled with the rule id created for this walk.
    path: &'static str,
    body: Value,
    /// The audit action the route must leave behind.
    action: &'static str,
}

fn mutations() -> Vec<Mutation> {
    vec![
        // apps/admin/lib/api.ts · createExporter → POST
        Mutation {
            method: Method::POST,
            path: "/api/v1/observability/exporters",
            body: json!({
                "name": format!("walk-exporter-{}", Uuid::new_v4().simple()),
                "kind": "webhook",
                "endpoint": "http://127.0.0.1:9/never",
                "batch_ms": 60000,
                "timeout_ms": 1000,
                "enabled": true,
            }),
            action: "observability.exporter.created",
        },
        // apps/admin/lib/api.ts · updateExporter → PATCH  (the route used to register only PUT)
        Mutation {
            method: Method::PATCH,
            path: "/api/v1/observability/exporters/{id}",
            body: json!({ "enabled": false, "batch_ms": 61000 }),
            action: "observability.exporter.updated",
        },
        // apps/admin/lib/api.ts · deleteExporter → DELETE
        Mutation {
            method: Method::DELETE,
            path: "/api/v1/observability/exporters/{id}",
            body: Value::Null,
            action: "observability.exporter.deleted",
        },
        // apps/admin/lib/api.ts · createAlertRule → POST
        Mutation {
            method: Method::POST,
            path: "/api/v1/observability/alert-rules",
            body: json!({
                "name": format!("walk-rule-{}", Uuid::new_v4().simple()),
                "expr": "omnion_queue_depth > 1000",
                "severity": "warning",
                "for_seconds": 0,
                "summary": "the permissions walk's own rule",
                "runbook_url": null,
            }),
            action: "observability.alert_rule.created",
        },
        // apps/admin/lib/api.ts · updateAlertRule → PATCH
        Mutation {
            method: Method::PATCH,
            path: "/api/v1/observability/alert-rules/{id}",
            body: json!({ "enabled": false }),
            action: "observability.alert_rule.updated",
        },
        // apps/admin/lib/api.ts · deleteAlertRule → DELETE
        Mutation {
            method: Method::DELETE,
            path: "/api/v1/observability/alert-rules/{id}",
            body: Value::Null,
            action: "observability.alert_rule.deleted",
        },
        // apps/admin/lib/api.ts · previewAlertRule → POST. It evaluates a caller-supplied
        // expression and writes nothing, so it carries `observability.manage` and is exempt from
        // the audit half — the request lists it as a read, not a mutation.
        Mutation {
            method: Method::POST,
            path: "/api/v1/observability/alert-rules/preview",
            body: json!({ "expr": "omnion_queue_depth > 0" }),
            action: "",
        },
        // apps/admin/lib/api.ts · createSilence → POST
        Mutation {
            method: Method::POST,
            path: "/api/v1/observability/silences",
            // `ends_at` is an RFC 3339 instant, not a duration: the handler parses it and
            // refuses an end in the past, and a test that sent a duration would be asserting a
            // refusal it thought was the guard.
            body: json!({ "reason": "the permissions walk", "ends_at": rfc3339_in(1) }),
            action: "observability.silence.created",
        },
        // apps/admin/lib/api.ts · deleteSilence → DELETE
        Mutation {
            method: Method::DELETE,
            path: "/api/v1/observability/silences/{id}",
            body: Value::Null,
            action: "observability.silence.lifted",
        },
        // apps/admin/lib/api.ts · saveObservabilitySettings → PUT
        Mutation {
            method: Method::PUT,
            path: "/api/v1/observability/settings",
            body: json!({
                "sampling_ratio": 0.2,
                "logs_retention_days": 14,
                "traces_retention_days": 7,
                "log_level_default": "info",
                "log_level_overrides": {},
                "cardinality_budget": 10000,
                "prometheus_public": false,
            }),
            action: "observability.settings.updated",
        },
        // apps/admin/lib/api.ts · syncMetricCatalog → POST
        Mutation {
            method: Method::POST,
            path: "/api/v1/observability/metrics/sync",
            body: Value::Null,
            action: "observability.metrics.catalog_synced",
        },
    ]
}

/// An RFC 3339 instant `hours` from now, for the silence window.
///
/// Built from the clock rather than pasted as a literal: a fixed instant that eventually falls
/// behind `now()` turns this walk into a suite that fails on a Tuesday for a reason that has
/// nothing to do with the permission it is checking.
fn rfc3339_in(hours: i64) -> String {
    (OffsetDateTime::now_utc() + time::Duration::hours(hours))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("an RFC 3339 instant")
}

struct TestResponse {
    status: StatusCode,
    body: Value,
    cookie: Option<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.11:51234".parse().expect("a peer address");
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    // `oneshot` bypasses the connect layer, so without this the peer address is missing on every
    // row — and an assertion about a peer then passes on exactly the data it should not.
    let mut response = response;
    response
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));

    let status = response.status();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie| cookie.split(';').next())
        .map(str::to_owned);
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

/// A request authenticated as the account that owns the walk's fixtures.
///
/// A walk that creates its fixtures through the account under test is a walk whose setup
/// depends on the thing it is proving: the read-only walk needed `observability.manage` to
/// create an exporter, got a bare `403` instead, and the failure read as "the fixture was
/// refused" rather than "the fixture was never created". The cookie is therefore passed
/// explicitly — a task-local that only lives for the scope that sets it is worse than a
/// parameter, because the scope ends and the reader has to know it.
fn admin_request(method: Method, uri: &str, body: Option<Value>, cookie: &str) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie);
    match body {
        None | Some(Value::Null) => builder.body(Body::empty()),
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string())),
    }
    .expect("the admin request builds")
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

/// An account in a fresh organization, with a role holding exactly `permissions`.
///
/// The role is created per run and per walk step rather than shared, because a role whose
/// permission set is right for one assertion is wrong for the next: `observability.read` must
/// NOT be able to write, and a helper that quietly added the write key would turn the whole
/// walk into a suite that asserts nothing.
async fn account_with(state: &AppState, permissions: &[&str]) -> (Uuid, Uuid, String) {
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Observability walk {suffix}"),
            slug: format!("obs-walk-{suffix}"),
        },
    )
    .await
    .expect("the organization is created");
    let email = format!("obs-walk-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Watch Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account is created");

    let role = role_store::create_role(
        state.db().pool(),
        NewRole {
            organization_id: organization.id,
            key: format!("obs-walk-role-{suffix}"),
            name: "Observability walk role".to_owned(),
            description: "A role of the observability permissions walk".to_owned(),
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

/// The value for a session `Cookie` header.
///
/// The bare `name=value` pair, NOT `Cookie: name=value`. This helper used to return the
/// `Cookie: ` prefix as well, and both call sites passed it to `.header(header::COOKIE, …)`,
/// so the request went out as `Cookie: Cookie: omnion_session=…`. Every authenticated call
/// answered `401 unauthenticated` — and because the whole suite was skipping (see
/// `state_or_fail`), that never showed up: the first run of these walks was a *vacuous* pass,
/// and the second exposed a second, independent defect underneath it.
///
/// This is why a skip guard and a first real run are two different pieces of evidence. The
/// guard is what makes the run fail loudly; only a genuine run finds what was hidden.
fn cookie_header(cookie: &str) -> String {
    cookie.to_owned()
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

#[tokio::test]
async fn observability_read_is_refused_on_every_write_and_the_refusal_names_the_permission() {
    // TWO accounts, deliberately. The fixtures below (a real exporter, a real rule, a real
    // silence) are created with `observability.manage`, because `routes/mod.rs` guards the whole
    // write surface with `guards::require(&state, "observability.manage")` — a narrower
    // `observability.exporters.manage` does NOT create an exporter, and asking the account under
    // test to create its own fixture is the mistake this two-account split removes. The account
    // that is then refused holds `observability.read` and nothing else.
    let state = support::walk_state::state_or_fail().await;
    let (_admin, _admin_org, admin_cookie) =
        account_with(&state, &["observability.manage", "observability.read"]).await;
    let (_actor, _organization, cookie) =
        account_with(&state, &["observability.read"]).await;

    // A real exporter and a real rule, so the paths under test address rows that exist rather
    // than ids that 404. A refusal on a missing row would still be a `403` from the guard — the
    // guard runs first — so the walk would pass while proving nothing about the body.
    let created_exporter = call(
        &state,
        admin_request(
            Method::POST,
            "/api/v1/observability/exporters",
            Some(json!({
                "name": format!("readonly-fixture-{}", Uuid::new_v4().simple()),
                "kind": "webhook",
                "endpoint": "http://127.0.0.1:9/never",
                "batch_ms": 60000,
                "timeout_ms": 1000,
                "enabled": true,
            })),
            &admin_cookie,
        ),
    )
    .await;
    let exporter_id = created_exporter.body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the fixture exporter was created: {}", created_exporter.body))
        .to_owned();
    let created_rule = call(
        &state,
        admin_request(
            Method::POST,
            "/api/v1/observability/alert-rules",
            Some(json!({
                "name": format!("readonly-fixture-{}", Uuid::new_v4().simple()),
                "expr": "omnion_queue_depth > 1000",
                "severity": "warning",
                "for_seconds": 0,
                "summary": "a rule the readonly walk addresses",
                "runbook_url": null,
            })),
            &admin_cookie,
        ),
    )
    .await;
    let rule_id = created_rule.body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the fixture rule was created: {}", created_rule.body))
        .to_owned();

    let created_silence = call(
        &state,
        admin_request(
            Method::POST,
            "/api/v1/observability/silences",
            Some(json!({ "rule_id": rule_id, "reason": "the readonly walk", "ends_at": rfc3339_in(1) })),
            &admin_cookie,
        ),
    )
    .await;
    let silence_id = created_silence.body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the fixture silence was created: {}", created_silence.body))
        .to_owned();

    let mut refused = 0;
    for mutation in mutations() {
        let uri = mutation
            .path
            .replace("{id}", match mutation.action {
                "observability.exporter.updated" | "observability.exporter.deleted" => {
                    exporter_id.as_str()
                }
                "observability.alert_rule.updated" | "observability.alert_rule.deleted" => {
                    rule_id.as_str()
                }
                "observability.silence.lifted" => silence_id.as_str(),
                _ => "",
            })
            .to_owned();
        let body = match mutation.action {
            "observability.alert_rule.updated" => {
                Some(json!({ "enabled": false }))
            }
            "observability.silence.lifted" => None,
            _ => Some(mutation.body.clone()),
        };
        let response = call(
            &state,
            Request::builder()
                .method(mutation.method.clone())
                .uri(&uri)
                .header(header::COOKIE, cookie_header(&cookie))
                .header(header::CONTENT_TYPE, "application/json")
                .body(match &body {
                    Some(value) => Body::from(value.to_string()),
                    None => Body::empty(),
                })
                .expect("the mutation request builds"),
        )
        .await;

        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{} {} was not refused for a caller holding only observability.read: {}",
            mutation.method,
            mutation.path,
            response.body
        );
        // The status alone is not the claim. A route that answered 403 for an unrelated reason —
        // a cross-scope check, a missing row — would satisfy the line above, so the refusal has
        // to name the permission that was missing.
        assert_eq!(
            response.body["error"]["code"], "permission_denied",
            "{} {} refused with the wrong code: {}",
            mutation.method,
            mutation.path,
            response.body
        );
        assert_eq!(
            response.body["error"]["details"]["permission"], "observability.manage",
            "{} {} refused without naming the missing power: {}",
            mutation.method,
            mutation.path,
            response.body
        );
        refused += 1;
    }

    assert_eq!(
        refused,
        mutations().len(),
        "the walk skipped a route: every mutating observability route has to be driven"
    );
}

#[tokio::test]
async fn every_mutation_writes_its_own_audit_row() {
    let state = support::walk_state::state_or_fail().await;
    let (actor, _organization, cookie) = account_with(
        &state,
        &[
            "observability.read",
            "observability.manage",
            "observability.exporters.manage",
        ],
    )
    .await;
    let pool = state.db().pool();

    let mut created_exporter = String::new();
    let mut created_rule = String::new();
    // A SECOND rule, for the silence. The table deletes the alert rule before it creates the
    // silence, so a silence aimed at that rule addresses a row that no longer exists and the
    // walk dies with `404 no alert rule with the id …` — which reads as a broken foreign key
    // and is really a walk that shared one fixture between two rows with different lifetimes.
    let mut silence_rule = String::new();
    let mut created_silence = String::new();

    for mutation in mutations() {
        // The addresser runs BEFORE the assertion, because the ids the later rows address are
        // produced by the earlier ones. A `403` walk has nothing to address and therefore does
        // not need it.
        let id = match mutation.action {
            "observability.exporter.updated" | "observability.exporter.deleted" => {
                if created_exporter.is_empty() {
                    let response = call(
                        &state,
                        authed(
                            Method::POST,
                            "/api/v1/observability/exporters",
                            Some(json!({
                                "name": format!("audit-exporter-{}", Uuid::new_v4().simple()),
                                "kind": "webhook",
                                "endpoint": "http://127.0.0.1:9/never",
                                "batch_ms": 60000,
                                "timeout_ms": 1000,
                                "enabled": true,
                            })),
                            &cookie,
                        ),
                    )
                    .await;
                    created_exporter = response.body["id"]
                        .as_str()
                        .expect("the exporter was created")
                        .to_owned();
                }
                created_exporter.clone()
            }
            "observability.alert_rule.updated" | "observability.alert_rule.deleted" => {
                if created_rule.is_empty() {
                    let response = call(
                        &state,
                        authed(
                            Method::POST,
                            "/api/v1/observability/alert-rules",
                            Some(json!({
                                "name": format!("audit-rule-{}", Uuid::new_v4().simple()),
                                "expr": "omnion_queue_depth > 1000",
                                "severity": "warning",
                                "for_seconds": 0,
                                "summary": "a rule the audit walk addresses",
                                "runbook_url": null,
                            })),
                            &cookie,
                        ),
                    )
                    .await;
                    created_rule = response.body["id"]
                        .as_str()
                        .unwrap_or_else(|| panic!("the audit walk's rule was not created: {}", response.body))
                        .to_owned();
                }
                created_rule.clone()
            }
            // The rule the silence will belong to, for BOTH silence rows. The lifted row
            // addresses the SILENCE, not the rule, so it captures the created silence's id below
            // instead — sharing one branch for the two would send `DELETE /silences/{rule id}`
            // and produce a `404 no silence with the id …`.
            "observability.silence.created" | "observability.silence.lifted" => {
                if silence_rule.is_empty() {
                    let response = call(
                        &state,
                        authed(
                            Method::POST,
                            "/api/v1/observability/alert-rules",
                            Some(json!({
                                "name": format!("audit-silence-rule-{}", Uuid::new_v4().simple()),
                                "expr": "omnion_queue_depth > 1000",
                                "severity": "warning",
                                "for_seconds": 0,
                                "summary": "the rule the audit walk's silence belongs to",
                                "runbook_url": null,
                            })),
                            &cookie,
                        ),
                    )
                    .await;
                    silence_rule = response.body["id"]
                        .as_str()
                        .unwrap_or_else(|| {
                            panic!("the silence's rule was not created: {}", response.body)
                        })
                        .to_owned();
                }
                if mutation.action == "observability.silence.lifted" {
                    // The lifted row addresses the silence created by the row above it.
                    created_silence.clone()
                } else {
                    silence_rule.clone()
                }
            }
            _ => String::new(),
        };

        let uri = mutation.path.replace("{id}", &id);
        let body = match mutation.action {
            // The patch body carries only the field that changes — the id is in the PATH.
            // `AlertRulePatch` is `deny_unknown_fields` (as every other write in this surface
            // is), so an `id` echoed in the body is a `422`. This walk used to send one and
            // the route was right to refuse it: a body that repeats the path's own id can
            // disagree with it, and nothing downstream would notice.
            "observability.alert_rule.updated" => Some(json!({ "enabled": false })),
            "observability.exporter.updated" => {
                let response = call(
                    &state,
                    authed(
                        Method::GET,
                        &format!("/api/v1/observability/exporters"),
                        None,
                        &cookie,
                    ),
                )
                .await;
                // The update body is the exporter's own shape; the route validates it, so a
                // partial body is a 422 rather than a silent reset of the unnamed fields.
                let row = response.body["exporters"]
                    .as_array()
                    .and_then(|rows| rows.iter().find(|row| row["id"] == id.as_str()))
                    .cloned()
                    .expect("the exporter is listed");
                Some(json!({
                    "name": row["name"],
                    "kind": row["kind"],
                    "endpoint": row["endpoint"],
                    "protocol": row["protocol"],
                    "batch_ms": row["batch_ms"],
                    "timeout_ms": row["timeout_ms"],
                    "enabled": false,
                }))
            }
            "observability.silence.lifted" | "observability.exporter.deleted"
            | "observability.alert_rule.deleted" => None,
            _ => Some(mutation.body.clone()),
        };

        // The baseline is the actor's TOTAL row count, counted the same way for every
        // mutation. The preview has no action name of its own, and a baseline of `0` for it
        // compares an absolute count against zero — after the eight mutations that precede it in
        // the table, which is why it reported `left: 8, right: 0` and read as "the preview wrote
        // a row" when the eight rows were the other mutations' own. A count is only comparable
        // to a count.
        let before: i64 = sqlx::query_scalar("select count(*) from audit_log where actor_user_id = $1")
            .bind(actor)
            .fetch_one(pool)
            .await
            .expect("the baseline count runs");
        let response = call(
            &state,
            authed(mutation.method.clone(), &uri, body, &cookie),
        )
        .await;
        assert!(
            response.status.is_success(),
            "{} {} as an account with observability.manage answered {}: {}",
            mutation.method,
            uri,
            response.status,
            response.body
        );

        if mutation.action == "observability.silence.created" {
            created_silence = response.body["id"]
                .as_str()
                .unwrap_or_else(|| panic!("the silence was not created: {}", response.body))
                .to_owned();
        }

        if mutation.action.is_empty() {
            // The preview writes nothing, and that is the contract: a GET-shaped evaluation must
            // not leave a row an operator later has to explain. Asserted rather than assumed —
            // against the same total-count baseline the other mutations are measured from.
            let after: i64 =
                sqlx::query_scalar("select count(*) from audit_log where actor_user_id = $1")
                    .bind(actor)
                    .fetch_one(pool)
                    .await
                    .expect("the count runs");
            assert_eq!(
                after, before,
                "the alert preview wrote an audit row; it evaluates and returns"
            );
            continue;
        }

        // Counted the same way as the baseline: the actor's total. The per-action count is
        // asserted separately below, because "exactly one row for THIS action" and "exactly one
        // row added in total" are different claims and only the first one survives a route that
        // writes one right row and one wrong one.
        let after: i64 =
            sqlx::query_scalar("select count(*) from audit_log where actor_user_id = $1")
                .bind(actor)
                .fetch_one(pool)
                .await
                .expect("the count runs");
        assert_eq!(
            after,
            before + 1,
            "{} {} answered {} but did not write exactly one audit row for its actor",
            mutation.method,
            uri,
            response.status
        );
        let per_action = audit_rows(&state, actor, mutation.action).await;
        assert_eq!(
            per_action, 1,
            "{} {} wrote a row whose action is not `{}`",
            mutation.method,
            uri,
            mutation.action
        );
    }
}

fn authed(method: Method, uri: &str, body: Option<Value>, cookie: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie_header(cookie));
    if let Some(value) = &body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        return builder
            .body(Body::from(value.to_string()))
            .expect("the authenticated request builds");
    }
    builder.body(Body::empty()).expect("the request builds")
}

#[tokio::test]
async fn the_preview_stays_read_only_for_an_account_with_manage() {
    let state = support::walk_state::state_or_fail().await;
    let (_actor, _organization, cookie) = account_with(
        &state,
        &["observability.read", "observability.manage"],
    )
    .await;
    let before: i64 = sqlx::query_scalar("select count(*) from audit_log")
        .fetch_one(state.db().pool())
        .await
        .expect("the count runs");
    let response = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/observability/alert-rules/preview",
            Some(json!({ "expr": "omnion_queue_depth > 0" })),
            &cookie,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let after: i64 = sqlx::query_scalar("select count(*) from audit_log")
        .fetch_one(state.db().pool())
        .await
        .expect("the count runs");
    assert_eq!(
        after, before,
        "the preview is an evaluation, not a mutation, and must leave no audit row"
    );
}

#[tokio::test]
async fn a_read_only_account_may_still_read_the_whole_surface() {
    let state = support::walk_state::state_or_fail().await;
    let (_actor, _organization, cookie) = account_with(&state, &["observability.read"]).await;

    // The narrowing property: `observability.read` is a REAL read permission, not a permission
    // that exists only so the write half can be refused. A guard that denied everything would
    // pass the 403 walk and make the whole screen unusable for its intended reader.
    for uri in [
        "/api/v1/observability/logs?limit=5",
        "/api/v1/observability/metrics/catalog",
        "/api/v1/observability/metrics/query?metric=omnion_queue_depth&range=15m",
        "/api/v1/observability/traces?limit=5",
        "/api/v1/observability/exporters",
        "/api/v1/observability/alert-rules",
        "/api/v1/observability/alerts",
        "/api/v1/observability/settings",
        "/api/v1/observability/bundle",
        "/api/v1/observability/lifecycle",
    ] {
        let response = call(&state, authed(Method::GET, uri, None, &cookie)).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{uri} was refused for a caller holding observability.read: {}",
            response.body
        );
    }
}
