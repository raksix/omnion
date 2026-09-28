//! Integration test for the secrets access trail, the anomaly detectors and the SIEM export
//! (docs/requests/REQ-125, slice 4).
//!
//! It runs against the development stack and skips itself with a printed reason when PostgreSQL
//! is not reachable, exactly like the three REQ-125 suites beside it.
//!
//! The walk proves the four claims the request makes, in the order they matter:
//!
//! 1. **Every operation lands a row with an actor, an address and a request id** — and the trail
//!    is *readable*, not merely written. An audit trail nothing queries is an audit log nobody has
//!    ever looked at, so the assertions read the rows back through the real router rather than
//!    counting writes.
//! 2. **A denial is a row like any other.** The trail's most useful screen is the one showing who
//!    tried to reach a secret they could not, so a denial is asserted to be first-class rather
//!    than to abort into a log nobody reads.
//! 3. **A scripted off-hours reveal raises a flag, and the acknowledge persists.** The detectors
//!    are pure functions, so the *rule* is unit-tested in the crate; what only this walk can prove
//!    is that a real reveal writes the flag, that the acknowledge survives a re-read, and that
//!    acknowledging twice says "already acknowledged" instead of claiming a change.
//! 4. **The export carries metadata only.** The assertion is a grep for the fixture value across
//!    the raw feed bytes — not "a field is absent", because a redaction pass that merely dropped
//!    one field today could leak another tomorrow. It is asserted twice: the list response and the
//!    NDJSON feed, because the feed is the one that leaves the installation.
//!
//! Two properties are asserted as *refusals* rather than as absences, because a screen that
//! quietly returns a value is worse than one that 403s: the audit surface is its own permission,
//! and an unknown action filter narrows the result instead of widening it into another feature's
//! audit rows.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The value behind every credential this suite creates. Every leak assertion greps for exactly
/// this string, so a leak is caught rather than inferred.
const FIXTURE_VALUE: &str = "qa-audit-value-do-not-leak-91ac4e";

/// The operator key all four REQ-125 suites seal with. **Must** match the other three: the key
/// ring is installation-wide, and a different value here would make `ensure_active_key` hand back
/// a key this process cannot open.
const SUITE_OPERATOR_KEY: &str = "omnion-secrets-suite-operator-key";

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    body: Value,
    /// The unparsed bytes. The export is newline-delimited JSON, so the leak grep reads *these*
    /// rather than a re-serialized value object — a re-serialization would drop nothing and prove
    /// nothing about what actually went on the wire.
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

/// Build a request; `token` becomes the session cookie.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, format!("omnion_session={token}"));
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
    request(Method::GET, uri, Some(token), None)
}

/// A `POST` with a session cookie and a JSON payload.
fn post(uri: &str, token: &str, body: Value) -> Request<Body> {
    request(Method::POST, uri, Some(token), Some(body))
}

/// A `PATCH` with a session cookie and a JSON payload.
fn patch(uri: &str, token: &str, body: Value) -> Request<Body> {
    request(Method::PATCH, uri, Some(token), Some(body))
}

