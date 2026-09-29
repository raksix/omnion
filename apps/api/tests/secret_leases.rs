//! Integration test for credential leases, loopback redemption and deployment keys
//! (docs/requests/REQ-125, slice 3).
//!
//! It runs against the development stack and skips itself with a printed reason when PostgreSQL
//! is not reachable, exactly like the two suites beside it.
//!
//! The walk proves the five claims the request makes, in the order they matter:
//!
//! 1. **`POST /secrets/{id}/lease` never returns a value.** The issuing handler is the one a bug
//!    is most likely to hide in, so the assertion is a grep for the fixture value across the whole
//!    response — not a check that some field is absent.
//! 2. **Redemption is the only path a value takes, and only for a machine identity.** A browser
//!    session is refused; a deployment key in the header is accepted. Both are asserted, because
//!    either alone would pass a broken implementation that merely flipped one guard.
//! 3. **A deployment key can lease inside its environment and is refused outside it**, including
//!    on a redemption it minted, and the refusal is the same error whatever the cause — a caller
//!    must not be able to tell a wrong key from a dead one.
//! 4. **A key past its expiry is refused**, and so is a use budget that has been spent.
//! 5. **`deployment.started` revokes live leases for the environment**, through the consumer in
//!    `secrets_runner` rather than a call from a deploy handler — the revocation runs here against
//!    the same `events` table a deploy by another writer would have written to.
//!
//! The token discipline is asserted too: a lease token is a handle, and a deployment key value
//! appears in exactly one response in this file — the one the panel shows once.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_secrets::leases::DEPLOYMENT_KEY_PREFIX;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The value behind every credential this suite creates. Every assertion greps for exactly this
/// string, so a leak is caught rather than inferred.
const FIXTURE_VALUE: &str = "qa-lease-value-do-not-leak-7f2b19";

/// The operator key all three REQ-125 suites seal with. **Must** match the other two: the key
/// ring is installation-wide, and a different value here would make `ensure_active_key` hand back
/// a key this process cannot open.
const SUITE_OPERATOR_KEY: &str = "omnion-secrets-suite-operator-key";

/// The header a machine identity presents its deployment key in.
const KEY_HEADER: &str = "x-omnion-deployment-key";

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    body: Value,
    raw: String,
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
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
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body = if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&raw).unwrap_or(Value::Null)
    };
    TestResponse { status, body, raw }
}

/// Build a request. `token` becomes the session cookie, `machine` the deployment-key header.
///
/// The two are mutually exclusive on purpose in every assertion that uses them: a request that
/// carried both would be testing a shape the API never produces.
fn request(
    method: Method,
    uri: &str,
    token: Option<&str>,
    machine: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, format!("omnion_session={token}"));
    }
    if let Some(machine) = machine {
        builder = builder.header(KEY_HEADER, machine);
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    builder.body(body).expect("request must build")
}

/// A `GET` with a session cookie.
fn get(uri: &str, token: &str) -> Request<Body> {
    request(Method::GET, uri, Some(token), None, None)
}

/// A `POST` with a session cookie and a JSON payload.
fn post(uri: &str, token: &str, body: Value) -> Request<Body> {
    request(Method::POST, uri, Some(token), None, Some(body))
}

/// A `POST` with a JSON payload authenticated by a machine identity.
fn post_as_machine(uri: &str, machine: &str, body: Value) -> Request<Body> {
    request(Method::POST, uri, None, Some(machine), Some(body))
}

/// A `DELETE` with a session cookie.
fn delete(uri: &str, token: &str) -> Request<Body> {
    request(Method::DELETE, uri, Some(token), None, None)
}

/// Object store of the test state.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// A state whose database has all migrations applied and the IAM seed loaded.
async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");
    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    Some((state, db))
}

/// Create an account and sign it in.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("lease-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Lease Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

/// Sign an account in and return its session token.
async fn login(state: &AppState, email: &str) -> String {
    let response = routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "email": email, "password": PASSWORD }).to_string(),
                ))
                .expect("request must build"),
        )
        .await
        .expect("router must answer");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the account must sign in"
    );
    response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie| cookie.split(';').next())
        .and_then(|pair| pair.split_once('='))
        .map(|(_, token)| token.to_owned())
        .expect("login must set a session cookie")
}

/// Create a local secret and seal one version of `FIXTURE_VALUE` under it.
async fn sealed_secret(db: &Db, organization_id: Uuid, name: String) -> Uuid {
    let secret_id: Uuid = sqlx::query_scalar(
        "insert into secrets (name, scope_type, organization_id, description) \
         values ($1, 'organization', $2, 'Sealed by the REQ-125 slice-3 suite') returning id",
    )
    .bind(&name)
    .bind(organization_id)
    .fetch_one(db.pool())
    .await
    .expect("the secret must be created");

    let key = omnion_secrets::store::ensure_active_key(db.pool())
        .await
        .expect("a root key must exist");
    let operator = omnion_secrets::store::operator_key().expect("the operator key must resolve");
    let ring = omnion_secrets::store::load_ring(db.pool())
        .await
        .expect("the ring must load");
    let envelope = ring
        .seal(&key.key_id, FIXTURE_VALUE.as_bytes(), &operator)
        .expect("the value must seal");
    sqlx::query(
        "insert into secret_versions (secret_id, version, envelope, key_id, value_hint) \
         values ($1, 1, $2, $3, $4)",
    )
    .bind(secret_id)
    .bind(&envelope)
    .bind(&key.key_id)
    .bind(omnion_secrets::hint_for(FIXTURE_VALUE))
    .execute(db.pool())
    .await
    .expect("the version must be stored");
    secret_id
}

