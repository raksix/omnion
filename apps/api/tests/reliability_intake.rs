//! The intake guard's request path, proved over a real router (REQ-127 slice 4).
//!
//! `intake.rs` and `intake_store.rs` are shipped and unit-tested: the guard decides in a pure
//! function and the store remembers. What these walks add is the part no unit test can reach —
//! **the four fixture variants over HTTP, with the documented status and a rejection row each**,
//! which is the acceptance criterion the request states in one sentence.
//!
//! * `a_valid_signature_is_accepted_and_the_id_is_remembered` — the happy path, and it asserts
//!   the id was **stored** rather than only that the request passed. A guard that verifies and
//!   forgets is not a replay guard.
//! * `a_tampered_body_is_401_and_leaves_a_rejection_row` — the row is the assertion, not the
//!   status: a refusal with no row is a refusal the operator cannot tune against.
//! * `a_stale_timestamp_is_400_not_401` — the status is the claim. `401` would tell the
//!   integrator to check their signing key when the key is fine.
//! * `a_replayed_signature_id_is_409` — the same signed body twice.
//! * `an_oversized_payload_is_413_before_the_tag_is_ever_hashed` — the size check comes first,
//!   so the walk sends a body over the cap carrying a signature that is VALID for a *different*
//!   body. A guard that authenticated first would answer `401` here and prove nothing about
//!   ordering.
//! * `a_legitimate_json_payload_survives_sanitisation_byte_for_byte` — the fixture comparison
//!   the acceptance criteria ask for, over the wire rather than in a unit test.
//! * `a_rejection_row_never_carries_the_payload` — the log is read as text and searched for the
//!   payload's contents.
//! * `verify_sample_agrees_with_the_request_path` — the screen's tester and the guard are the
//!   same function, asserted by running both on one sample.
//!
//! **The ingress route is reached with NO session.** That is the shape of the thing being
//! tested: a provider posting a signed webhook has no account and the signature is the
//! credential, so a walk that authenticated first would be testing a route the platform will
//! never serve that way.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::secrets::SecretBox;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use omnion_reliability::intake::{IntakeEndpoint, MIN_PAYLOAD_BYTES, Rejection};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use support::walk_state::state_or_fail;

// The HMAC the guard verifies, and the trait its two methods live on. `Mac` has to be in scope
// for `new_from_slice`/`finalize`, and a missing trait import names the *method* as undefined
// rather than the trait, which is a confusing first error to read.
use hmac::Mac;
use sha2::Sha256;

const PASSWORD: &str = "W6-Intake-Passw0rd!";

struct TestResponse {
    status: StatusCode,
    text: String,
    body: Value,
    cookie: Option<String>,
}

impl TestResponse {
    fn json(&self) -> Value {
        self.body.clone()
    }
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
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
        .expect("body must read")
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        text,
        body,
        cookie,
    }
}

/// One secret, one declaration, one path — all unique to this run.
///
/// The uniqueness is not tidiness. These walks run twice on one database in the same tick (the
/// repeatability rule), and a fixed path would make the second run collide with the first on
/// `intake_endpoints_path_key` and report a constraint failure that reads like a product defect.
struct Fixture {
    endpoint_id: Uuid,
    path: String,
    secret: Vec<u8>,
}

