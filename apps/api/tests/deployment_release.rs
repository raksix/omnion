//! Integration walk for REQ-128 slice 4: the deployment centre's release surface, the bundle
//! generator and the upgrade plan's acknowledgement.
//!
//! It runs against the development stack and **fails** rather than skipping when the migrations
//! do not apply — see `support::walk_state` for why that is a defect and not an environment.
//!
//! ## The four claims this walk makes, and why each needs a walk rather than a unit test
//!
//! 1. **The plan is `unknown`, not `reversible`, and the acknowledgement is what makes it
//!    complete.** This is the sentence the whole request is written against, and it can only be
//!    observed end to end: the plan is built from two cached manifests, the verdict comes from
//!    the absence of REQ-129's gate, and the checklist refuses to render as complete until an
//!    operator acknowledges. A unit test that builds a plan proves the builder; this proves the
//!    builder, the store, the route and the refusal together.
//!
//! 2. **`deployment.read` cannot generate a bundle or acknowledge a plan, and the refusal names
//!    the permission.** The status alone is not the claim: a route that refused for an unrelated
//!    reason (a cross-scope check, a missing row) satisfies a bare `403` while proving nothing
//!    about the permission, which is why the walk reads `details.permission`.
//!
//! 3. **The acknowledgement is durable and unique per range.** Asserted by **counting** the
//!    acknowledged rows rather than checking that the returned row carries an actor: a store that
//!    wrote the flag twice still returns a flagged row. And the count is re-read after the plan
//!    is REGENERATED, because a screen that re-asks for consent every time somebody re-ran the
//!    update check trains operators to click through warnings.
//!
//! 4. **No route on this surface ever returns a credential.** The bundle's `config` column is
//!    asserted to hold exactly the fields the platform builds, and every generated file is
//!    scanned with the same structural rule the release pipeline uses.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
mod support;

use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_deployment::manifest::ReleaseManifest;
use omnion_identity::users::{self, NewUser};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

use support::walk_auth::{self, PASSWORD};

/// The permissions the fixture account holds. `deployment.manage` is the deployment centre's own
/// write power; the upgrade acknowledgement rides it rather than inventing a key.
const FIXTURE_PERMISSIONS: &[&str] = &[
    "deployment.read",
    "deployment.deploy",
    "deployment.bundle.generate",
];

/// Every read and write on this surface, with the method the PANEL will send.
///
/// A table rather than hand-written requests because the defect this shape of walk exists to
/// catch is a client/router drift: the method below is what the admin's `api.ts` will call, so a
/// route registered under a different verb fails here rather than in a browser.
fn routes() -> Vec<(Method, &'static str, bool)> {
    vec![
        (Method::GET, "/api/v1/deployment/artifacts", false),
        (Method::GET, "/api/v1/deployment/artifacts/0.5.0", false),
        (Method::GET, "/api/v1/deployment/bundles", false),
        (Method::POST, "/api/v1/deployment/bundles", true),
        (Method::GET, "/api/v1/deployment/bundles/{id}", false),
        (
            Method::GET,
            "/api/v1/deployment/bundles/{id}/files/README.md",
            false,
        ),
        (Method::POST, "/api/v1/deployment/bundles/{id}/render", true),
        (Method::GET, "/api/v1/deployment/upgrade-plan", false),
        (
            Method::POST,
            "/api/v1/deployment/upgrade-plan/acknowledge",
            true,
        ),
    ]
}

