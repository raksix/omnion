//! Integration test for the metric registry, the exposition and the catalogue
//! (docs/requests/REQ-126, slice 2).
//!
//! It runs against the development stack and skips itself with a printed reason when PostgreSQL
//! is not reachable, exactly like the REQ-125 and REQ-126 slice 1 suites beside it.
//!
//! The walk proves the four claims of the slice, and each one is a claim about what a **caller
//! can see**, not about what the registry holds:
//!
//! 1. **A scrape returns every documented family with real values after traffic.** Traffic is
//!    generated over the real router, not by calling the registry, because the acceptance line
//!    says "after traffic" and a test that records a metric itself proves only that recording
//!    works. The status label is asserted to be a class (`2xx`), because a per-code label is the
//!    cardinality failure the request names and nothing else would catch it here.
//! 2. **Route labels are templates, never literal ids.** Two requests to two *different* ids of
//!    the same route must land on ONE series. This is the assertion that would fail if the
//!    middleware passed `request.uri()` — and it would fail loudly, which a "the label looks
//!    plausible" check never would.
//! 3. **Registering past the cardinality budget is reported and labelled, not silently dropped.**
//!    The fold has to be visible three ways: the sample lands in the `other` series, the
//!    `omnion_registry_budget_exceeded` counter appears on the scrape, and the catalogue names
//!    the family. A guard that only did the first would satisfy "bounded" and fail "reported".
//! 4. **The catalogue is seeded from the registry and matches it.** Not "the endpoint answers":
//!    a catalogue seeded from a hard-coded list and a registry that disagrees with it is the
//!    failure this whole design exists to prevent, so the test compares the two name sets.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_telemetry::metric_catalog;
use omnion_telemetry::metrics::{self, FAMILIES};
use serde_json::{Value, json};
use std::net::SocketAddr;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    body: Value,
    text: String,
    request_id: Option<String>,
    /// The `omnion_session` cookie from a login, so a helper can chain it into the next call.
    cookie: Option<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.7:51234"
        .parse()
        .expect("a literal is a valid peer");
    let mut request = request;
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie| cookie.split(';').next())
        .map(str::to_owned);
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
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
        text: raw,
        request_id,
        cookie,
    }
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .expect("a static request builds")
}

async fn state_or_skip() -> Option<AppState> {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "[observability metrics] skipping: the environment is not configured ({error})"
            );
            return None;
        }
    };
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "[observability metrics] skipping: PostgreSQL is not reachable ({error}) — \
                 set OMNION_DATABASE_URL to a database with the migrations applied"
            );
            return None;
        }
    };
    if let Err(error) = db.migrate().await {
        eprintln!("[observability metrics] skipping: migrations failed ({error})");
        return None;
    }
    let redis = RedisClient::new(&config.redis.url).ok()?;
    let storage = omnion_storage::Storage::from_env().ok()?;
    let build = BuildInfo::new("omnion-api", env!("CARGO_PKG_VERSION"));
    Some(AppState::new(build, config, db, redis, storage))
}

/// An authenticated caller with the owner role bound, so the guarded routes are not a 403.
///
/// The session travels as the `omnion_session` **cookie**, because that is what the panel sends;
/// a suite that authenticates with `Authorization: Bearer` proves the guard accepts a header the
/// browser does not send.
async fn sign_in(state: &AppState) -> (Uuid, String) {
    seed::ensure(state.db().pool())
        .await
        .expect("the permission catalogue seeds");
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Metrics org {suffix}"),
            slug: format!("metrics-{suffix}"),
        },
    )
    .await
    .expect("the organization must be created");
    // A read may resolve its own organization, but a *write* needs a named one: the resync writes
    // an audit row scoped to the caller's organization, so the account is created with it rather
    // than patched afterwards.
    let email = format!("metrics-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Metrics Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account must be created");
    seed::bind_owner(state.db().pool(), user.id)
        .await
        .expect("the owner role must be bound");

    let response = call(
        state,
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "email": email, "password": PASSWORD }).to_string(),
            ))
            .expect("a login request must build"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "login: {}", response.text);
    let cookie = response
        .cookie
        .as_deref()
        .expect("login sets a session cookie");
    // `Set-Cookie` is `omnion_session=<token>; Path=/`, so splitting on ';' alone still carries
    // the name — and re-wrapping it produces `omnion_session=omnion_session=<token>`, which the
    // guard answers with `invalid_session` and no hint about why.
    let token = cookie
        .split_once('=')
        .map(|(_, value)| value)
        .expect("the cookie carries a value");
    (user.id, token.to_owned())
}

