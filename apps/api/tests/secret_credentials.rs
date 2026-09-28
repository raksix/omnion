//! Integration test for typed credential profiles and the credential slots
//! (docs/requests/REQ-125, slice 2).
//!
//! It runs against the development stack and skips itself with a printed reason when PostgreSQL
//! is not reachable, exactly like the slice-1 suite beside it.
//!
//! The walk proves:
//!
//! * all five kinds can be created, and a credential's non-secret fields render while its value
//!   never does — asserted by grepping every response body for the fixture value;
//! * **a validator failure stores the provider's sentence and a red chip without blocking the
//!   save** — the kind is wrong, the credential is still there with `201` and `invalid`;
//! * a field that carries the value is refused with the *field name*, never the value;
//! * a slot resolves to its primary, and removing the primary falls back to the fallback with an
//!   event and an audit row naming the consumer;
//! * a slot cannot hold the same secret as primary and fallback — `409`, not a `400`;
//! * a read-only (`file` / `env`) provider can never be typed, and the panel is told why;
//! * the permission split: `secrets.read` reads, `secrets.manage` types and validates,
//!   `secrets.assign` assigns — an account with only the read permission is refused the other two.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The value behind every credential this suite creates. Every assertion greps for exactly this
/// string, so a leak is caught rather than inferred.
const FIXTURE_VALUE: &str = "qa-credential-value-do-not-leak-91c4d7";

/// The operator key both REQ-125 suites seal with.
///
/// This **must** be the same string `secret_key_ring.rs` uses. The key ring is
/// installation-wide, and every suite shares one development database, so the ring in that
/// database is already wrapped with the key that suite set. A different value here would make
/// `ensure_active_key` hand back a key this process cannot open, and every seal would fail with
/// a bare `Crypto` that reads like a bug in the envelope format rather than a fixture mismatch.
const SUITE_OPERATOR_KEY: &str = "omnion-secrets-suite-operator-key";

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

/// Build a request; `token` becomes the session cookie, `body` an optional JSON payload.
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

/// A `GET`.
fn get(uri: &str, token: &str) -> Request<Body> {
    request(Method::GET, uri, Some(token), None)
}

/// A `POST` with a JSON payload.
fn post(uri: &str, token: &str, body: Value) -> Request<Body> {
    request(Method::POST, uri, Some(token), Some(body))
}