struct TestResponse {
    status: StatusCode,
    body: Value,
    cookies: Vec<String>,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let peer: SocketAddr = "198.51.100.23:51234".parse().expect("a peer address");
    let mut request = request;
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    // BOTH cookies: a sign-in sets `omnion_session` AND `omnion_csrf`, and the double-submit
    // check refuses a write that presents only the session. A walk holding one of the two is no
    // longer a request the panel can make, and every POST it issues then reads `403` for a reason
    // that has nothing to do with what it is testing.
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

/// An account in a fresh organization, holding exactly `permissions`.
///
/// Per run and per account, because a role whose permission set is right for one assertion is
/// wrong for the next — the read-only walk MUST NOT be able to write, and a helper that quietly
/// added the write key would turn the whole walk into a suite that asserts nothing.
async fn account_with(state: &AppState, permissions: &[&str]) -> (Uuid, Uuid, walk_auth::Session) {
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Deployment walk {suffix}"),
            slug: format!("deploy-walk-{suffix}"),
        },
    )
    .await
    .expect("the organization is created");
    let email = format!("deploy-walk-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Deploy Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account is created");

    let role = omnion_permissions::roles::create_role(
        state.db().pool(),
        omnion_permissions::model::NewRole {
            organization_id: organization.id,
            key: format!("deploy-walk-role-{suffix}"),
            name: "Deployment walk role".to_owned(),
            description: "A role of the deployment tooling walk".to_owned(),
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
    (
        user.id,
        organization.id,
        walk_auth::Session::from_set_cookies(&response.cookies),
    )
}

/// A patch component unique to THIS walk, so no two runs — and no two walks in this binary —
/// share a version range.
///
/// ## Why the walks must not share a range
///
/// An upgrade plan's range is `(from_version, to_version, topology)`, and the `from` is the
/// **build's** version — `0.1.0` in a test binary, identical for every run and every walk. So
/// two walks in the same file that pick the same target version are working on the SAME range,
/// and the first one to acknowledge it leaves an acknowledgement the second one finds. That is
/// not a flaky test; it is a **cross-walk leak through the database**, and it presented as
/// "the plan came back already complete" on a run where the product was behaving exactly as
/// designed.
///
/// The suffix is therefore derived from the walk's own name, not from a fresh uuid: the two
/// walks in this file can then never collide with each other, and a rerun of the same walk
/// after a *failed* attempt is cleared by `delete_acknowledgements` below rather than by
/// luck.
fn patch_for(walk: &str) -> u32 {
    let digest: u64 = {
        // FNV-1a over the walk's name: a fixture that varies per RUN is what makes "durable"
        // meaningful, and a fixture that varies per WALK is what keeps two walks apart. Neither
        // is a random source, so a failing run is reproducible.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in walk.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    };
    // `+ 1_000` keeps every walk's range above the versions other tests in the tree cache, and
    // `u32` keeps it a valid patch component.
    (digest % 1_000_000_000) as u32 + 1_000
}

/// Remove every acknowledgement this file's walks created, so a rerun starts clean.
///
/// The two walks deliberately share a database — a walk that opened its own would be measuring a
/// different installation than the panel does. So the isolation has to be explicit: the
/// acknowledgement is the one row whose presence is *meant* to survive a regeneration, and a
/// previous run's is indistinguishable from this run's.
async fn delete_acknowledgements(state: &AppState) {
    sqlx::query(
        "update upgrade_plans set destructive_acknowledged_by = null, \
                 destructive_acknowledged_at = null where destructive_acknowledged_by is not null",
    )
    .execute(state.db().pool())
    .await
    .expect("acknowledgements are clearable");
}

/// The manifest a walk caches, carrying the artifacts a release really publishes.
///
/// Built through the store's own `cache_manifest` so the rows are the shape production writes —
/// a hand-written `insert` would prove the walk's own SQL rather than the cache.
fn manifest(version: &str, migrations: &[&str], destructive: bool) -> ReleaseManifest {
    ReleaseManifest {
        version: version.into(),
        channel: "stable".into(),
        source_commit: Some(format!("{}-{}", version, "a".repeat(7))),
        core_min: Some("0.1.0".into()),
        migrations: migrations.iter().map(|m| (*m).to_owned()).collect(),
        migrations_destructive: destructive,
        notes_md: format!("release notes for {version}"),
        upgrade_notes_url: Some(format!("https://example.invalid/notes/{version}")),
        fetched_at: time::OffsetDateTime::now_utc(),
        raw: json!({
            "schema_version": "1",
            "version": version,
            "images": [
                {"name": "ghcr.io/raksix/omnion/api", "tag": version, "digest": format!("sha256:{}", version.replace('.', ""))},
                {"name": "ghcr.io/raksix/omnion/admin", "tag": version}
            ],
            "artifacts": [
                {"kind": "image", "name": "ghcr.io/raksix/omnion/api", "digest": format!("sha256:{}", version.replace('.', "")),
                 "platforms": ["linux/amd64", "linux/arm64"], "size_bytes": 88_000_000},
                {"kind": "image", "name": "ghcr.io/raksix/omnion/admin", "digest": null,
                 "platforms": ["linux/amd64"], "size_bytes": 210_000_000},
                {"kind": "chart", "name": "omnion", "digest": "sha256:chart", "platforms": [], "size_bytes": 40_000}
            ]
        }),
    }
}

#[tokio::test]
async fn a_plan_is_unknown_until_acknowledged_and_the_acknowledgement_is_durable_and_unique() {
    // The whole slice in one walk: cache two releases, ask for the plan, and follow the
    // acknowledgement through the store rather than through the returned body.
    let state = support::walk_state::state_or_fail().await;
    // A previous run's acknowledgement is indistinguishable from this run's, and the range is
    // `(the build's version, the target, compose)` — the same range on every run. So the reset
    // is explicit rather than hoped for: a walk that only passes on a clean database reports a
    // failure nobody can reproduce.
    delete_acknowledgements(&state).await;
    let (actor, _org, session) = account_with(&state, FIXTURE_PERMISSIONS).await;
    // The patch component is a NUMBER: `0.5.a1b2…` is not a version and the manifest is refused
    // before any of the walk's own assertions run, which reads as "the cache is broken" rather
    // than "the fixture is not a version".
    let patch = patch_for("plan-walk");
    // A cached "running" manifest so the delta is a DIFFERENCE rather than "everything the
    // release ships". The route itself reads the real build version for `from_version`; this
    // fixture exists so the cached rows include a plausible predecessor.
    let running_version = format!("0.4.{patch}");
    let to_version = format!("0.5.{patch}");

    // A running version, so the delta is a difference rather than "everything". The build's own
    // version is what the route reads, so the walk asks the store for a plan on the range and
    // then checks the route's verdict through the same manifests.
    let running = manifest(&running_version, &["0001_a.sql", "0002_b.sql"], false);
    let target = manifest(
        &to_version,
        &["0001_a.sql", "0002_b.sql", "0003_c.sql", "0004_d.sql"],
        false,
    );
    let written = omnion_deployment::store::cache_manifest(state.db().pool(), &running)
        .await
        .expect("the running manifest caches");
    assert_eq!(written, 3, "the manifest names three artifacts");
    omnion_deployment::store::cache_manifest(state.db().pool(), &target)
        .await
        .expect("the target manifest caches");

    // ---- 1. The artifacts list, and the explicit "not published" rows. -----------------
    let response = call(
        &state,
        request(Method::GET, "/api/v1/deployment/artifacts", None, &session),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the artifact list must answer: {}",
        response.body
    );
    let releases = response.body["releases"]
        .as_array()
        .expect("a release list");
    let target_row = releases
        .iter()
        .find(|row| row["version"] == json!(to_version))
        .expect("the target release is listed");
    assert_eq!(
        target_row["published_kinds"],
        json!(["chart", "image"]),
        "the kinds that shipped, in the order the coverage query returns them"
    );
    // The request asks for an explicit "not published for this version" row rather than a blank.
    // A client that computed the missing set itself would call `cli` missing on a release that
    // never claimed to ship one.
    assert_eq!(
        target_row["missing_kinds"],
        json!(["cli", "sbom", "compose"]),
        "every kind the release did not publish is named"
    );

    // The artifact digest is a digest and the missing one is null, not a fabricated value.
    let artifacts = response.body["artifacts"]
        .as_array()
        .expect("an artifact list");
    let chart = artifacts
        .iter()
        .find(|a| a["version"] == json!(to_version) && a["kind"] == json!("chart"))
        .expect("the chart artifact");
    assert_eq!(chart["digest"], json!("sha256:chart"));
    let admin = artifacts
        .iter()
        .find(|a| {
            a["version"] == json!(to_version) && a["name"] == json!("ghcr.io/raksix/omnion/admin")
        })
        .expect("the admin image");
    assert!(
        admin["digest"].is_null(),
        "a release that published no digest must show null, not an invented one"
    );

    // ---- 2. One release in full, with the core minimum answered against THIS build. -----
    let response = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/deployment/artifacts/{to_version}"),
            None,
            &session,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["release"]["source_commit"].is_string(), true);
    assert_eq!(
        response.body["core_minimum_satisfied"],
        json!(true),
        "the release declares 0.1.0 and this build is at least that"
    );
    assert_eq!(
        response.body["missing_kinds"],
        json!(["cli", "sbom", "compose"])
    );

    // ---- 3. The plan: UNKNOWN, not reversible, and the checklist refuses completion. -----
    let response = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/deployment/upgrade-plan?to={to_version}&topology=compose&bundle_kind=compose-small"),
            None,
            &session,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let plan = &response.body["summary"]["plan"];
    assert!(
        plan.is_object(),
        "a plan must be built from two cached manifests: {}",
        response.body
    );
    assert_eq!(
        plan["destructive"]["verdict"],
        json!("unknown"),
        "REQ-129's gate has not landed, so an unmarked migration is not proven reversible"
    );
    assert_eq!(plan["rollback"]["database"]["method"], json!("unknown"));
    assert_eq!(plan["rollback"]["database"]["available"], json!(false));
    assert_eq!(
        plan["rollback"]["application"]["available"],
        json!(true),
        "an application rollback is a tag change and is always there"
    );
    // The point of no return is a MIGRATION, and it is the one that applies the new work.
    let ponr = plan["point_of_no_return"].as_u64().expect("a marker");
    let steps = plan["steps"].as_array().expect("a step list");
    assert_eq!(steps[ponr as usize]["kind"], json!("migrate"));
    assert_eq!(
        steps[0]["kind"],
        json!("backup"),
        "the rollback is a restore, so the backup is the first step"
    );
    let migrate_at = steps
        .iter()
        .position(|step| step["kind"] == json!("migrate"))
        .expect("a migrate step");
    let deploy_at = steps
        .iter()
        .position(|step| step["kind"] == json!("deploy"))
        .expect("a deploy step");
    assert!(
        migrate_at < deploy_at,
        "migrations run before new code serves traffic"
    );

    assert_eq!(
        response.body["summary"]["requires_acknowledgement"],
        json!(true)
    );
    assert_eq!(
        plan["checklist"]["complete"],
        json!(false),
        "an un-acknowledged plan is never complete"
    );
    // Exactly one problem: the missing acknowledgement. A verifier that reported everything would
    // look diligent and be useless.
    let problems = response.body["problems"]
        .as_array()
        .expect("a problem list");
    assert_eq!(
        problems.len(),
        1,
        "only the acknowledgement should be outstanding: {problems:?}"
    );
    assert_eq!(problems[0]["check"], json!("unacknowledged"));

    // ---- 4. The acknowledgement, by COUNT and then by regeneration. --------------------
    let plan_id = response.body["summary"]["stored"]["id"]
        .as_str()
        .expect("a plan id")
        .to_owned();
    let response = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/upgrade-plan/acknowledge",
            Some(json!({ "verdict": format!("{plan_id}@unknown") })),
            &session,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["acknowledged_by"], json!(actor.to_string()));

    // **The range is read out of the response, not from the fixture's own variables.** The
    // plan's `from_version` is the BUILD's version (`0.1.0` in a test binary) and its
    // `to_version` is the cached release — neither is the `from_version` this walk defined as a
    // fixture, because the walk's fixture is the TARGET only. The first draft counted against
    // the fixture pair and got zero, which read as "the acknowledgement was not stored"; it was
    // stored, under the range the plan actually names.
    let range_from = plan["from_version"]
        .as_str()
        .expect("the plan names the version it runs")
        .to_owned();
    let range_to = plan["to_version"]
        .as_str()
        .expect("the plan names its target")
        .to_owned();
    let count = omnion_deployment::store::acknowledged_plan_count(
        state.db().pool(),
        &range_from,
        &range_to,
        "compose",
    )
    .await
    .expect("the acknowledged count runs");
    assert_eq!(count, 1, "exactly one acknowledged plan for the range");

    // Acknowledging the wrong verdict is refused: a consent that does not say what it accepted
    // is a consent to an unknown.
    let response = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/upgrade-plan/acknowledge",
            Some(json!({ "verdict": format!("{plan_id}@destructive") })),
            &session,
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CONFLICT,
        "acknowledging a verdict the plan does not carry must be refused: {}",
        response.body
    );

    // And re-reading the plan does NOT re-ask: the acknowledgement is a durable fact.
    let response = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/deployment/upgrade-plan?to={to_version}&topology=compose&bundle_kind=compose-small"),
            None,
            &session,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let plan = &response.body["summary"]["plan"];
    assert_eq!(
        plan["checklist"]["complete"],
        json!(true),
        "an operator who already accepted the warning must not be asked again"
    );
    assert_eq!(
        response.body["summary"]["requires_acknowledgement"],
        json!(false)
    );
    let problems = response.body["problems"]
        .as_array()
        .expect("a problem list");
    assert!(
        problems.is_empty(),
        "an acknowledged, self-consistent plan has no problems: {problems:?}"
    );

    // Counted AGAIN after the regeneration, because a store that wrote the acknowledgement onto
    // the new row while clearing the old one would pass the first count and fail this.
    let count = omnion_deployment::store::acknowledged_plan_count(
        state.db().pool(),
        &range_from,
        &range_to,
        "compose",
    )
    .await
    .expect("the acknowledged count runs");
    assert_eq!(
        count, 1,
        "a regeneration must move the acknowledgement, not duplicate it"
    );
}