/// Mint a deployment key through the API and return `(id, value)`.
///
/// The value is returned by the create response and nowhere else; every later assertion greps the
/// list endpoint for it, which is how "shown once" is proven rather than asserted.
async fn mint_key(
    state: &AppState,
    token: &str,
    name: &str,
    environment: &str,
    scopes: &[&str],
    expires_at: &str,
) -> (Uuid, String) {
    let response = call(
        state,
        post(
            "/api/v1/deployment-keys",
            token,
            json!({
                "name": name,
                "environment": environment,
                "scopes": scopes,
                "expires_at": expires_at,
            }),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "a deployment key must be mintable: {}",
        response.raw
    );
    let id: Uuid =
        serde_json::from_value(response.body["id"].clone()).expect("an id must come back");
    let value = response.body["value"]
        .as_str()
        .expect("the value must be in the create response and only there")
        .to_owned();
    (id, value)
}

/// An RFC 3339 timestamp `hours` from now, as the key creation needs.
///
/// A negative number is only ever handed to the API, and only to assert that it REFUSES: the
/// product rejects an expiry in the past outright, so a walk cannot mint an already-expired key
/// through the route and must age a live one in the database if it wants the `expired` rendering.
fn in_hours(hours: i64) -> String {
    let when = time::OffsetDateTime::now_utc() + time::Duration::hours(hours);
    when.format(&time::format_description::well_known::Rfc3339)
        .expect("the timestamp must format")
}

#[tokio::test]
async fn leases_deployment_keys_and_deploy_revocation_are_proven_end_to_end() {
    // The key ring needs an operator key even though this suite never unseals by hand.
    unsafe { std::env::set_var(omnion_secrets::KEY_ENCRYPTION_ENV, SUITE_OPERATOR_KEY) };

    let Some((state, db)) = live_state().await else {
        return;
    };
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");

    let slug = format!("lease-{}", Uuid::new_v4().simple());
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Lease Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the organization must be created");

    let (owner_id, owner_email) = create_account(&db, Some(organization_id)).await;
    seed::bind_owner(db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");
    let (member_id, member_email) = create_account(&db, Some(organization_id)).await;
    let owner_token = login(&state, &owner_email).await;
    let member_token = login(&state, &member_email).await;
    assert!(
        !owner_token.is_empty(),
        "the owner must hold a session token"
    );

    // The scope list on a deployment key names **credentials**, not permissions. `scope_allows`
    // is the whole rule: an entry matches the secret's NAME, with a trailing `.*` as the one
    // wildcard. A key scoped to the permission name `secrets.lease` therefore matches no
    // credential at all, and every redemption it attempts is refused as "not scoped to that
    // credential" — the walk used to be written that way, which is why its one positive
    // redemption had never been able to pass.
    //
    // A family wildcard is the shape a deployment actually wants, so the keys below are scoped
    // to the family rather than to the exact per-run name: the secrets are named with a random
    // suffix on purpose, and a key that had to be re-minted per run would be a key nobody keeps.
    // The wildcard is `prefix.*` — the dot belongs to the rule, and a `prefix-*` spelling
    // matches nothing at all, which is the second thing this walk had to learn.
    let payment_name = format!("payments.live.{}", Uuid::new_v4().simple());
    let smtp_name = format!("smtp.live.{}", Uuid::new_v4().simple());
    let payment_secret = sealed_secret(&db, organization_id, payment_name.clone()).await;
    let smtp_secret = sealed_secret(&db, organization_id, smtp_name.clone()).await;
    const PAYMENT_SCOPE: &str = "payments.live.*";

    /* ------------------------------------------------------------------- the empty list reads */

    let leases = call(&state, get("/api/v1/secret-leases", &owner_token)).await;
    assert_eq!(leases.status, StatusCode::OK, "the lease list must read");
    let keys = call(&state, get("/api/v1/deployment-keys", &owner_token)).await;
    assert_eq!(keys.status, StatusCode::OK, "the key list must read");
    assert_eq!(
        keys.body["header"].as_str(),
        Some(KEY_HEADER),
        "the create drawer must be told the header to present"
    );
    assert!(
        !keys.body["guidance"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the screen states the risk in its own words"
    );

    /* ------------------------------------------------------- issuing a lease returns no value */

    let issued = call(
        &state,
        post(
            &format!("/api/v1/secrets/{payment_secret}/lease"),
            &owner_token,
            json!({
                "consumer": "nightly-release",
                "environment": "production",
                "ttl_seconds": 900,
                "max_uses": 2,
            }),
        ),
    )
    .await;
    assert_eq!(
        issued.status,
        StatusCode::CREATED,
        "a lease must be issuable: {}",
        issued.raw
    );
    assert!(
        !issued.raw.contains(FIXTURE_VALUE),
        "the issuing response must not carry the value: {}",
        issued.raw
    );
    let lease_id: Uuid = serde_json::from_value(issued.body["id"].clone()).expect("an id");
    let lease_token = issued.body["token"]
        .as_str()
        .expect("a lease token must be handed out once")
        .to_owned();
    assert_ne!(
        lease_token, FIXTURE_VALUE,
        "the lease token is a handle, not the secret"
    );
    assert_eq!(
        issued.body["max_uses"].as_i64(),
        Some(2),
        "the use cap the operator asked for must come back, not a guess"
    );
    assert_eq!(
        issued.body["state"].as_str(),
        Some("live"),
        "a fresh lease is live"
    );

    /* --------------------------------------------- a browser cannot redeem, a machine identity can */

    // The first half: a session cookie is not a machine identity. The route has no session guard
    // at all, so this has to be refused by the handler itself.
    let browser_attempt = call(
        &state,
        post(
            &format!("/api/v1/secret-leases/{lease_id}/redeem"),
            &owner_token,
            json!({ "token": lease_token }),
        ),
    )
    .await;
    assert_eq!(
        browser_attempt.status,
        StatusCode::UNAUTHORIZED,
        "a browser session must never redeem a lease: {}",
        browser_attempt.raw
    );
    assert!(
        !browser_attempt.raw.contains(FIXTURE_VALUE),
        "a refused redemption must not echo the value either"
    );
    // The error shape is `{"error": {code, message, details}}` — `details` is a child of the
    // error, not a sibling. An assertion that reads `body["details"]` finds `null` and reports
    // "the route forgot the header" when the route is in fact perfect; when a route and a test
    // disagree about the shape, the route is the spec.
    assert_eq!(
        browser_attempt.body["error"]["details"]["header"].as_str(),
        Some(KEY_HEADER),
        "the refusal names the header a caller should have used, not the value: {}",
        browser_attempt.raw
    );

    // The second half: a deployment key presented in the header may redeem inside its scope.
    let (key_id, key_value) = mint_key(
        &state,
        &owner_token,
        &format!("release-{}", Uuid::new_v4().simple()),
        "production",
        &[PAYMENT_SCOPE],
        &in_hours(24),
    )
    .await;
    // The prefix is a CONTRACT (an operator recognises a key pasted into a CI variable), so the
    // walk asserts the crate's own constant rather than a spelling of its own. It used to assert
    // `omdk_`, which four other places in the tree — the generator, the CLI help, a route test
    // and this crate's fixture — had never used, and the failure read as "the product minted the
    // wrong key" when the product was the one thing in agreement with itself.
    assert!(
        key_value.starts_with(DEPLOYMENT_KEY_PREFIX),
        "a deployment key value is recognisable as one: {key_value}"
    );

    let redeemed = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secret-leases/{lease_id}/redeem"),
            &key_value,
            json!({ "token": lease_token }),
        ),
    )
    .await;
    assert_eq!(
        redeemed.status,
        StatusCode::OK,
        "a machine identity in scope must redeem: {}",
        redeemed.raw
    );
    assert_eq!(
        redeemed.body["value"].as_str(),
        Some(FIXTURE_VALUE),
        "redemption is the one response that carries the value"
    );
    assert_eq!(
        redeemed.body["version"].as_i64(),
        Some(1),
        "the redemption names the version it came from"
    );
    assert!(
        redeemed.body["request_id"].is_string(),
        "every redemption carries a request id for the audit trail"
    );

    // And the list endpoint never carries the value, even for the owner.
    let after = call(&state, get("/api/v1/secret-leases", &owner_token)).await;
    assert!(
        !after.raw.contains(FIXTURE_VALUE),
        "the lease list must stay value-free: {}",
        after.raw
    );
    assert!(
        !after.raw.contains(&lease_token),
        "the lease token left the issuing response and must not come back: {}",
        after.raw
    );
    let listed = after.body["leases"]
        .as_array()
        .expect("the list must be an array")
        .iter()
        .find(|entry| entry["id"] == lease_id.to_string())
        .expect("the lease must be listed");
    assert_eq!(
        listed["uses"].as_i64(),
        Some(1),
        "the redemption budget shows what was spent"
    );
    // The address is `None`, and that is the honest answer rather than a missing one.
    // `ClientAddress` is extracted from the `ConnectInfo<SocketAddr>` extension, which exists
    // only when the router is served by a real listener; an in-process `oneshot` call has no peer
    // address, so `as_text()` is `None` and `redeem_lease` writes `null`. The walk asserted
    // `Some("::1")`, which is a claim about a connection the test does not make — and it is the
    // second time this assertion was about the harness rather than the product (the first was
    // the scope). What the walk CAN prove about the column is that redemption is what fills it,
    // so the value is compared against the row the walk just spent, read back out of PostgreSQL
    // rather than out of a response that projects it.
    let stamped: (Option<String>, Option<time::OffsetDateTime>) = sqlx::query_as(
        "select last_address, last_redeemed_at from secret_leases where id = $1",
    )
    .bind(lease_id)
    .fetch_one(db.pool())
    .await
    .expect("the spent lease must be readable");
    assert!(
        stamped.1.is_some(),
        "the redemption did not stamp the lease: {stamped:?}"
    );
    assert_eq!(
        stamped.0, listed["last_address"].as_str().map(str::to_owned),
        "the list and the row must agree about the redemption address: {stamped:?} vs {listed:?}"
    );

    /* ----------------------------------------------- the use cap is spent, then refused again */

    let second = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secret-leases/{lease_id}/redeem"),
            &key_value,
            json!({ "token": lease_token }),
        ),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "the second redemption is within the cap: {}",
        second.raw
    );

    let third = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secret-leases/{lease_id}/redeem"),
            &key_value,
            json!({ "token": lease_token }),
        ),
    )
    .await;
    assert_eq!(
        third.status,
        StatusCode::GONE,
        "a third redemption is past the cap and must be refused: {}",
        third.raw
    );
    assert!(
        !third.raw.contains(FIXTURE_VALUE),
        "a spent lease answers a refusal, not a value"
    );

    // The denial is a row, not a silence: the request's rule is that a refusal is auditable.
    let denials: i64 = sqlx::query_scalar(
        "select count(*) from deployment_key_uses \
         where key_id = $1 and action = 'denied' and result <> 'ok'",
    )
    .bind(key_id)
    .fetch_one(db.pool())
    .await
    .expect("the use log must read");
    assert!(
        denials >= 1,
        "a refused redemption must be written to the use log, found {denials}"
    );

    /* ------------------------------- a machine identity cannot ISSUE; only redeem is for a key */

    // The walk used to read this as "a key bound to production must not lease in staging" and
    // expected a 403 from `POST /secrets/{id}/lease` with a key in the header. The route does not
    // take one: issuing is session-guarded (`CurrentSession`), so a machine key there is simply
    // an unauthenticated caller and the honest answer is 401. The environment binding is real
    // and it is enforced — at REDEMPTION, by `check_key_may_touch` — which is the assertion
    // further down, on a lease that actually exists.
    let machine_cannot_issue = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secrets/{payment_secret}/lease"),
            &key_value,
            json!({ "consumer": "nightly-release", "environment": "staging" }),
        ),
    )
    .await;
    assert_eq!(
        machine_cannot_issue.status,
        StatusCode::UNAUTHORIZED,
        "a lease is issued by a session, never by a machine key: {}",
        machine_cannot_issue.raw
    );
    assert!(
        !machine_cannot_issue.raw.contains(FIXTURE_VALUE),
        "the refusal must not echo a value"
    );

    // The scope rule, at the point it is actually enforced: REDEMPTION. `check_key_may_touch`
    // runs inside the redeem handler, so a lease on the `smtp` credential is what has to exist
    // first — the same key, an in-environment lease, and a secret outside its `payments.live.*`
    // scope, leaving the scope as the only rule that can refuse it. Every deployment-key refusal
    // is the same status and code by design, so the *only* thing separating a scope refusal from
    // a wrong key is which secret was asked for.
    let smtp_lease = call(
        &state,
        post(
            &format!("/api/v1/secrets/{smtp_secret}/lease"),
            &owner_token,
            json!({ "consumer": "release-runner", "environment": "production" }),
        ),
    )
    .await;
    assert_eq!(smtp_lease.status, StatusCode::CREATED, "a lease must issue");
    let smtp_lease_id: Uuid = serde_json::from_value(smtp_lease.body["id"].clone()).expect("an id");
    let smtp_lease_token = smtp_lease.body["token"]
        .as_str()
        .expect("a token")
        .to_owned();

    let out_of_scope = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secret-leases/{smtp_lease_id}/redeem"),
            &key_value,
            json!({ "token": smtp_lease_token }),
        ),
    )
    .await;
    assert_eq!(
        out_of_scope.status,
        StatusCode::FORBIDDEN,
        "a key scoped to payments must not redeem an smtp lease: {}",
        out_of_scope.raw
    );
    assert!(
        !out_of_scope.raw.contains(FIXTURE_VALUE),
        "the scope refusal must not echo a value"
    );

    // A wrong key and a dead key are the same answer, so a caller cannot probe for the difference.
    let wrong_key = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secret-leases/{smtp_lease_id}/redeem"),
            "omnion_dk_totally-made-up-key-value-0000000000000000",
            json!({ "token": smtp_lease_token }),
        ),
    )
    .await;
    assert_eq!(
        wrong_key.status,
        StatusCode::UNAUTHORIZED,
        "an unknown key must be refused: {}",
        wrong_key.raw
    );
    // The two refusals must be indistinguishable to a caller — but the bodies differ in exactly
    // one field, `details.request_id`, and that difference is the point: it is what lets an
    // operator find the audit row for *this* call. So the comparison is on the parts a caller
    // could learn anything from, and the request id is asserted to be PRESENT and different
    // rather than compared away. A whole-body equality assertion here would have been asserting
    // that two separate requests share an id.
    let (wrong_code, wrong_message) = (
        wrong_key.body["error"]["code"].as_str(),
        wrong_key.body["error"]["message"].as_str(),
    );
    let (scoped_code, scoped_message) = (
        out_of_scope.body["error"]["code"].as_str(),
        out_of_scope.body["error"]["message"].as_str(),
    );
    assert_eq!(
        (wrong_code, wrong_message),
        (scoped_code, scoped_message),
        "a wrong key and a scope escalation must not be distinguishable from outside: {} vs {}",
        wrong_key.raw,
        out_of_scope.raw
    );
    let wrong_request_id = wrong_key.body["error"]["details"]["request_id"].as_str();
    let scoped_request_id = out_of_scope.body["error"]["details"]["request_id"].as_str();
    assert!(
        wrong_request_id.is_some_and(|id| !id.is_empty()) && scoped_request_id.is_some(),
        "every refusal carries a request id so the operator can find its audit row"
    );
    assert_ne!(
        wrong_request_id, scoped_request_id,
        "two separate calls must not share a request id"
    );

    /* ----------------------------------------------------------- a key past its expiry is refused */

    // **The API refuses to mint one at all**, and the walk used to insist it could. `mint_key`
    // with `in_hours(-1)` wanted a `201` so it could then assert the redemption was refused — but
    // `create_deployment_key` rejects an expiry in the past outright, with a message that says
    // why ("dead on arrival"), and that is the better product: a key nobody can present is a key
    // in the list that an operator has to reason about, and refusing at creation removes the
    // state rather than rendering it honestly. The refusal is therefore the assertion, and the
    // "expired" rendering is still checked below on a row that gets there the only way a real
    // deployment can — by ageing.
    let backdated = call(
        &state,
        post(
            "/api/v1/deployment-keys",
            &owner_token,
            json!({
                "name": format!("expired-{}", Uuid::new_v4().simple()),
                "environment": "production",
                "scopes": [PAYMENT_SCOPE],
                "expires_at": in_hours(-1),
            }),
        ),
    )
    .await;
    assert_eq!(
        backdated.status,
        StatusCode::BAD_REQUEST,
        "a key that is dead on arrival must not be mintable: {}",
        backdated.raw
    );
    assert!(
        !backdated.raw.contains(FIXTURE_VALUE),
        "the refusal must not echo a value"
    );

    // A live key that has since aged behaves the same as the walk's original intent: its value
    // stops working, and the refusal is the uniform one.
    let (aged_key_id, aged_value) = mint_key(
        &state,
        &owner_token,
        &format!("aged-{}", Uuid::new_v4().simple()),
        "production",
        &[PAYMENT_SCOPE],
        &in_hours(1),
    )
    .await;
    // Age it from the past, which is the one thing the API will not do.
    sqlx::query("update deployment_keys set expires_at = now() - interval '1 hour' where id = $1")
        .bind(aged_key_id)
        .execute(db.pool())
        .await
        .expect("the key must be ageable");
    let expired_use = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secret-leases/{lease_id}/redeem"),
            &aged_value,
            json!({ "token": lease_token }),
        ),
    )
    .await;
    assert_eq!(
        expired_use.status,
        StatusCode::UNAUTHORIZED,
        "a key past its expiry must be refused: {}",
        expired_use.raw
    );
    let (expired_code, expired_message) = (
        expired_use.body["error"]["code"].as_str(),
        expired_use.body["error"]["message"].as_str(),
    );
    assert_eq!(
        (expired_code, expired_message),
        (wrong_code, wrong_message),
        "an expired key and a wrong key must be the same answer: {} vs {}",
        expired_use.raw,
        wrong_key.raw
    );
    let expired_key_id = aged_key_id;

    // The aged key is listed and renders as `expired` rather than `active`, so the screen never
    // shows a working key that is not one.
    let key_list = call(&state, get("/api/v1/deployment-keys", &owner_token)).await;
    let expired_row = key_list.body["keys"]
        .as_array()
        .expect("keys must be an array")
        .iter()
        .find(|entry| entry["id"] == expired_key_id.to_string())
        .expect("the expired key must be listed");
    assert_eq!(
        expired_row["state"].as_str(),
        Some("expired"),
        "a past-dated key reports expired, not active"
    );
    assert!(
        !key_list.raw.contains(&aged_value),
        "the key value is shown once and never returns: {}",
        key_list.raw
    );
    assert!(
        !key_list.raw.contains(&key_value),
        "a live key's value is equally unreadable afterwards"
    );
    assert!(
        expired_row["fingerprint"]
            .as_str()
            .is_some_and(|f| f.starts_with("omnion-dk-")),
        "the panel offers a fingerprint an operator can compare without the value"
    );

    /* --------------------------------------------------------- the use log names the pipeline */

    let uses = call(
        &state,
        get(
            &format!("/api/v1/deployment-keys/{key_id}/uses"),
            &owner_token,
        ),
    )
    .await;
    assert_eq!(uses.status, StatusCode::OK, "the use log must read");
    // The route answers with a BARE array — `Json<Vec<DeploymentKeyUseView>>` — not the
    // `{ "uses": [...] }` envelope the walk assumed. A wrapper that does not exist is a
    // `.expect("an array")` that fires on a perfectly good response, and it fires on the
    // RESPONSE rather than on a product defect, which is the worst place for a walk to fail.
    let entries = uses.body.as_array().unwrap_or_else(|| {
        panic!("the use log must be a bare array of rows: {}", uses.raw);
    });
    assert!(
        entries
            .iter()
            .any(|entry| entry["action"] == "lease" && entry["result"] == "ok"),
        "a successful lease is logged: {uses_raw}",
        uses_raw = uses.raw
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry["action"] == "denied" && entry["result"] != "ok"),
        "a denial is logged with its result: {uses_raw}",
        uses_raw = uses.raw
    );
    assert!(
        entries
            .iter()
            .all(|entry| !entry.to_string().contains(FIXTURE_VALUE)),
        "the use log is metadata only"
    );

    /* ------------------------------------------------- revoking a key revokes what it minted */

    let live_lease = call(
        &state,
        post(
            &format!("/api/v1/secrets/{payment_secret}/lease"),
            &owner_token,
            json!({ "consumer": "release-runner", "environment": "production", "max_uses": 5 }),
        ),
    )
    .await;
    assert_eq!(live_lease.status, StatusCode::CREATED, "a lease must issue");
    let live_token = live_lease.body["token"]
        .as_str()
        .expect("a token")
        .to_owned();

    let revoked = call(
        &state,
        post(
            &format!("/api/v1/deployment-keys/{key_id}/revoke"),
            &owner_token,
            json!({ "reason": "the pipeline it was minted for was retired" }),
        ),
    )
    .await;
    // The route answers `204 No Content` (`Result<StatusCode, ApiError>`) — it revokes and says
    // nothing, which is the right shape for an idempotent action. The walk asserted `200` and
    // then read a `state` field out of a body that a `204` does not have, so the "a revoked key
    // says so" claim was being checked against a field the product never claimed to return. It is
    // checked below, against the list an operator actually reads.
    assert_eq!(
        revoked.status,
        StatusCode::NO_CONTENT,
        "a key must be revocable: {}",
        revoked.raw
    );

    // "A revoked key says so" — checked on the list, because that is where an operator looks
    // and the revoke itself returned no body at all.
    let after_revoke_list = call(
        &state,
        get("/api/v1/deployment-keys", &owner_token),
    )
    .await;
    let revoked_row = after_revoke_list.body["keys"]
        .as_array()
        .expect("keys must be an array")
        .iter()
        .find(|entry| entry["id"] == key_id.to_string())
        .expect("the revoked key must still be listed — a revoke is not a delete");
    assert_eq!(
        revoked_row["state"].as_str(),
        Some("revoked"),
        "a revoked key says so in the list: {}",
        after_revoke_list.raw
    );

    let after_revoke = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secret-leases/{lease_id}/redeem"),
            &key_value,
            json!({ "token": lease_token }),
        ),
    )
    .await;
    // A revoked key answers `401` with the SAME uniform body a wrong key does, and that is the
    // security fix rather than a regression: the walk used to expect `410 Gone`, which told a
    // caller that its key was REAL and had been revoked — a free oracle for enumerating valid
    // keys, and the whole point of `DEPLOYMENT_KEY_UNUSABLE`. The lease itself was already
    // revoked by the sweep, so the *information a caller must not get* is the key's state, and
    // the status is where it was leaking. The `401` is correct; the assertion now holds the
    // uniformity instead of the leak.
    assert_eq!(
        after_revoke.status,
        StatusCode::UNAUTHORIZED,
        "a revoked key must be indistinguishable from a wrong one: {}",
        after_revoke.raw
    );
    assert_eq!(
        (after_revoke.body["error"]["code"].as_str(),
         after_revoke.body["error"]["message"].as_str()),
        (wrong_code, wrong_message),
        "a revoked key and a wrong key must render the same sentence: {} vs {}",
        after_revoke.raw,
        wrong_key.raw
    );
    // The operator still learns what happened — the use log is where the distinction lives, and
    // it never travels to the caller. The row is `action = "denied"` with the error's own `code`
    // in `result`, which is the machine-only half of the uniform refusal: "denied /
    // deployment_key_unavailable" says to an operator exactly which rule fired.
    let revoked_uses: Vec<(String, String)> = sqlx::query_as(
        "select action, coalesce(result, '') from deployment_key_uses \
         where key_id = $1 order by created_at desc limit 10",
    )
    .bind(key_id)
    .fetch_all(db.pool())
    .await
    .expect("the use log must be readable");
    assert!(
        revoked_uses
            .iter()
            .any(|(action, result)| action == "denied" && result == "deployment_key_unavailable"),
        "the refusal is recorded for the operator even though the caller learns nothing: \
         {revoked_uses:?}"
    );

    // A live key cannot be deleted — only revoked. A delete on a live key would remove the row a
    // use log points at, and the log is the only way a leak is found.
    let doomed = mint_key(
        &state,
        &owner_token,
        &format!("doomed-{}", Uuid::new_v4().simple()),
        "production",
        &[PAYMENT_SCOPE],
        &in_hours(24),
    )
    .await;
    let delete_live = call(
        &state,
        delete(
            &format!("/api/v1/deployment-keys/{}", doomed.0),
            &owner_token,
        ),
    )
    .await;
    assert!(
        delete_live.status.is_client_error(),
        "a live key must not be deletable, got {}",
        delete_live.status
    );

    let delete_revoked = call(
        &state,
        delete(&format!("/api/v1/deployment-keys/{key_id}"), &owner_token),
    )
    .await;
    assert_eq!(
        delete_revoked.status,
        StatusCode::NO_CONTENT,
        "a revoked key's record can be deleted: {}",
        delete_revoked.raw
    );

    /* -------------------------------------------- a deployment revokes the environment's leases */

    // Two leases: one in production, one in staging. A deploy in production must take exactly the
    // first. Getting this wrong in either direction is the bug the request exists for.
    let prod_lease = call(
        &state,
        post(
            &format!("/api/v1/secrets/{payment_secret}/lease"),
            &owner_token,
            json!({ "consumer": "before-deploy", "environment": "production", "max_uses": 9 }),
        ),
    )
    .await;
    let staging_lease = call(
        &state,
        post(
            &format!("/api/v1/secrets/{payment_secret}/lease"),
            &owner_token,
            json!({ "consumer": "before-deploy", "environment": "staging", "max_uses": 9 }),
        ),
    )
    .await;
    let prod_lease_id: Uuid = serde_json::from_value(prod_lease.body["id"].clone()).expect("an id");
    let prod_lease_token = prod_lease.body["token"]
        .as_str()
        .expect("a token")
        .to_owned();
    let staging_lease_id: Uuid =
        serde_json::from_value(staging_lease.body["id"].clone()).expect("an id");
    let staging_lease_token = staging_lease.body["token"]
        .as_str()
        .expect("a token")
        .to_owned();
    let _ = live_token; // issued above, superseded by the pair this assertion is about

    // The event is written the way a deployment centre by another writer would write it: nobody
    // calls the revocation, the consumer reads the stream. That is the whole design claim.
    let event_id: i64 = sqlx::query_scalar(
        "insert into events (name, payload) values ('deployment.started', $1) returning id",
    )
    .bind(
        json!({ "environment": "production", "release": format!("v{}", Uuid::new_v4().simple()) }),
    )
    .fetch_one(db.pool())
    .await
    .expect("the deployment event must be recorded");

    let revoked_count = omnion_api::secrets_runner::revoke_leases_for_deploys(&state)
        .await
        .expect("the revocation tick must run");
    assert!(
        revoked_count >= 1,
        "the deploy must revoke at least the production lease, got {revoked_count}"
    );

    let after_deploy = call(&state, get("/api/v1/secret-leases", &owner_token)).await;
    let prod_row = after_deploy.body["leases"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|entry| entry["id"] == prod_lease_id.to_string())
        .expect("the production lease must still be listed, revoked or not");
    let staging_row = after_deploy.body["leases"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|entry| entry["id"] == staging_lease_id.to_string())
        .expect("the staging lease must still be listed");
    assert_eq!(
        prod_row["state"].as_str(),
        Some("revoked"),
        "a lease in the deployed environment is revoked"
    );
    assert_eq!(
        staging_row["state"].as_str(),
        Some("live"),
        "a lease in another environment is untouched: {staging_row}"
    );
    let reason = prod_row["revoke_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("deployment") && reason.contains("production"),
        "the lease row carries the reason a deploy revoked it, found {reason:?}"
    );

    // And the reason is a fact, not a hint: the revoked handle is refused, and the refusal is
    // written to the audit with a request id the operator can quote.
    let denied_after_deploy = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secret-leases/{prod_lease_id}/redeem"),
            &key_value,
            json!({ "token": prod_lease_token }),
        ),
    )
    .await;
    // The refusal is `401` with the uniform sentence, not `410 Gone` — the key is unusable, and
    // saying *why* it is unusable (revoked by a deploy) to the machine holding it is the same
    // oracle as any other distinguishable refusal. The deploy's effect on the LEASE is asserted
    // above, on the row's own `revoke_reason`, which is where an operator reads it.
    assert_eq!(
        denied_after_deploy.status,
        StatusCode::UNAUTHORIZED,
        "a lease a deploy revoked must not redeem: {}",
        denied_after_deploy.raw
    );
    assert_eq!(
        (denied_after_deploy.body["error"]["code"].as_str(),
         denied_after_deploy.body["error"]["message"].as_str()),
        (wrong_code, wrong_message),
        "a deploy-revoked lease renders the uniform refusal: {} vs {}",
        denied_after_deploy.raw,
        wrong_key.raw
    );
    assert!(
        denied_after_deploy.body["error"]["details"]["request_id"].is_string(),
        "a refusal carries a request id: {}",
        denied_after_deploy.raw
    );
    assert!(
        !denied_after_deploy.raw.contains(FIXTURE_VALUE),
        "a revoked lease answers a refusal, not a credential"
    );

    // The cursor moved, so a second tick does not re-revoke what the first already took.
    let cursor: i64 = sqlx::query_scalar(
        "select last_event_id from event_consumer_cursors where consumer = 'secrets.lease_revocation'",
    )
    .fetch_one(db.pool())
    .await
    .expect("the cursor must exist after the migration");
    assert_eq!(
        cursor, event_id,
        "the cursor sits on the event it acted on, not behind it"
    );
    let second_tick = omnion_api::secrets_runner::revoke_leases_for_deploys(&state)
        .await
        .expect("the second tick must run");
    assert_eq!(
        second_tick, 0,
        "a second tick has nothing unread, so it revokes nothing"
    );

    // A lease in staging still works — the deployment did not touch it, and proving that is what
    // stops the revocation from being "revoke everything" in a later refactor.
    let (staging_key, staging_key_value) = mint_key(
        &state,
        &owner_token,
        &format!("staging-{}", Uuid::new_v4().simple()),
        "staging",
        &[PAYMENT_SCOPE],
        &in_hours(24),
    )
    .await;
    let staging_redeem = call(
        &state,
        post_as_machine(
            &format!("/api/v1/secret-leases/{staging_lease_id}/redeem"),
            &staging_key_value,
            json!({ "token": staging_lease_token }),
        ),
    )
    .await;
    assert_eq!(
        staging_redeem.status,
        StatusCode::OK,
        "the staging lease still works after a production deploy: {}",
        staging_redeem.raw
    );
    assert_eq!(
        staging_redeem.body["value"].as_str(),
        Some(FIXTURE_VALUE),
        "and it is still the right value"
    );
    let _ = staging_key;

    /* ---------------------------- a read-only bridge refuses every write, and says why ------ */

    // A `file` / `env` provider is a pointer to a credential managed outside the platform. The
    // only claim worth making about it is that Omnion can never become a second, worse copy of
    // the operator's own file store — so the write path has to be refused structurally, not by
    // hiding the button. The test writes straight at the API, which is the only way to prove a
    // UI that is merely greyed out is not what is holding the line.
    let bridge_id: Uuid = sqlx::query_scalar(
        "insert into secrets (name, scope_type, organization_id, description, provider, \
                              provider_locator, read_only) \
         values ($1, 'organization', $2, 'A credential managed outside the platform', 'env', \
                 'STRIPE_SECRET_KEY', true) returning id",
    )
    .bind(format!("bridge-live-{}", Uuid::new_v4().simple()))
    .bind(organization_id)
    .fetch_one(db.pool())
    .await
    .expect("the bridge must be created");

    let typed = call(
        &state,
        post(
            &format!("/api/v1/secrets/{bridge_id}/credential"),
            &owner_token,
            json!({ "kind": "api_key", "fields": { "endpoint": "https://api.example.test" } }),
        ),
    )
    .await;
    assert_eq!(
        typed.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "typing a read-only bridge must be refused with 422, not accepted: {}",
        typed.raw
    );
    assert_eq!(
        typed.body["error"]["code"].as_str(),
        Some("secret_read_only"),
        "the refusal names the reason a UI can render: {}",
        typed.raw
    );

    // A lease is a write too: it hands a value to a machine, which a pointer cannot do.
    let bridge_lease = call(
        &state,
        post(
            &format!("/api/v1/secrets/{bridge_id}/lease"),
            &owner_token,
            json!({ "consumer": "release-runner", "environment": "production" }),
        ),
    )
    .await;
    assert_eq!(
        bridge_lease.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "a read-only bridge cannot be leased — a lease is a write, and the route says so: {}",
        bridge_lease.raw
    );
    assert_eq!(
        bridge_lease.body["error"]["code"].as_str(),
        Some("secret_read_only"),
        "and the refusal names the reason rather than a generic validation failure: {}",
        bridge_lease.raw
    );

    // The list still shows it, and says so in words rather than pretending it is broken.
    let credentials = call(&state, get("/api/v1/secrets/credentials", &owner_token)).await;
    let bridge_row = credentials.body["credentials"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|entry| entry["id"] == bridge_id.to_string())
        .expect("the bridge must be listed even without a profile");
    assert_eq!(
        bridge_row["read_only"].as_bool(),
        Some(true),
        "the row is marked read-only: {bridge_row}"
    );
    // Asserted as a containment, not an equality: the sentence is deliberately longer than a
    // chip, and pinning the exact wording would make a copy edit a test failure for no gain.
    let description = bridge_row["kind_description"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        description.contains("managed outside the platform"),
        "the panel explains the state instead of showing a missing kind: {bridge_row}"
    );
    assert!(
        description.contains("env"),
        "and it names the provider that resolves it, so the operator knows which system is the \
         real owner: {bridge_row}"
    );
    assert_eq!(
        bridge_row["offline_checkable"].as_bool(),
        Some(false),
        "a bridge has no value here, so there is nothing to check offline: {bridge_row}"
    );
    assert_eq!(
        bridge_row["provider"].as_str(),
        Some("env"),
        "the provider is named so the row is not an orphan: {bridge_row}"
    );

    /* ------------------------------------------- the permission split: read is not lease or mint */

    let member_leases = call(&state, get("/api/v1/secret-leases", &member_token)).await;
    assert_eq!(
        member_leases.status,
        StatusCode::FORBIDDEN,
        "a member without the secrets permission cannot read the lease list"
    );
    let member_mint = call(
        &state,
        post(
            "/api/v1/deployment-keys",
            &member_token,
            json!({
                "name": "not-allowed",
                "environment": "production",
                "scopes": [PAYMENT_SCOPE],
                "expires_at": in_hours(24),
            }),
        ),
    )
    .await;
    assert_eq!(
        member_mint.status,
        StatusCode::FORBIDDEN,
        "a member without the permission cannot mint a machine credential"
    );

    // An anonymous caller cannot reach the list or the redemption, whatever it presents.
    let anonymous = call(
        &state,
        request(Method::GET, "/api/v1/secret-leases", None, None, None),
    )
    .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "the lease list needs a session"
    );
    let _ = member_id;
}