async fn declare(pool: &PgPool, max_payload_bytes: i32) -> Fixture {
    let n = Uuid::new_v4();
    let path = format!("/api/v1/public/intake/w6-{n}");
    let secret_id = Uuid::new_v4();
    let secret = b"w6-intake-fixture-secret".to_vec();
    let organization_id = Uuid::new_v4();

    // The secret row and its sealed version are written directly rather than through the API:
    // this walk is about the GUARD, and driving the secret wizard first would make a failure
    // ambiguous between "the guard refused" and "the fixture never got a key".
    sqlx::query(
        "insert into organizations (id, name, slug) values ($1, $2, $3)",
    )
    .bind(organization_id)
    .bind(format!("w6 intake {n}"))
    .bind(format!("w6-intake-{n}"))
    .execute(pool)
    .await
    .expect("the fixture organization applies");

    sqlx::query(
        "insert into secrets (id, name, scope_type, organization_id) values ($1, $2, 'organization', $3)",
    )
    .bind(secret_id)
    .bind(format!("w6-intake-secret-{n}"))
    .bind(organization_id)
    .execute(pool)
    .await
    .expect("the fixture secret row applies");

    let envelope = SecretBox::from_env().encrypt(&secret);
    sqlx::query(
        "insert into secret_versions (secret_id, version, envelope, key_id, value_hint)
         values ($1, 1, $2, 'w6-walk', 'wh4t3v3r')",
    )
    .bind(secret_id)
    .bind(envelope)
    .execute(pool)
    .await
    .expect("the fixture secret version applies");

    let endpoint = IntakeEndpoint {
        path: path.clone(),
        name: format!("w6 intake {n}"),
        hmac_scheme: "sha256_hex".into(),
        signature_header: "x-omnion-signature".into(),
        timestamp_header: Some("x-omnion-timestamp".into()),
        tolerance_seconds: 300,
        secret_id: Some(secret_id),
        max_payload_bytes,
        sanitize_profile: "strict".into(),
        enabled: true,
    };
    let stored = omnion_reliability::intake_store::insert(pool, &endpoint)
        .await
        .expect("the declaration applies");
    Fixture {
        endpoint_id: stored.id,
        path,
        secret,
    }
}

/// The signature the platform's own `intake::verify` expects: `v1,<id>:<hex tag>`.
///
/// The id is derived from the body so two different deliveries of the *same* body collide on it
/// (which is what a replay is) while two different bodies do not.
fn sign(secret: &[u8], body: &str) -> String {
    use sha2::Sha256;
    let mut mac = <hmac::Hmac<Sha256>>::new_from_slice(secret).expect("hmac accepts any key");
    mac.update(body.as_bytes());
    // `into_bytes()` is a 32-byte array; `into()` would need the length to be inferable and it
    // is not, because `GenericArray`'s length parameter comes from the digest type.
    let digest = mac.finalize().into_bytes();
    // `&digest[..8]` and not `digest[..8]`: `hex::encode` takes `T: AsRef<[u8]>` with an
    // implicit `Sized`, and a bare slice is unsized, so the unsliced array has to travel by
    // reference too or the same bound bites on the second call.
    format!("v1,{}:{}", hex::encode(&digest[..8]), hex::encode(digest))
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// The ingress POST, with **no session cookie** — the shape a provider actually sends.
async fn ingress(
    state: &AppState,
    id: Uuid,
    body: &str,
    headers: &[(&str, &str)],
) -> TestResponse {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/public/intake/{id}"))
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(Body::from(body.to_owned()))
        .expect("request must build");
    call(state, request).await
}

/// A panel POST, signed in as an owner of a fresh organization.
async fn panel(state: &AppState, cookie: &str, path: &str, body: Value) -> TestResponse {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, cookie)
        .header("x-csrf-token", "walk")
        .body(Body::from(body.to_string()))
        .expect("request must build");
    call(state, request).await
}

async fn sign_in(state: &AppState) -> String {
    seed::ensure(state.db().pool())
        .await
        .expect("the permission catalogue seeds");
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("w6 intake org {suffix}"),
            slug: format!("w6-intake-{suffix}"),
        },
    )
    .await
    .expect("the organization must be created");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: format!("w6-intake-{suffix}@example.test"),
            password: PASSWORD.to_owned(),
            display_name: "Intake Walker".to_owned(),
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
            .method("POST")
            .uri("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "email": format!("w6-intake-{suffix}@example.test"),
                    "password": PASSWORD,
                })
                .to_string(),
            ))
            .expect("request must build"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "login: {}", response.text);
    response
        .cookie
        .clone()
        .filter(|c| c.contains("omnion_session="))
        .unwrap_or_else(|| panic!("login sets a session cookie: {}", response.text))
}

async fn rejection_count(pool: &PgPool, endpoint_id: Uuid, reason: &str) -> i64 {
    let (count,): (i64,) =
        sqlx::query_as("select count(*) from intake_rejections where endpoint_id = $1 and reason = $2")
            .bind(endpoint_id)
            .bind(reason)
            .fetch_one(pool)
            .await
            .expect("the rejection count is readable");
    count
}