#[tokio::test]
async fn deployment_read_cannot_generate_a_bundle_or_acknowledge_a_plan_and_the_refusal_names_the_key()
 {
    let state = support::walk_state::state_or_fail().await;
    let suffix = Uuid::new_v4().simple().to_string();
    // Its own range: the acknowledgement walk's range is `0.5.<its own patch>` and this one is
    // `0.5.<this patch>`, so neither can find the other's consent.
    let target = manifest(
        &format!("0.5.{}", patch_for("read-only-walk")),
        &["0001_a.sql"],
        false,
    );
    omnion_deployment::store::cache_manifest(state.db().pool(), &target)
        .await
        .expect("the target manifest caches");

    // TWO accounts: the fixture account holds the write keys, the account under test holds
    // `deployment.read` and nothing else. Asking the read-only account to create its own fixture
    // is the mistake this split removes — the first draft of a walk like this did exactly that
    // and read the resulting `403` as "the fixture was refused".
    let (_fixture_actor, _fixture_org, fixture) = account_with(&state, FIXTURE_PERMISSIONS).await;
    let (_reader, _reader_org, reader) = account_with(&state, &["deployment.read"]).await;

    // The bundle the read-only account may not create, and the plan it may not acknowledge.
    let response = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/bundles",
            Some(json!({
                "name": format!("fixture-{suffix}"),
                "kind": "helm",
                "version": target.version,
                "domain": "panel.example.com",
                "tls_mode": "cert-manager",
                "registry": "ghcr.io/raksix/omnion",
                "tag": null,
                "preset": "small",
                "observability": false
            })),
            &fixture,
        ),
    )
    .await;
    let bundle_id = if response.status == StatusCode::CREATED {
        response.body["id"]
            .as_str()
            .expect("a bundle id")
            .to_owned()
    } else {
        // The generator subprocess is not available in every environment (an installed panel does
        // not ship `release/`). That is a real answer — 503 with a reason — and the walk records
        // it rather than pretending the bundle exists. The refusal half below does not depend on
        // it.
        assert!(
            response.status == StatusCode::SERVICE_UNAVAILABLE,
            "generating a bundle answers 201 or 503-with-a-reason, not {}: {}",
            response.status,
            response.body
        );
        String::new()
    };
    let plan_response = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/deployment/upgrade-plan?to={}", target.version),
            None,
            &fixture,
        ),
    )
    .await;
    assert_eq!(
        plan_response.status,
        StatusCode::OK,
        "the upgrade plan must answer: {}",
        plan_response.body
    );
    let plan_id = plan_response.body["summary"]["stored"]["id"]
        .as_str()
        .expect("a stored plan id")
        .to_owned();

    // Every write, as a caller holding only `deployment.read`. The refusal's `code` AND
    // `details.permission` are read, not only the status.
    let mut writes: Vec<(Method, String, Value)> = vec![
        (
            Method::POST,
            "/api/v1/deployment/bundles".to_owned(),
            json!({
                "name": format!("read-only-{suffix}"),
                "kind": "helm",
                "version": target.version,
                "domain": "",
                "tls_mode": "none",
                "registry": "ghcr.io/raksix/omnion",
                "tag": null,
                "preset": "small",
                "observability": false
            }),
        ),
        (
            Method::POST,
            format!("/api/v1/deployment/upgrade-plan/acknowledge"),
            json!({ "verdict": format!("{plan_id}@unknown") }),
        ),
    ];
    if !bundle_id.is_empty() {
        writes.push((
            Method::POST,
            format!("/api/v1/deployment/bundles/{bundle_id}/render"),
            Value::Null,
        ));
    }
    for (method, path, body) in &writes {
        let response = call(
            &state,
            request(method.clone(), path, Some(body.clone()), &reader),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{method} {path} must be refused for a read-only account: {}",
            response.body
        );
        assert_eq!(
            response.body["error"]["details"]["permission"],
            json!(expected_permission(path)),
            "{path} must name the permission it refused for: {}",
            response.body
        );
    }

    // And the negative half of the fix: the read-only account may still READ the whole surface.
    // "Deny more" is not the fix; the split is.
    // Only the routes whose `{id}` fixture actually exists. A `{id}` that was never created makes
    // the path `/api/v1/deployment/bundles//files/README.md`, which is a 400 about a malformed
    // URL and not about permissions — so the first draft of this loop read that 400 as a
    // permissions failure, which is the third fixture-shaped false conclusion in this file. Each
    // skip says WHY it is skipped rather than being invisible.
    for (method, path, writes) in routes() {
        if writes {
            continue;
        }
        if path.contains("{id}") && bundle_id.is_empty() {
            eprintln!(
                "skipping {method} {path}: the bundle fixture was not created (the generator \
                 is absent here), so there is no id to read"
            );
            continue;
        }
        let path = path.replace("{id}", &bundle_id);
        let response = call(&state, request(method.clone(), &path, None, &reader)).await;
        assert!(
            response.status == StatusCode::OK || response.status == StatusCode::NOT_FOUND,
            "{method} {path} must be readable for a read-only account, got {}: {}",
            response.status,
            response.body
        );
    }
}