/// Object store of the test state.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// A state whose database has all migrations applied.
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
async fn create_account(db: &Db) -> (Uuid, String) {
    let email = format!("audit-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Audit Test".to_owned(),
            organization_id: None,
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
async fn sealed_secret(db: &Db, name: &str) -> Uuid {
    let secret_id: Uuid = sqlx::query_scalar(
        "insert into secrets (name, scope_type, description) \
         values ($1, 'organization', 'Sealed by the REQ-125 slice-4 suite') returning id",
    )
    .bind(name)
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

/// Write an audit row of this crate's own shape, bypassing the handlers so the detector run is
/// deterministic: a `RevealObservation`'s local hour is a parameter, and a walk that had to wait
/// until 03:00 local to exercise the off-hours rule would be a walk that rarely runs it.
async fn record_reveal(
    db: &Db,
    secret_id: Uuid,
    actor_user_id: Option<Uuid>,
    request_id: Uuid,
    local_hour: u8,
) {
    let observation = omnion_secrets::audit::RevealObservation {
        secret_id,
        actor_user_id,
        address: Some("198.51.100.7".to_owned()),
        request_id,
        at: time::OffsetDateTime::now_utc(),
        local_hour,
    };
    omnion_secrets::audit::record_reveal_anomalies(db.pool(), None, &observation)
        .await
        .expect("the reveal must be observed");
    // The row the detectors read, written AFTER the flags so the counts are the history and not
    // the observation itself — a detector that counted its own trigger would fire on every call.
    let mut entry = omnion_audit::NewAuditEntry::system("secret.revealed")
        .target("secret", secret_id.to_string())
        .request_id(request_id)
        .metadata(json!({ "key_id": "wrapping-key-id-not-a-value" }))
        .ip_address(Some("198.51.100.7".to_owned()));
    if let Some(actor) = actor_user_id {
        entry = omnion_audit::NewAuditEntry::by_user(actor, "secret.revealed")
            .target("secret", secret_id.to_string())
            .request_id(request_id)
            .metadata(json!({ "key_id": "wrapping-key-id-not-a-value" }))
            .ip_address(Some("198.51.100.7".to_owned()));
    }
    omnion_audit::entries::record(db.pool(), entry)
        .await
        .expect("the audit row must be written");
}

/// The four REQ-125 claims, end to end over the real router.
#[tokio::test]
async fn the_trail_joins_by_request_id_flags_off_hours_and_the_export_carries_no_value() {
    // SAFETY: the same guard the three suites beside it use. `set_var` is `unsafe` in edition 2024
    // and every one of these processes is its own single-threaded test binary, so no other thread
    // can observe the environment mid-test.
    unsafe {
        std::env::set_var(
            omnion_secrets::KEY_ENCRYPTION_ENV,
            SUITE_OPERATOR_KEY,
        );
    }

    let Some((state, db)) = live_state().await else {
        return;
    };
    let (_user_id, email) = create_account(&db).await;
    let token = login(&state, &email).await;
    let secret_id = sealed_secret(&db, "audit-slice-4-fixture").await;

    // ── claim 1: the operations land a row, and the trail is readable ────────────────────────────
    let trail = call(&state, get("/api/v1/secrets/audit?limit=200", &token)).await;
    assert_eq!(
        trail.status,
        StatusCode::OK,
        "the trail must be readable: {}",
        trail.raw
    );
    let filters = trail.body["filters"]
        .as_array()
        .expect("the screen's filter list must be an array");
    assert!(
        filters.iter().any(|value| value == "secret.revealed")
            && filters.iter().any(|value| value == "secret.access.denied"),
        "the trail must offer the actions the crate writes: {filters:?}"
    );
    assert!(
        !trail.raw.contains(FIXTURE_VALUE),
        "the trail must never carry a value: {}",
        trail.raw
    );

    // The detector block the panel renders, with the sentence that explains it. A threshold
    // nobody can read is a threshold nobody can raise.
    let detectors = &trail.body["detectors"];
    assert!(
        detectors["explanation"].as_str().unwrap_or_default().len() > 40,
        "the detectors must explain themselves: {detectors}"
    );
    assert_eq!(
        detectors["hard_rule_enforced"], false,
        "the hard rule ships off and the screen says so rather than hiding it"
    );

    // ── claim 2: a denial is a row like any other ────────────────────────────────────────────────
    // A revoked lease redemption is a real denial with a real request id, produced by a real
    // handler rather than a hand-written row — which is the only kind worth asserting.
    let issued = call(
        &state,
        post(
            &format!("/api/v1/secrets/{secret_id}/lease"),
            &token,
            json!({ "consumer": "audit-suite", "environment": "staging", "ttl_seconds": 60 }),
        ),
    )
    .await;
    assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.raw);
    let lease_id: Uuid = issued.body["id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .expect("the issued lease must carry an id");
    let revoked = call(
        &state,
        post(
            &format!("/api/v1/secret-leases/{lease_id}/revoke"),
            &token,
            json!({ "reason": "audit suite" }),
        ),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.raw);

    let after_denial = call(&state, get("/api/v1/secrets/audit?limit=200", &token)).await;
    let rows = after_denial.body["entries"]
        .as_array()
        .expect("entries must be an array");
    let lease_row = rows
        .iter()
        .find(|row| row["lease_id"] == lease_id.to_string())
        .expect("the lease operation must be in the trail");
    assert_eq!(
        lease_row["action"], "secret.lease",
        "a lease operation must name its action"
    );
    assert!(
        lease_row["request_id"].is_string(),
        "every row must carry the request id the caller was handed: {lease_row}"
    );
    assert!(
        lease_row["ip_address"].is_string(),
        "every row must carry the peer address: {lease_row}"
    );

    // ── claim 3: an off-hours reveal raises a flag, and the acknowledge persists ─────────────────
    let off_hours_request: Uuid = Uuid::new_v4();
    record_reveal(&db, secret_id, None, off_hours_request, 3).await;

    let flagged = call(&state, get("/api/v1/secrets/audit/anomalies", &token)).await;
    assert_eq!(flagged.status, StatusCode::OK, "{}", flagged.raw);
    let anomalies = flagged.body["anomalies"]
        .as_array()
        .expect("anomalies must be an array");
    let off_hours = anomalies
        .iter()
        .find(|row| row["pattern"] == "off_hours_reveal")
        .expect("a 03:00 reveal must raise the off-hours flag");
    assert_eq!(
        off_hours["request_id"], off_hours_request.to_string(),
        "the flag must join to the request that raised it"
    );
    assert_eq!(
        off_hours["severity"], "advisory",
        "a flag is advisory; the hard rule is what would make it blocking and it ships off"
    );
    assert_eq!(
        off_hours["acknowledged_at"], Value::Null,
        "a fresh flag is unacknowledged"
    );

    let flag_id = off_hours["id"].as_i64().expect("the flag must carry an id");
    let acknowledged = call(
        &state,
        patch(
            &format!("/api/v1/secrets/audit/anomalies/{flag_id}/acknowledge"),
            &token,
            json!({}),
        ),
    )
    .await;
    assert_eq!(
        acknowledged.status,
        StatusCode::OK,
        "the acknowledge must be accepted: {}",
        acknowledged.raw
    );
    assert_eq!(acknowledged.body["state"], "acknowledged");

    // Re-read: the acknowledge must have persisted, which is the whole point of the action.
    let reread = call(&state, get("/api/v1/secrets/audit/anomalies", &token)).await;
    let cleared = reread.body["anomalies"]
        .as_array()
        .expect("anomalies must be an array")
        .iter()
        .find(|row| row["id"].as_i64() == Some(flag_id))
        .expect("the flag must still be listed after being cleared");
    assert!(
        cleared["acknowledged_at"].is_string(),
        "the acknowledge must persist: {cleared}"
    );

    // And a second acknowledge says so rather than claiming a change that did not happen.
    let again = call(
        &state,
        patch(
            &format!("/api/v1/secrets/audit/anomalies/{flag_id}/acknowledge"),
            &token,
            json!({}),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.raw);
    assert_eq!(
        again.body["state"], "already_acknowledged",
        "a double click is a normal thing for a panel to receive; it must not report a change"
    );

    // ── claim 4: the export carries metadata only ────────────────────────────────────────────────
    // The feed is the one path that leaves the installation, so it gets the raw-bytes grep rather
    // than a field check: a field check would pass against a redaction pass that had not yet
    // heard of the next column someone adds to `audit_log`.
    let exported = call(&state, get("/api/v1/secrets/audit/export?limit=200", &token)).await;
    assert_eq!(exported.status, StatusCode::OK, "{}", exported.raw);
    assert!(
        !exported.raw.contains(FIXTURE_VALUE),
        "the SIEM feed must carry no credential value: {}",
        exported.raw
    );
    assert!(
        !exported.raw.contains("wrapping-key-id-not-a-value"),
        "the feed must carry no masked fragment either: {}",
        exported.raw
    );

    // And it must actually be a feed: one JSON object per line, each with the join key.
    let lines: Vec<&str> = exported
        .raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert!(!lines.is_empty(), "the export must not be empty: {}", exported.raw);
    for line in &lines {
        let record: Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("every line must be one JSON object ({err}): {line}"));
        assert!(
            record.get("request_id").is_some()
                && record.get("action").is_some()
                && record.get("actor_type").is_some(),
            "each record must carry the join key: {record}"
        );
        // The allowlist itself: a field the projection does not name cannot be on the output.
        assert!(
            record.get("metadata").is_none() && record.get("envelope").is_none(),
            "the feed is a projection with an allowlist, not a whole row: {record}"
        );
    }

    // The export is itself audited. "Who pulled the whole secrets trail into an external system"
    // is precisely the question an operator cannot answer afterwards.
    let after_export = call(&state, get("/api/v1/secrets/audit?limit=200", &token)).await;
    assert!(
        after_export.raw.contains("secret.audit.exported"),
        "the export must write its own row"
    );

    // ── the narrowing and widening properties ────────────────────────────────────────────────────
    // An unknown action is dropped rather than forwarded, so a crafted `?action=` cannot turn a
    // `secrets.audit` surface into a way to read another feature's audit rows.
    let narrowed = call(
        &state,
        get("/api/v1/secrets/audit?action=iam.role.created&limit=200", &token),
    )
    .await;
    assert_eq!(narrowed.status, StatusCode::OK, "{}", narrowed.raw);
    assert_eq!(
        narrowed.body["entries"]
            .as_array()
            .expect("entries must be an array")
            .len(),
        0,
        "an action this crate does not write must narrow the result to nothing, not widen it"
    );

    // The join key works as a filter — the property the whole request-id column exists for.
    let joined = call(
        &state,
        get(
            &format!("/api/v1/secrets/audit?request_id={off_hours_request}"),
            &token,
        ),
    )
    .await;
    let joined_rows = joined.body["entries"]
        .as_array()
        .expect("entries must be an array");
    assert!(
        !joined_rows.is_empty()
            && joined_rows
                .iter()
                .all(|row| row["request_id"] == off_hours_request.to_string()),
        "filtering by a request id must return only that request's rows: {joined_rows:?}"
    );

    // A malformed `since` is refused by name rather than silently ignored — a filter that quietly
    // does nothing is worse than one that says it could not be read.
    let bad_since = call(
        &state,
        get("/api/v1/secrets/audit?since=yesterday", &token),
    )
    .await;
    assert_eq!(
        bad_since.status,
        StatusCode::BAD_REQUEST,
        "a malformed instant must be refused: {}",
        bad_since.raw
    );
    assert_eq!(bad_since.body["error"]["code"], "invalid_since");

    // The export accepts a bounded slice; an unbounded one is capped rather than refused, because
    // a cap answers the question and a refusal does not.
    let capped = call(
        &state,
        get("/api/v1/secrets/audit/export?limit=99999", &token),
    )
    .await;
    assert_eq!(capped.status, StatusCode::OK, "{}", capped.raw);
}