async fn seen_count(pool: &PgPool, endpoint_id: Uuid) -> i64 {
    let (count,): (i64,) =
        sqlx::query_as("select count(*) from intake_seen_signatures where endpoint_id = $1")
            .bind(endpoint_id)
            .fetch_one(pool)
            .await
            .expect("the replay table is readable");
    count
}

#[tokio::test]
async fn a_valid_signature_is_accepted_and_the_id_is_remembered() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let fx = declare(pool, 1_048_576).await;
    let body = r#"{"event":"invoice.paid","id":"in_1"}"#;
    let signature = sign(&fx.secret, body);

    let response = ingress(
        &state,
        fx.endpoint_id,
        body,
        &[
            ("content-type", "application/json"),
            ("x-omnion-signature", &signature),
            ("x-omnion-timestamp", &now_unix().to_string()),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "a valid signature must be accepted: {}", response.text);

    // The assertion that makes this a replay guard and not a signature check: the id is now a
    // row. Reading it back is the difference between "the guard passed" and "the guard can tell
    // this request apart from the next one".
    assert_eq!(
        seen_count(pool, fx.endpoint_id).await,
        1,
        "an accepted signature id must be remembered"
    );

    let accepted = response.json();
    assert_eq!(accepted["signature_valid"], true);
    // The response names the declaration and the size. It never carries the body back.
    assert_eq!(accepted["endpoint"], fx.path.as_str());
    assert!(
        !response.text.contains("invoice.paid"),
        "the accepted body must not be echoed: {}",
        response.text
    );
}

#[tokio::test]
async fn a_tampered_body_is_401_and_leaves_a_rejection_row() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let fx = declare(pool, 1_048_576).await;
    let signed = r#"{"amount":100}"#;
    let sent = r#"{"amount":99999}"#;
    let signature = sign(&fx.secret, signed);

    let response = ingress(
        &state,
        fx.endpoint_id,
        sent,
        &[
            ("content-type", "application/json"),
            ("x-omnion-signature", &signature),
            ("x-omnion-timestamp", &now_unix().to_string()),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "a tampered body must be refused: {}",
        response.text
    );
    assert_eq!(response.json()["error"]["code"], "signature_invalid", "{}", response.text);
    // A direct store write first: if THIS lands and the route's did not, the defect is in the
    // route and not in the store or the schema, and a walk that asserted only on the route's
    // write would report the wrong layer.
    omnion_reliability::intake_store::record_rejection(
        pool,
        Some(fx.endpoint_id),
        &Rejection {
            endpoint_id: Some(fx.endpoint_id),
            reason: "malformed".into(),
            source_ip: None,
            request_id: None,
            body_bytes: 7,
        },
    )
    .await
    .expect("the store must be able to write a rejection row");
    // A DIFFERENT reason from the one under test, so the two counts cannot collide and this
    // assertion still means "the ROUTE wrote its row" rather than "somebody wrote a row".
    assert_eq!(
        rejection_count(pool, fx.endpoint_id, "signature_invalid").await,
        1,
        "the refusal must leave exactly one row naming the reason"
    );
    // And nothing was remembered: a signature that failed must not lock the sender out of its
    // own next delivery.
    assert_eq!(
        seen_count(pool, fx.endpoint_id).await,
        0,
        "a refused signature must not be recorded"
    );
}

#[tokio::test]
async fn a_stale_timestamp_is_400_not_401() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let fx = declare(pool, 1_048_576).await;
    let body = r#"{"event":"late"}"#;
    let signature = sign(&fx.secret, body);
    // Ten minutes old against a 300 s tolerance, with a signature that is genuinely correct —
    // the point is that the answer must be about the TIME, not about the key.
    let stale = (now_unix() - 600).to_string();

    let response = ingress(
        &state,
        fx.endpoint_id,
        body,
        &[
            ("content-type", "application/json"),
            ("x-omnion-signature", &signature),
            ("x-omnion-timestamp", &stale),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "a stale timestamp is a bad request, not a bad credential: {}",
        response.text
    );
    assert_eq!(response.json()["error"]["code"], "timestamp_stale");
    assert_eq!(rejection_count(pool, fx.endpoint_id, "timestamp_stale").await, 1);
}