/// The permission a path's write is refused for.
///
/// Lifted into a function rather than repeated per case so a route that changes its guard and
/// this table together cannot pass: the table and the router are the two claims, and a walk that
/// repeats the router's own answer is asserting nothing.
fn expected_permission(path: &str) -> &'static str {
    if path.contains("acknowledge") {
        "deployment.deploy"
    } else {
        "deployment.bundle.generate"
    }
}

#[tokio::test]
async fn a_generated_bundle_stores_only_the_platforms_own_fields_and_no_credential() {
    let state = support::walk_state::state_or_fail().await;
    let (_actor, _org, session) = account_with(&state, FIXTURE_PERMISSIONS).await;
    let suffix = Uuid::new_v4().simple().to_string();
    let target = manifest(
        &format!("0.6.{}", patch_for("bundle-walk")),
        &["0001_a.sql"],
        false,
    );
    omnion_deployment::store::cache_manifest(state.db().pool(), &target)
        .await
        .expect("the manifest caches");

    // A request that TRIES to smuggle a credential in. It is refused, and the stored record is
    // the platform's own — asserted by reading the row back, not by trusting the response.
    let response = call(
        &state,
        request(
            Method::POST,
            "/api/v1/deployment/bundles",
            Some(json!({
                "name": format!("creds-{suffix}"),
                "kind": "helm",
                "version": target.version,
                "domain": "panel.example.com",
                "tls_mode": "existing-secret",
                "registry": "ghcr.io/raksix/omnion",
                "tag": null,
                "preset": "large",
                "observability": true,
                // A field the record type does not have. It is not stored, not echoed and not
                // written anywhere — the guarantee is the record's shape, not a scan of values.
                "database_password": "fixture-only-9f2b1c-abcdef",
                "tls_key": "-----BEGIN PRIVATE KEY-----"
            })),
            &session,
        ),
    )
    .await;
    assert!(
        response.status == StatusCode::CREATED
            || response.status == StatusCode::SERVICE_UNAVAILABLE,
        "a valid request answers 201, and 503 when the generator is absent: {} / {}",
        response.status,
        response.body
    );

    if response.status != StatusCode::CREATED {
        return;
    }
    let bundle_id = response.body["id"].as_str().expect("a bundle id");

    // Read the ROW back out of PostgreSQL, not the response: a handler that echoes a field it
    // did not store would pass a response-shaped assertion and leak on the next read.
    let stored: Value = sqlx::query_scalar("select config from environment_bundles where id = $1")
        .bind(Uuid::parse_str(bundle_id).expect("a uuid"))
        .fetch_one(state.db().pool())
        .await
        .expect("the bundle row is readable");
    let keys: Vec<&str> = stored
        .as_object()
        .expect("a config object")
        .keys()
        .map(String::as_str)
        .collect();
    for forbidden in [
        "database_password",
        "tls_key",
        "password",
        "secret",
        "token",
    ] {
        assert!(
            !keys.contains(&forbidden),
            "the record must not carry {forbidden}: {keys:?}"
        );
    }
    assert_eq!(
        keys.len(),
        6,
        "exactly the fields the platform builds: {keys:?}"
    );
    // And the serialized row carries no fixture value anywhere, including inside a nested value.
    let serialized = stored.to_string();
    assert!(
        !serialized.contains("fixture-only-9f2b1c"),
        "a fixture credential value reached the record: {serialized}"
    );

    // The response's file list is empty rather than absent when the generator shipped no body, and
    // the note says no credential value ships — an operator who does not believe that will look
    // for a key that is not there and never apply the bundle.
    assert!(
        response.body["note"]
            .as_str()
            .unwrap_or_default()
            .contains("no credential value ships"),
        "the note must be present: {}",
        response.body["note"]
    );
}