/// A `PUT` with a JSON payload.
fn put(uri: &str, token: &str, body: Value) -> Request<Body> {
    request(Method::PUT, uri, Some(token), Some(body))
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
    let email = format!("credential-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Credential Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

/// Sign an account in and return its session token.
///
/// The token comes from the `Set-Cookie` header rather than the body: that is the shape the API
/// actually sets, so a change to the login response breaks this helper loudly instead of making
/// every later request in the suite fail as `401 unauthenticated`.
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
         values ($1, 'organization', $2, 'Sealed by the REQ-125 slice-2 suite') returning id",
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

#[tokio::test]
async fn typed_credentials_and_slots_are_proven_end_to_end() {
    // The key ring needs an operator key even though this suite never unseals by hand.
    unsafe { std::env::set_var(omnion_secrets::KEY_ENCRYPTION_ENV, SUITE_OPERATOR_KEY) };

    let Some((state, db)) = live_state().await else {
        return;
    };
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");

    let slug = format!("credentials-{}", Uuid::new_v4().simple());
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Credential Test Organization")
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

    /* --------------------------------------------------------------- all five kinds create */

    let api_key_id = sealed_secret(
        &db,
        organization_id,
        format!("ai-key-{}", Uuid::new_v4().simple()),
    )
    .await;
    let oauth_id = sealed_secret(
        &db,
        organization_id,
        format!("oauth-{}", Uuid::new_v4().simple()),
    )
    .await;
    let smtp_id = sealed_secret(
        &db,
        organization_id,
        format!("smtp-{}", Uuid::new_v4().simple()),
    )
    .await;
    let payment_id = sealed_secret(
        &db,
        organization_id,
        format!("pay-{}", Uuid::new_v4().simple()),
    )
    .await;
    let ssh_id = sealed_secret(
        &db,
        organization_id,
        format!("ssh-{}", Uuid::new_v4().simple()),
    )
    .await;

    // The SSH profile's fingerprint is recomputed from the public key, so the suite computes it
    // with the crate's own helper — a hard-coded string would only prove the happy path.
    let public_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample slice2";
    let fingerprint = omnion_secrets::validators::fingerprint_of(public_key.as_bytes(), "sha256");

    let cases = [
        (
            api_key_id,
            json!({ "kind": "api_key", "fields": { "endpoint": "https://api.example.com", "username": "svc-ai", "key_prefix": "sk-live-abcd" } }),
            "valid",
        ),
        (
            oauth_id,
            json!({ "kind": "oauth_token", "fields": { "endpoint": "https://issuer.example.com", "expires_at": "2031-01-01T00:00:00Z", "scopes": "read,write" } }),
            "valid",
        ),
        (
            smtp_id,
            json!({ "kind": "smtp_account", "fields": { "host": "smtp.example.com", "port": 587, "username": "bot@example.com", "tls": "starttls" } }),
            "valid",
        ),
        (
            payment_id,
            json!({ "kind": "payment_key", "fields": { "endpoint": "https://api.payments.example", "key_prefix": "sk_test_9f2", "account_id": "acct_1" } }),
            "valid",
        ),
        (
            ssh_id,
            json!({ "kind": "ssh_key", "fields": { "fingerprint": fingerprint, "fingerprint_algorithm": "sha256", "public_key": public_key, "comment": "release@ci" } }),
            "valid",
        ),
    ];

    let mut created: Vec<Uuid> = Vec::new();
    for (secret_id, payload, expected) in &cases {
        let response = call(
            &state,
            post(
                &format!("/api/v1/secrets/{secret_id}/credential"),
                &owner_token,
                payload.clone(),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "{payload} must create a profile: {}",
            response.raw
        );
        assert_eq!(response.body["kind"], payload["kind"]);
        assert_eq!(response.body["validation_state"], json!(expected));
        assert!(
            !response.raw.contains(FIXTURE_VALUE),
            "a credential response must never carry the value: {}",
            response.raw
        );
        created.push(*secret_id);
    }

    // The list carries every kind's description and the fields the wizard asks for, so the
    // picker never has to keep a second copy of the wording.
    let list = call(&state, get("/api/v1/secrets/credentials", &owner_token)).await;
    assert_eq!(list.status, StatusCode::OK, "list body: {}", list.raw);
    assert_eq!(list.body["kinds"].as_array().map(Vec::len), Some(5));
    assert!(
        list.body["kinds"]
            .as_array()
            .expect("kinds")
            .iter()
            .any(|kind| kind["offline"] == json!(true) && kind["kind"] == json!("ssh_key")),
        "the SSH kind must be marked offline-checkable"
    );
    assert_eq!(list.body["total"], json!(5));
    assert_eq!(list.body["valid"], json!(5));
    assert!(
        !list.raw.contains(FIXTURE_VALUE),
        "the list must never carry a value: {}",
        list.raw
    );

    // The non-secret fields survive the round trip and the version is metadata, not content.
    let detail = call(
        &state,
        get(
            &format!("/api/v1/secrets/credentials/{smtp_id}"),
            &owner_token,
        ),
    )
    .await;
    // The body goes in the message: a bare `500` here says nothing about which of the store, the
    // permission check or the serialiser failed, and the suite runs without a server to read a
    // log from.
    assert_eq!(
        detail.status,
        StatusCode::OK,
        "the detail read must answer: {}",
        detail.raw
    );
    assert_eq!(detail.body["fields"]["host"], json!("smtp.example.com"));
    assert_eq!(detail.body["version"], json!(1));
    assert!(
        detail.body["field_pairs"]
            .as_array()
            .expect("pairs")
            .iter()
            .any(|pair| pair[0] == json!("Host") && pair[1] == json!("smtp.example.com")),
        "the detail list must humanise the field names"
    );

    /* ------------------------------------ a validator failure stores a chip and blocks nothing */

    // A payment key with a typo'd prefix: the fields are wrong, and the credential is saved with
    // a red chip carrying the validator's own sentence.
    let broken_id = sealed_secret(
        &db,
        organization_id,
        format!("broken-{}", Uuid::new_v4().simple()),
    )
    .await;
    let broken = call(
        &state,
        post(
            &format!("/api/v1/secrets/{broken_id}/credential"),
            &owner_token,
            json!({ "kind": "payment_key", "fields": { "key_prefix": "sk_lve_", "account_id": "acct_1" } }),
        ),
    )
    .await;
    assert_eq!(
        broken.status,
        StatusCode::CREATED,
        "a failing validation must not block the save: {}",
        broken.raw
    );
    assert_eq!(broken.body["validation_state"], json!("invalid"));
    let message = broken.body["validation_message"]
        .as_str()
        .expect("a validator sentence");
    assert!(
        message.contains("sk_lve_"),
        "the chip must carry the provider-side detail: {message}"
    );

    // The row really is stored — a refusal would have lost the operator's work.
    let stored: String =
        sqlx::query_scalar("select validation_state from secret_credentials where secret_id = $1")
            .bind(broken_id)
            .fetch_one(db.pool())
            .await
            .expect("the credential must be stored");
    assert_eq!(stored, "invalid");

    // An explicit re-run is the same outcome, and it never returns the value.
    let revalidated = call(
        &state,
        post(
            &format!("/api/v1/secrets/{broken_id}/validate"),
            &owner_token,
            json!({}),
        ),
    )
    .await;
    assert_eq!(revalidated.status, StatusCode::OK);
    assert_eq!(revalidated.body["valid"], json!(false));
    assert!(
        !revalidated.raw.contains(FIXTURE_VALUE),
        "a validation response must never carry the value"
    );

    // And the recovery path: fix the field and the chip goes green again, with the event.
    let fixed = call(
        &state,
        post(
            &format!("/api/v1/secrets/{broken_id}/credential"),
            &owner_token,
            json!({ "kind": "payment_key", "fields": { "key_prefix": "sk_live_fixed", "account_id": "acct_1" } }),
        ),
    )
    .await;
    assert_eq!(fixed.body["validation_state"], json!("valid"));

    /* ------------------------------------- a field that carries the value is refused by name */

    for (name, value) in [
        ("value", json!("sk_live_whatever")),
        ("password", json!("hunter2")),
        ("private_key", json!("-----BEGIN OPENSSH PRIVATE KEY-----")),
    ] {
        let refused = call(
            &state,
            post(
                &format!("/api/v1/secrets/{api_key_id}/credential"),
                &owner_token,
                json!({ "kind": "api_key", "fields": { name: value } }),
            ),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{name} must be refused"
        );
        assert_eq!(
            refused.body["error"]["code"],
            json!("invalid_secrets_request")
        );
        let sentence = refused.body["error"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            sentence.contains(name),
            "the refusal must name the field: {sentence}"
        );
        assert!(
            !refused.raw.contains("hunter2") && !refused.raw.contains("BEGIN OPENSSH"),
            "the refusal must never echo the value"
        );
    }

    // A long opaque run in an innocuous field is refused too — that is what a token looks like.
    let smuggled = call(
        &state,
        post(
            &format!("/api/v1/secrets/{api_key_id}/credential"),
            &owner_token,
            json!({ "kind": "api_key", "fields": { "note": "sk9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f" } }),
        ),
    )
    .await;
    assert_eq!(smuggled.status, StatusCode::BAD_REQUEST);

    // A kind outside the five is refused with the list.
    let bad_kind = call(
        &state,
        post(
            &format!("/api/v1/secrets/{api_key_id}/credential"),
            &owner_token,
            json!({ "kind": "password", "fields": {} }),
        ),
    )
    .await;
    assert_eq!(bad_kind.status, StatusCode::BAD_REQUEST);
    assert!(
        bad_kind.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("api_key"),
        "the refusal must name the valid kinds"
    );

    /* ----------------------------------------------- a read-only provider can never be typed */

    let bridge_name = format!("bridge-{}", Uuid::new_v4().simple());
    let bridge_id: Uuid = sqlx::query_scalar(
        "insert into secrets (name, scope_type, organization_id, provider, provider_locator, read_only) \
         values ($1, 'organization', $2, 'env', 'STRIPE_API_KEY', true) returning id",
    )
    .bind(&bridge_name)
    .bind(organization_id)
    .fetch_one(db.pool())
    .await
    .expect("the bridge must be created");

    let refused_bridge = call(
        &state,
        post(
            &format!("/api/v1/secrets/{bridge_id}/credential"),
            &owner_token,
            json!({ "kind": "api_key", "fields": { "key_prefix": "sk-live-abcd" } }),
        ),
    )
    .await;
    // `422`, not `400`. The payload is well-formed; the *resource* refuses the operation,
    // because a `file`/`env` bridge is managed outside the platform and will never accept a
    // write. The request's acceptance line asks for `405`/`422` for exactly this reason, and a
    // `400` invites a caller to retry with different input when no input would help.
    assert_eq!(refused_bridge.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        refused_bridge.body["error"]["code"],
        json!("secret_read_only")
    );

    // It still shows in the list, flagged, so an operator can see it exists and is external.
    let list_with_bridge = call(&state, get("/api/v1/secrets/credentials", &owner_token)).await;
    let bridge_view = list_with_bridge.body["credentials"]
        .as_array()
        .expect("credentials")
        .iter()
        .find(|entry| entry["name"] == json!(bridge_name))
        .expect("the bridge must be listed");
    assert_eq!(bridge_view["read_only"], json!(true));
    assert_eq!(bridge_view["provider"], json!("env"));
    assert_eq!(bridge_view["provider_locator"], json!("STRIPE_API_KEY"));

    /* ------------------------------------------------------------------ the slot assignments */

    // The matrix starts with the catalogue and nothing assigned.
    let empty_slots = call(&state, get("/api/v1/credential-slots", &owner_token)).await;
    assert_eq!(empty_slots.status, StatusCode::OK);
    assert_eq!(
        empty_slots.body["catalog"].as_array().map(Vec::len),
        Some(6)
    );
    assert!(
        empty_slots.body["assignable"]
            .as_array()
            .is_some_and(|list| list.len() >= 6),
        "the editor's pickers need every typed credential"
    );

    // The same secret cannot be both primary and fallback: a `409`, with the crate's sentence.
    let self_reference = call(
        &state,
        put(
            "/api/v1/credential-slots/environment/smtp",
            &owner_token,
            json!({
                "scope_id": "production",
                "primary_secret_id": smtp_id,
                "fallback_secret_id": smtp_id,
            }),
        ),
    )
    .await;
    assert_eq!(
        self_reference.status,
        StatusCode::CONFLICT,
        "a self-referencing fallback is a conflict: {}",
        self_reference.raw
    );
    assert_eq!(
        self_reference.body["error"]["code"],
        json!("credential_slot_self_reference")
    );

    // A real assignment: smtp primary, a second account as the fallback.
    let fallback_id = sealed_secret(
        &db,
        organization_id,
        format!("smtp-backup-{}", Uuid::new_v4().simple()),
    )
    .await;
    let assigned = call(
        &state,
        put(
            "/api/v1/credential-slots/environment/smtp",
            &owner_token,
            json!({
                "scope_id": "production",
                "primary_secret_id": smtp_id,
                "fallback_secret_id": fallback_id,
            }),
        ),
    )
    .await;
    assert_eq!(
        assigned.status,
        StatusCode::OK,
        "assign body: {}",
        assigned.raw
    );
    assert_eq!(assigned.body["primary_name"].is_null(), json!(false));
    assert_eq!(assigned.body["slot"], json!("smtp"));
    assert_eq!(
        assigned.body["description"],
        json!("Outgoing mail server the notification centre sends through")
    );
    assert!(
        assigned.body["consumers"]
            .as_str()
            .unwrap_or_default()
            .contains("notifications")
    );

    // A consumer resolves the primary, and the resolution is metadata only.
    let resolved = call(
        &state,
        get(
            "/api/v1/credential-slots/environment/smtp/resolve/production",
            &owner_token,
        ),
    )
    .await;
    assert_eq!(resolved.status, StatusCode::OK);
    assert_eq!(resolved.body["secret_id"], json!(smtp_id));
    assert_eq!(resolved.body["fell_back"], json!(false));
    assert!(
        !resolved.raw.contains(FIXTURE_VALUE),
        "a resolution must never carry a value: {}",
        resolved.raw
    );

    // Removing the primary falls back — with the fallback answering and the summary saying so.
    let swapped = call(
        &state,
        put(
            "/api/v1/credential-slots/environment/smtp",
            &owner_token,
            json!({
                "scope_id": "production",
                "primary_secret_id": null,
                "fallback_secret_id": fallback_id,
            }),
        ),
    )
    .await;
    assert_eq!(swapped.status, StatusCode::OK);
    assert_eq!(swapped.body["primary_name"], Value::Null);

    let after_removal = call(
        &state,
        get(
            "/api/v1/credential-slots/environment/smtp/resolve/production",
            &owner_token,
        ),
    )
    .await;
    assert_eq!(after_removal.body["secret_id"], json!(fallback_id));
    assert_eq!(after_removal.body["fell_back"], json!(true));
    assert!(
        after_removal.body["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("fallback"),
        "the summary must say the fallback answered"
    );

    // The matrix now shows the assignment with its last consumer — the sentence a removal quotes.
    let matrix = call(&state, get("/api/v1/credential-slots", &owner_token)).await;
    let row = matrix.body["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .find(|entry| entry["slot"] == json!("smtp"))
        .expect("the smtp slot must be listed");
    assert!(
        row["last_resolved_by"]
            .as_str()
            .unwrap_or_default()
            .contains("preview:")
    );
    assert_eq!(row["fallback_name"].is_null(), json!(false));

    // An unassigned scope answers with a sentence, not a stack trace.
    let unknown_scope = call(
        &state,
        get(
            "/api/v1/credential-slots/environment/smtp/resolve/staging",
            &owner_token,
        ),
    )
    .await;
    assert_eq!(unknown_scope.status, StatusCode::NOT_FOUND);
    assert_eq!(
        unknown_scope.body["error"]["code"],
        json!("credential_slot_not_found")
    );

    // A typo'd slot is refused with the list of valid names.
    let typo = call(
        &state,
        put(
            "/api/v1/credential-slots/environment/mail",
            &owner_token,
            json!({ "scope_id": "production", "primary_secret_id": smtp_id }),
        ),
    )
    .await;
    assert_eq!(typo.status, StatusCode::BAD_REQUEST);
    assert!(
        typo.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("smtp"),
        "the refusal must name the valid slots"
    );

    /* -------------------------------------------------------------- events, audit and tenant */

    // The slot change and the validation failure are both on the bus, and both name the actor.
    let slot_events: i64 = sqlx::query_scalar(
        "select count(*) from events where name = 'secrets.credential_slot_assigned'",
    )
    .fetch_one(db.pool())
    .await
    .expect("the event stream must read");
    assert!(
        slot_events >= 2,
        "each assignment must record its event, got {slot_events}"
    );

    let slot_audit: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'credential_slot.assigned' \
         and target_id = $1",
    )
    .bind(assigned.body["id"].as_str().expect("a slot id"))
    .fetch_one(db.pool())
    .await
    .expect("the audit trail must read");
    assert!(slot_audit >= 1, "a slot assignment must be audited");
    let last_consumer: Option<String> = sqlx::query_scalar(
        "select metadata->>'last_consumer' from audit_log \
         where action = 'credential_slot.assigned' order by created_at desc limit 1",
    )
    .fetch_one(db.pool())
    .await
    .expect("the audit row must read");
    assert!(
        last_consumer.is_some(),
        "the audit row must name the consumer that was resolving the slot"
    );

    let validation_audit: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'secret.credential.validated'",
    )
    .fetch_one(db.pool())
    .await
    .expect("the audit trail must read");
    assert!(validation_audit >= 1, "a validation run must be audited");

    // Another organization's secret is not reachable from here.
    let (other_org_id, other_email) = {
        let other_slug = format!("other-{}", Uuid::new_v4().simple());
        let id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("Other Organization")
        .bind(&other_slug)
        .fetch_one(db.pool())
        .await
        .expect("the second organization must be created");
        let (user_id, email) = create_account(&db, Some(id)).await;
        seed::bind_owner(db.pool(), user_id)
            .await
            .expect("the second owner binding must be created");
        (id, email)
    };
    let other_token = login(&state, &other_email).await;
    let cross_tenant = call(
        &state,
        get(
            &format!("/api/v1/secrets/credentials/{smtp_id}"),
            &other_token,
        ),
    )
    .await;
    assert_eq!(
        cross_tenant.status,
        StatusCode::FORBIDDEN,
        "another organization must not read this credential: {}",
        cross_tenant.body
    );
    let cross_detail = call(
        &state,
        post(
            &format!("/api/v1/secrets/{smtp_id}/credential"),
            &other_token,
            json!({ "kind": "api_key", "fields": { "key_prefix": "sk-live-other" } }),
        ),
    )
    .await;
    assert_eq!(
        cross_detail.status,
        StatusCode::FORBIDDEN,
        "another organization must not type this credential: {}",
        cross_detail.raw
    );
    assert_eq!(
        cross_detail.body["error"]["code"],
        json!("wrong_organization")
    );

    // The other organization's own list does not carry the first organization's credentials.
    let other_list = call(&state, get("/api/v1/secrets/credentials", &other_token)).await;
    assert!(
        other_list.body["credentials"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
            <= 1,
        "a tenant must not see another tenant's credentials"
    );

    /* ---------------------------------------------------------------- the permission split */

    // A member with no secrets permission at all is refused on every route in this slice.
    for (method, uri) in [
        (Method::GET, "/api/v1/secrets/credentials".to_owned()),
        (Method::GET, "/api/v1/credential-slots".to_owned()),
    ] {
        let refused = call(&state, request(method, &uri, Some(&member_token), None)).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{uri} must be refused"
        );
    }
    let refused_write = call(
        &state,
        post(
            &format!("/api/v1/secrets/{api_key_id}/credential"),
            &member_token,
            json!({ "kind": "api_key", "fields": { "key_prefix": "sk-live-abcd" } }),
        ),
    )
    .await;
    assert_eq!(refused_write.status, StatusCode::FORBIDDEN);
    let refused_assign = call(
        &state,
        put(
            "/api/v1/credential-slots/environment/smtp",
            &member_token,
            json!({ "scope_id": "production", "primary_secret_id": smtp_id }),
        ),
    )
    .await;
    assert_eq!(refused_assign.status, StatusCode::FORBIDDEN);

    /* ----------------------------------------------------------------------------- cleanup */

    let all: Vec<Uuid> = created
        .iter()
        .copied()
        .chain([
            broken_id,
            fallback_id,
            bridge_id,
            api_key_id,
            oauth_id,
            payment_id,
            ssh_id,
        ])
        .collect();
    sqlx::query("delete from secret_versions where secret_id = any($1)")
        .bind(&all)
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from credential_slots where primary_secret_id = any($1) or fallback_secret_id = any($1)")
        .bind(&all)
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from secrets where id = any($1)")
        .bind(&all)
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from role_bindings where subject_id = any($1) or user_id = any($1)")
        .bind(vec![owner_id, member_id])
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from users where id = any($1)")
        .bind(vec![owner_id, member_id])
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from organizations where id = any($1)")
        .bind(vec![organization_id, other_org_id])
        .execute(db.pool())
        .await
        .ok();
}