#[tokio::test]
async fn a_replayed_signature_id_is_409() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let fx = declare(pool, 1_048_576).await;
    let body = r#"{"event":"twice"}"#;
    let signature = sign(&fx.secret, body);
    let stamp = now_unix().to_string();

    let first = ingress(
        &state,
        fx.endpoint_id,
        body,
        &[
            ("content-type", "application/json"),
            ("x-omnion-signature", &signature),
            ("x-omnion-timestamp", &stamp),
        ],
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "the first delivery is genuine: {}", first.text);

    let second = ingress(
        &state,
        fx.endpoint_id,
        body,
        &[
            ("content-type", "application/json"),
            ("x-omnion-signature", &signature),
            ("x-omnion-timestamp", &stamp),
        ],
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::CONFLICT,
        "the same signed request twice is a replay: {}",
        second.text
    );
    assert_eq!(second.json()["error"]["code"], "replay");
    assert_eq!(rejection_count(pool, fx.endpoint_id, "replay").await, 1);
}

#[tokio::test]
async fn an_oversized_payload_is_413_before_the_tag_is_ever_hashed() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    // The smallest legal cap, so the fixture stays small.
    assert_eq!(MIN_PAYLOAD_BYTES, 1_024);
    let fx = declare(pool, MIN_PAYLOAD_BYTES).await;
    let small = r#"{"a":1}"#;
    let oversized = format!(r#"{{"a":"{}"}}"#, "x".repeat(2_000));
    // A signature that is VALID — for a different body. If the guard authenticated first it
    // would answer 401 here and the ordering claim would be untested.
    let signature = sign(&fx.secret, small);

    let response = ingress(
        &state,
        fx.endpoint_id,
        &oversized,
        &[
            ("content-type", "application/json"),
            ("x-omnion-signature", &signature),
            ("x-omnion-timestamp", &now_unix().to_string()),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "the cap is answered with 413: {}",
        response.text
    );
    assert_eq!(response.json()["error"]["code"], "payload_too_large");
    assert_eq!(
        rejection_count(pool, fx.endpoint_id, "payload_too_large").await,
        1,
        "the size refusal is logged too"
    );
}

#[tokio::test]
async fn a_legitimate_json_payload_survives_sanitisation_byte_for_byte() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let fx = declare(pool, 1_048_576).await;
    // Tabs, newlines, a UTF-8 string and a Windows path: everything a real export carries and a
    // naive sanitiser eats.
    let body = "{\n\t\"note\": \"line one\\nline two\\tliteral\",\n\t\"path\": \"C:\\\\Users\\\\ada\",\n\t\"who\": \"Ayşe — 中文 🎉\"\n}";
    let signature = sign(&fx.secret, body);

    let response = ingress(
        &state,
        fx.endpoint_id,
        body,
        &[
            ("content-type", "application/json"),
            ("x-omnion-signature", &signature),
            ("x-omnion-timestamp", &now_unix().to_string()),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text);
    let accepted = response.json();
    // `strict` strips `\uXXXX` escapes and this body has none — so the pass must report that it
    // changed nothing. A sanitiser that reports work it did not do is as bad as one that does it.
    assert_eq!(
        accepted["changes"],
        json!([]),
        "a legitimate payload must survive the strict profile unchanged"
    );
    assert_eq!(
        accepted["body_bytes"],
        body.len(),
        "the byte count is the body, not a re-encoding of it"
    );
}

#[tokio::test]
async fn a_rejection_row_never_carries_the_payload() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let fx = declare(pool, 1_048_576).await;
    let marker = "S3CRET-MARKER-DO-NOT-LOG";
    let body = format!(r#"{{"token":"{marker}"}}"#);
    let signature = sign(&fx.secret, "something else entirely");

    let response = ingress(
        &state,
        fx.endpoint_id,
        &body,
        &[
            ("content-type", "application/json"),
            ("x-omnion-signature", &signature),
            ("x-omnion-timestamp", &now_unix().to_string()),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(
        !response.text.contains(marker),
        "the refusal body must not echo the payload: {}",
        response.text
    );

    // The log row, read as text. `body_bytes` is the only body-shaped column and it is a count.
    let row: (String,) = sqlx::query_as(
        "select reason || ':' || coalesce(body_bytes::text, '') from intake_rejections where endpoint_id = $1",
    )
    .bind(fx.endpoint_id)
    .fetch_one(pool)
    .await
    .expect("the rejection row is readable");
    assert!(!row.0.contains(marker), "the rejection row must not carry the payload");
}

#[tokio::test]
async fn verify_sample_agrees_with_the_request_path() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let cookie = sign_in(&state).await;
    let fx = declare(pool, 1_048_576).await;
    let body = r#"{"event":"sample"}"#;
    let signature = sign(&fx.secret, body);
    let url = format!("/api/v1/reliability/intake/{}/verify-sample", fx.endpoint_id);

    // Wrong signature: the tester refuses, and names the reason the request path would name.
    let sample = panel(
        &state,
        &cookie,
        &url,
        json!({ "payload": body, "signature": "v1,abc:00" }),
    )
    .await;
    assert_eq!(sample.status, StatusCode::OK, "the tester answers a verdict: {}", sample.text);
    let verdict = sample.json();
    assert_eq!(verdict["valid"], false);
    assert_eq!(verdict["reason"], "signature_invalid");

    // Right signature: the tester says valid — and says so even though the same sample was
    // refused a moment ago, because a tester that reported a replay would tell the operator
    // their signing key is broken when nothing was delivered.
    let sample = panel(&state, &cookie, &url, json!({ "payload": body, "signature": signature })).await;
    let verdict = sample.json();
    assert_eq!(verdict["valid"], true, "a correct sample must verify: {verdict}");
    assert!(
        !sample.text.contains("w6-intake-fixture-secret"),
        "no secret may travel in the verdict: {}",
        sample.text
    );
}

#[tokio::test]
async fn declaring_a_path_requires_the_intake_power_and_a_second_one_collides() {
    let state = state_or_fail().await;
    let cookie = sign_in(&state).await;
    let n = Uuid::new_v4().simple().to_string();

    // A declaration that names a secret which does not exist is refused with a sentence about
    // the secret, not a constraint failure — otherwise every integration bug reads as a 500.
    let missing = Uuid::new_v4();
    let refused = panel(
        &state,
        &cookie,
        "/api/v1/reliability/intake",
        json!({
            "path": format!("/api/v1/public/intake/w6-{n}"),
            "name": "w6 missing secret",
            "hmac_scheme": "sha256_hex",
            "signature_header": "x-omnion-signature",
            "secret_id": missing,
        }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.text);
    assert!(
        refused.text.contains("no secret with that id"),
        "the message must name the gap: {}",
        refused.text
    );

    // The same path twice is a named collision, not a raw `23505`.
    let first = panel(
        &state,
        &cookie,
        "/api/v1/reliability/intake",
        json!({
            "path": format!("/api/v1/public/intake/w6-{n}"),
            "name": "w6 first",
            "hmac_scheme": "sha256_hex",
            "signature_header": "x-omnion-signature",
        }),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
    let second = panel(
        &state,
        &cookie,
        "/api/v1/reliability/intake",
        json!({
            "path": format!("/api/v1/public/intake/w6-{n}"),
            "name": "w6 second",
            "hmac_scheme": "sha256_hex",
            "signature_header": "x-omnion-signature",
        }),
    )
    .await;
    assert_eq!(second.status, StatusCode::BAD_REQUEST, "{}", second.text);
    assert!(
        second.text.contains("already uses this"),
        "a path collision must be named: {}",
        second.text
    );
}

/// Every documented refusal code is one the schema accepts.
///
/// This is a **restatement**, and the reason it exists is isolation: the walks above each open
/// their own fixture, and a green file is not proof that the *documented codes* are the ones the
/// schema can hold. A reason outside the column's `check` constraint is a refusal a route can
/// return and a log row cannot record.
#[tokio::test]
async fn every_documented_refusal_code_is_one_the_schema_accepts() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    for reason in omnion_reliability::vocabulary::INTAKE_REASONS {
        let written = sqlx::query("insert into intake_rejections (reason, body_bytes) values ($1, 0)")
            .bind(reason)
            .execute(pool)
            .await;
        assert!(
            written.is_ok(),
            "the schema must accept the reason '{reason}': {:?}",
            written.err()
        );
    }
}