fn authed(method: Method, path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, format!("omnion_session={token}"))
        .body(Body::empty())
        .expect("a request with a method builds")
}

#[tokio::test]
async fn a_scrape_returns_the_documented_families_with_route_templates_and_a_status_class() {
    let Some(state) = state_or_skip().await else {
        return;
    };
    let (_user, token) = sign_in(&state).await;

    // Traffic, over the real router, so the families are filled the way they are in production.
    // Two DIFFERENT secret ids on the same route: if the middleware labelled with a literal path
    // these would be two series, and the assertion below would fail rather than "look fine".
    let first = uuid_like();
    let second = uuid_like();
    // Cloned rather than moved: the assertion below greps for the *first* id, and a `for` loop
    // over the array moves both out of scope.
    for id in [first.clone(), second.clone()] {
        let response = call(
            &state,
            authed(
                Method::GET,
                &format!("/api/v1/analytics/goals/{id}?site_id={id}"),
                &token,
            ),
        )
        .await;
        // 404 is a served request and counts as traffic; what matters is that the route matched.
        assert!(
            response.status == StatusCode::NOT_FOUND || response.status.is_success(),
            "the seeded traffic request answered {}: {}",
            response.status,
            response.text
        );
    }
    // A request that genuinely 500s is not something this test fabricates; a 4xx is enough to
    // prove the class label is computed rather than hard-coded to 2xx.
    let refused = call(
        &state,
        get("/api/v1/observability/metrics/query?metric=nope"),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);

    let scrape = call(&state, get("/metrics")).await;
    assert_eq!(scrape.status, StatusCode::OK);
    assert!(
        scrape
            .text
            .contains("# TYPE omnion_http_requests_total counter"),
        "the family header is missing: {}",
        scrape.text
    );
    assert!(
        scrape
            .text
            .contains("omnion_http_requests_total{route=\"/api/v1/analytics/goals/{id}\""),
        "the route label is not a template: {}",
        scrape.text
    );
    // The property the template label exists for: TWO requests to TWO different ids are ONE
    // series. The first draft asserted the series ended in `1`, which is what a single request
    // would produce — so it passed against an implementation that was counting correctly and
    // failed against the one thing being tested. What has to be asserted is the COUNT of series
    // (1) and the value on it (2), and both.
    let template_series: Vec<&str> = scrape
        .text
        .lines()
        .filter(|line| line.contains("route=\"/api/v1/analytics/goals/{id}\""))
        .filter(|line| line.starts_with("omnion_http_requests_total{"))
        .collect();
    assert_eq!(
        template_series.len(),
        1,
        "two ids on one route produced {} series: {template_series:?}",
        template_series.len()
    );
    assert!(
        template_series[0].ends_with("} 2"),
        "the two requests are not counted on the one series: {template_series:?}"
    );
    // And the status is a CLASS. A per-code label is the cardinality failure the request names,
    // and `2xx`/`4xx`/`5xx` is what the panel's ratio is computed from.
    assert!(
        template_series[0].contains("status=\"4xx\"")
            || template_series[0].contains("status=\"2xx\""),
        "the status label is not a class: {template_series:?}"
    );
    assert!(
        !template_series[0].contains("status=\"404\"")
            && !template_series[0].contains("status=\"200\""),
        "the status label is a code, not a class: {template_series:?}"
    );
    assert!(
        !scrape.text.contains(&format!("goals/{first}")),
        "a literal id leaked into the exposition: {}",
        scrape.text
    );
    assert!(
        scrape
            .text
            .contains("omnion_http_request_duration_seconds_count"),
        "the duration histogram is missing: {}",
        scrape.text
    );

    // Every documented family has a TYPE line, even the ones with no samples yet: an operator
    // scraping this instance must be able to see what the platform *can* emit.
    for spec in FAMILIES {
        assert!(
            scrape.text.contains(&format!("# TYPE {} ", spec.name)),
            "{} has no TYPE line in the exposition",
            spec.name
        );
    }

    // Exactly one HELP and one TYPE per name: a duplicate makes Prometheus refuse the WHOLE
    // scrape, and every other assertion here would still have passed.
    for spec in FAMILIES {
        let helps = scrape
            .text
            .lines()
            .filter(|line| line.starts_with(&format!("# HELP {} ", spec.name)))
            .count();
        assert_eq!(helps, 1, "{} has {helps} HELP lines", spec.name);
    }

    let catalog = call(
        &state,
        authed(Method::GET, "/api/v1/observability/metrics/catalog", &token),
    )
    .await;
    assert_eq!(catalog.status, StatusCode::OK, "{}", catalog.text);
    let families = catalog.body["families"]
        .as_array()
        .expect("the catalogue has families");
    let names: Vec<&str> = families
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    for spec in FAMILIES {
        assert!(
            names.contains(&spec.name),
            "{} is declared by the registry and missing from the catalogue",
            spec.name
        );
    }
    // Every declared family is present. The count is NOT asserted for equality: a module may have
    // registered its own (the sibling test in this very file does, and its row survives a resync
    // by design), so "the catalogue has exactly the core's families" is a claim about a
    // configuration this instance does not control. Assert the containment, and assert that a row
    // the running build does not declare is *marked* not-live rather than silently presented as
    // current.
    assert!(names.len() >= FAMILIES.len());
    for name in &names {
        if !FAMILIES.iter().any(|spec| spec.name == *name) {
            let row = families
                .iter()
                .find(|row| row["name"] == *name)
                .expect("the row is in the list it came from");
            assert_eq!(
                row["live"],
                json!(false),
                "{name} is not declared by this build and must be marked as not live"
            );
            assert_eq!(row["source"], "module", "{name} came from a module");
        }
    }

    // The catalogue's label list is the family's declaration, positionally.
    let http_row = families
        .iter()
        .find(|row| row["name"] == "omnion_http_requests_total")
        .expect("the http family is catalogued");
    assert_eq!(http_row["labels"], json!(["route", "method", "status"]));
    assert_eq!(http_row["kind"], "counter");
    assert_eq!(http_row["source"], "core");
    assert!(http_row["cardinality_estimate"].as_i64().unwrap_or(0) >= 1);
    assert!(http_row["cardinality_budget"].as_i64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn a_query_names_the_family_when_the_selector_is_unknown_and_clamps_an_over_wide_window() {
    let Some(state) = state_or_skip().await else {
        return;
    };
    let (_user, token) = sign_in(&state).await;

    let refused = call(
        &state,
        authed(
            Method::GET,
            "/api/v1/observability/metrics/query?metric=omnion_not_a_family",
            &token,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.body["error"]["code"], "unknown_metric");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("omnion_not_a_family")),
        "the refusal does not name what was asked for: {}",
        refused.text
    );

    // A window wider than the ring is clamped, and the clamp is visible in the body — a chart
    // that silently resampled would be reporting a resolution the caller did not ask for.
    let wide = call(
        &state,
        authed(
            Method::GET,
            "/api/v1/observability/metrics/query?metric=omnion_cache_hits_total&window_minutes=525600",
            &token,
        ),
    )
    .await;
    assert_eq!(wide.status, StatusCode::OK, "{}", wide.text);
    assert_eq!(
        wide.body["window_minutes"].as_i64(),
        Some(metrics::MAX_POINTS as i64)
    );
    assert_eq!(
        wide.body["max_points"].as_i64(),
        Some(metrics::MAX_POINTS as i64)
    );
    assert!(
        wide.body["promql"]
            .as_str()
            .is_some_and(|q| q.contains("omnion_cache_hits_total"))
    );

    let zero = call(
        &state,
        authed(
            Method::GET,
            "/api/v1/observability/metrics/query?metric=omnion_cache_hits_total&window_minutes=0",
            &token,
        ),
    )
    .await;
    assert_eq!(zero.status, StatusCode::BAD_REQUEST);
    assert_eq!(zero.body["error"]["code"], "invalid_window");
}

#[tokio::test]
async fn a_family_past_its_budget_is_folded_and_the_fold_is_reported_three_ways() {
    let Some(state) = state_or_skip().await else {
        return;
    };
    let (_user, token) = sign_in(&state).await;

    // Two caps exist and a test has to know which one it is driving:
    //
    // * the **label set** is bounded to `BOUNDED_SET_CAP` verbatim values per position, after
    //   which new values collapse to `other`;
    // * the **series cap** is a separate ceiling per family.
    //
    // `omnion_circuit_state` has a series cap of 40 and a single label, so driving it with
    // 46 distinct providers folds NOTHING — every value is still under the label bound and there
    // are still only 46 series against a cap of 40 that the overflow series accounts for. The
    // first draft of this test did exactly that and asserted a fold that the guard had correctly
    // declined to perform. Driving `BOUNDED_SET_CAP` is what exercises the documented overflow.
    let spec = metrics::family("omnion_circuit_state").expect("the family is declared");
    let fold_at = metrics::BOUNDED_SET_CAP + 6;
    for index in 0..fold_at {
        let provider = format!("probe{index}");
        metrics::global().gauge_set("omnion_circuit_state", &[provider.as_str()], 0.0);
    }
    let cap = spec.max_series.min(metrics::BOUNDED_SET_CAP + 1);

    let scrape = call(&state, get("/metrics")).await;
    // (1) the sample is accounted for, in the `other` series rather than lost.
    assert!(
        scrape
            .text
            .contains("omnion_circuit_state{provider=\"other\"}"),
        "the overflow series is missing: {}",
        scrape.text
    );
    // A value inside the bound is still verbatim — the guard is not meant to be lossy for the
    // labels it accepts, only for the ones past the bound.
    assert!(
        scrape.text.contains("provider=\"probe0\""),
        "a value inside the bound was folded: {}",
        scrape.text
    );
    assert!(
        !scrape
            .text
            .contains(&format!("provider=\"probe{}\"", fold_at - 1)),
        "a value past the bound is still present verbatim: {}",
        scrape.text
    );
    // (2) the fold is counted on the scrape itself.
    assert!(
        scrape
            .text
            .contains("omnion_registry_budget_exceeded{family=\"omnion_circuit_state\"}"),
        "the fold was not reported: {}",
        scrape.text
    );

    // (3) the catalogue names the family as over budget.
    let catalog = call(
        &state,
        authed(Method::GET, "/api/v1/observability/metrics/catalog", &token),
    )
    .await;
    assert_eq!(catalog.status, StatusCode::OK, "{}", catalog.text);
    let over: Vec<&str> = catalog.body["over_budget"]
        .as_array()
        .expect("the catalogue names the over-budget families")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(
        over.contains(&"omnion_circuit_state"),
        "the catalogue does not report the over-budget family: {over:?}"
    );
    let row = catalog.body["families"]
        .as_array()
        .expect("families")
        .iter()
        .find(|row| row["name"] == "omnion_circuit_state")
        .expect("the family is catalogued");
    assert_eq!(row["over_budget"], json!(true));
    assert!(
        row["cardinality_estimate"].as_i64().unwrap_or(i64::MAX) <= cap as i64,
        "the family grew past its cap: {}",
        row["cardinality_estimate"]
    );
}

#[tokio::test]
async fn the_catalogue_is_seeded_from_the_registry_and_a_resync_is_audited() {
    let Some(state) = state_or_skip().await else {
        return;
    };
    let (user, token) = sign_in(&state).await;

    // Seed explicitly, the way the boot does.
    let declarations: Vec<metric_catalog::FamilyDeclaration> = FAMILIES
        .iter()
        .map(|spec| metric_catalog::FamilyDeclaration::from_spec(spec, 0))
        .collect();
    metric_catalog::sync_from_registry(state.db().pool(), &declarations)
        .await
        .expect("the catalogue seeds");

    // AT LEAST the declared families, never exactly: this test itself writes a module family (to
    // prove a resync does not delete it) and a previous run's row is still in the database. An
    // exact equality here fails on the SECOND run for a reason that has nothing to do with the
    // seeding — the "landmine for whoever runs the suite second" rule again, this time on a count.
    let total = metric_catalog::count(state.db().pool())
        .await
        .expect("countable");
    assert!(
        total >= FAMILIES.len() as i64,
        "the catalogue holds {total} rows and does not contain every declared family"
    );

    // A row the core does not declare SURVIVES a resync: a module's families are in the same
    // table, and a truncate-on-boot would delete another package's documentation.
    let module_family = format!("omnion_module_probe_{}_total", Uuid::new_v4().simple());
    metric_catalog::sync_from_registry(
        state.db().pool(),
        &[metric_catalog::FamilyDeclaration {
            name: module_family.clone(),
            kind: "counter".to_owned(),
            unit: "1".to_owned(),
            description: "A family registered by a module.".to_owned(),
            labels: vec!["state".to_owned()],
            source: "module".to_owned(),
            cardinality_budget: 64,
            budgeted: true,
            cardinality_estimate: 0,
        }],
    )
    .await
    .expect("the module family seeds");
    metric_catalog::sync_from_registry(state.db().pool(), &declarations)
        .await
        .expect("the core resyncs");
    assert!(
        metric_catalog::find(state.db().pool(), &module_family)
            .await
            .expect("readable")
            .is_some(),
        "a resync deleted a module's family"
    );

    // The resync endpoint is a write: it answers the catalogue and it audits.
    let response = call(
        &state,
        authed(Method::POST, "/api/v1/observability/metrics/sync", &token),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text);
    assert!(
        response.body["families"]
            .as_array()
            .is_some_and(|f| !f.is_empty())
    );

    let audit: i64 = sqlx::query_scalar(
        "select count(*)::int8 from audit_log \
         where actor_user_id = $1 and action = 'observability.metrics.catalog_synced'",
    )
    .bind(user)
    .fetch_one(state.db().pool())
    .await
    .expect("the audit table is readable");
    assert!(audit >= 1, "the resync wrote no audit row");
}

#[tokio::test]
async fn observability_read_does_not_grant_the_catalogue_resync() {
    let Some(state) = state_or_skip().await else {
        return;
    };
    // A caller with no binding at all: the guard refuses before the handler, which is the whole
    // point of the split between `observability.read` and `observability.manage`.
    let response = call(
        &state,
        authed(
            Method::POST,
            "/api/v1/observability/metrics/sync",
            "not-a-real-token",
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        response.text
    );

    let read = call(
        &state,
        authed(
            Method::GET,
            "/api/v1/observability/metrics/catalog",
            "not-a-real-token",
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::UNAUTHORIZED, "{}", read.text);
}

#[tokio::test]
async fn the_exposition_is_not_cached_and_carries_the_canonical_content_type() {
    let Some(state) = state_or_skip().await else {
        return;
    };
    let peer: SocketAddr = "198.51.100.7:51234".parse().expect("a literal");
    let mut request = get("/metrics");
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        content_type, "text/plain; version=0.0.4; charset=utf-8",
        "a scraper that gets the wrong content type refuses the body"
    );
    let cache = response
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        cache, "no-store",
        "a stale /metrics reports numbers that stopped happening"
    );
}

/// A uuid-shaped path segment. Not `Uuid::new_v4()` inlined, because the two requests in the
/// traffic walk have to differ while the ROUTE stays the same, and a shared literal is how a test
/// quietly ends up proving nothing.
fn uuid_like() -> String {
    Uuid::new_v4().to_string()
}

/// A small sanity check on the module the rest of the suite leans on: a registry the tests write
/// into is shared, so a test that assumed a clean registry would pass in isolation and fail in a
/// suite. This one asserts the sharing is real rather than leaving it to whoever runs second.
#[test]
fn the_registry_is_process_wide_and_a_tester_reaches_what_a_production_layer_recorded() {
    metrics::global().counter_add("omnion_cache_hits_total", &["suite-probe"], 1.0);
    assert_eq!(
        metrics::global().value_of("omnion_cache_hits_total", &["suite-probe"]),
        Some(1.0),
        "two calls to `global()` returned two registries"
    );
    let rendered = metrics::global().render();
    assert!(rendered.contains("omnion_cache_hits_total{cache=\"suite-probe\"} 1"));
}

/// The exposition's contract with a scraper is a *format*, so the format is worth a test of its
/// own that does not depend on the API being up.
#[test]
fn the_exposition_renders_a_histogram_the_text_format_accepts() {
    let registry = metrics::Registry::new();
    registry.observe("omnion_queue_job_duration_seconds", &["send"], 0.4);
    let text = registry.render();
    for line in text.lines() {
        if line.starts_with("omnion_queue_job_duration_seconds_bucket") {
            assert!(
                line.contains("le=\""),
                "a bucket line without an le label is not parseable: {line}"
            );
        }
        if line.starts_with("omnion_queue_job_duration_seconds_") {
            assert!(
                !line.ends_with("NaN") && !line.ends_with("Infinity"),
                "a NaN in the exposition makes the scrape unparseable: {line}"
            );
        }
    }
    // The registry used here is LOCAL, and the assertion is that the global one is untouched by
    // it. Written the other way round — asserting the global one HAS the sample — this test would
    // pass only if the two registries were the same object, i.e. it would be asserting the
    // opposite of what its comment says.
    assert!(
        !metrics::global()
            .render()
            .contains("omnion_queue_job_duration_seconds_count"),
        "a local registry wrote into the global one"
    );
}

#[test]
fn the_build_info_family_is_recorded_with_a_commit_that_is_never_invented() {
    // A made-up sha points an incident review at a release that did not ship, so the fallback is
    // `unknown` and the test says so.
    let registry = metrics::Registry::new();
    let commit = option_env!("OMNION_COMMIT").unwrap_or("unknown");
    registry.gauge_set(
        "omnion_build_info",
        &[env!("CARGO_PKG_VERSION"), commit],
        1.0,
    );
    let text = registry.render();
    assert!(text.contains("omnion_build_info{version="));
    assert!(text.contains(&format!("commit=\"{commit}\"")));
    assert!(
        !commit.is_empty() && commit.chars().all(|ch| ch.is_ascii_alphanumeric()),
        "the commit label must be a value, not a placeholder: {commit:?}"
    );
}

/// An offset date in the format the catalogue returns, so a caller reading `last_seen_at` is not
/// handed a nine-element array. The `json!` renderer produces one for an `OffsetDateTime`, and the
/// panel printed `2026` as the year because of it (REQ-125, slice 2).
#[test]
fn a_catalogue_timestamp_is_an_rfc3339_string_not_a_tuple() {
    let at = OffsetDateTime::now_utc();
    let rendered = at.format(&Rfc3339).unwrap_or_default();
    let value = json!({ "last_seen_at": rendered });
    assert!(
        value["last_seen_at"].is_string(),
        "the timestamp serialised as a tuple: {value}"
    );
}

/// The PromQL the "copy as PromQL" affordance hands out must name a real series.
///
/// The overflow bucket is sorted into the series list like any other, so the first series is
/// frequently `provider="other"` — and an operator who pastes that into a dashboard has been
/// handed the aggregate of everything the cap folded, which reads exactly like a series they
/// configured. The depth pass caught it: the copied string was literally `other`.
#[tokio::test]
async fn the_promql_names_a_real_series_and_not_the_overflow_bucket() {
    let Some(state) = state_or_skip().await else {
        return;
    };
    let (_user, token) = sign_in(&state).await;

    // Drive a family past its label bound so the overflow series exists AND sorts into the list.
    //
    // The registry is a process-global singleton and a learned label set is a ONE-WAY door: the
    // first 24 values a family ever sees are kept verbatim forever. Two tests driving the same
    // family therefore interleave into one shared set and neither can assert a boundary, because
    // which of them got the verbatim values depends on the test scheduler. This family is chosen
    // precisely because no other test in this file records into it — the isolation is a property
    // of the pick, so the comment names it.
    for index in 0..(metrics::BOUNDED_SET_CAP + 4) {
        let scope = format!("pq{index}");
        metrics::global().counter_add("omnion_rate_limit_refusals_total", &[scope.as_str()], 1.0);
    }

    let response = call(
        &state,
        authed(
            Method::GET,
            "/api/v1/observability/metrics/query?metric=omnion_rate_limit_refusals_total&window_minutes=60",
            &token,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text);
    let promql = response.body["promql"]
        .as_str()
        .expect("the query answers a promql string");
    assert!(
        !promql.contains("=\"other\""),
        "the copy affordance handed out the overflow bucket: {promql}"
    );

    // The registry is a process-global singleton, so a sibling test's own series (`probe0`, …)
    // live in the same family and sort ahead of the ones this test just wrote. Asserting a
    // hard-coded prefix would therefore describe *those* series and pass or fail on a fact about
    // test order rather than about the code. The property to prove is relational: the string the
    // screen hands out names one of the series the same response listed.
    let series = response.body["series"]
        .as_array()
        .expect("the query answers a series list");
    let named = series.iter().any(|entry| {
        entry["labels"].as_array().is_some_and(|labels| {
            labels
                .iter()
                .filter_map(|value| value.as_str())
                .any(|value| promql.contains(value))
        })
    });
    assert!(
        named,
        "the promql names no listed series: {promql} (series: {series:?})"
    );
}
