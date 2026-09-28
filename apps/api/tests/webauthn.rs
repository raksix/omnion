//! Integration test for the passkey surface (REQ-006, slice 3b): enrolment and the assertion
//! that finishes a sign-in, driven over the real router with a **software authenticator**.
//!
//! The authenticator is a P-256 key pair this test holds; it builds the same blobs a browser
//! would (client data JSON, authenticator data, a CBOR attestation object, a DER signature) and
//! posts them to the API. That is what makes this a ceremony test rather than a mock: nothing
//! on the server side is stubbed — the challenge, the origin rule, the relying party hash and
//! the signature are all verified for real.
//!
//! The origins here are loopback ones (`http://127.0.0.1:3100`, the QA panel), which is exactly
//! the documented exception in `omnion_identity::webauthn`.
//!
//! Like the other IAM suites these tests run against the development stack and skip themselves
//! with a printed reason when PostgreSQL is not reachable.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;

/// Password of the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The origin the browser would report: the QA panel on loopback (the documented exception).
const ORIGIN: &str = "http://127.0.0.1:3100";

/// Result of one in-process HTTP call.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let uri = request.uri().to_string();
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => panic!(
                "the {uri} call answered {status} with a non-JSON body: {}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(300)])
            ),
        }
    };

    TestResponse {
        status,
        set_cookie,
        body,
    }
}

/// Build a request; `token` becomes the session cookie and `body` the JSON payload.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };

    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

/// The session token a response set, if any.
fn cookie_token(response: &TestResponse) -> String {
    let cookie = response
        .set_cookie
        .as_ref()
        .expect("the response must set a session cookie");
    cookie
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is key=value")
        .1
        .to_owned()
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            None
        }
    }
}

/// A state whose database has all migrations applied.
async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = live_db(&config).await?;
    db.migrate().await.expect("migrations must apply");

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let storage = omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        storage,
    );
    Some((state, db))
}

/// A fresh account in its own organization, without any role binding.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("webauthn-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "WebAuthn Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Sign in with the password; the response is whatever the API answers (a cookie or a challenge).
async fn login(state: &AppState, email: &str) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": email, "password": PASSWORD })),
        ),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// The software authenticator
// ---------------------------------------------------------------------------------------------

/// A CBOR head for a small item.
fn head(major: u8, length: usize) -> Vec<u8> {
    let mut out = Vec::new();
    match length {
        0..=23 => out.push((major << 5) | u8::try_from(length).expect("small")),
        24..=255 => {
            out.push((major << 5) | 24);
            out.push(u8::try_from(length).expect("one byte"));
        }
        _ => {
            out.push((major << 5) | 25);
            out.push(u8::try_from(length >> 8).expect("two bytes"));
            out.push(u8::try_from(length & 0xff).expect("two bytes"));
        }
    }
    out
}

fn cbor_text(text: &str) -> Vec<u8> {
    let mut out = head(3, text.len());
    out.extend_from_slice(text.as_bytes());
    out
}

fn cbor_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = head(2, data.len());
    out.extend_from_slice(data);
    out
}

fn cbor_uint(value: u64) -> Vec<u8> {
    head(0, usize::try_from(value).expect("small"))
}

fn cbor_neg(value: i64) -> Vec<u8> {
    head(1, usize::try_from(-1 - value).expect("small"))
}

/// A CBOR map from already-encoded keys and values.
fn cbor_map(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = head(5, entries.len());
    for (key, value) in entries {
        out.extend_from_slice(key);
        out.extend_from_slice(value);
    }
    out
}

/// A passkey the test holds the private half of.
struct SoftwareAuthenticator {
    /// The P-256 signer.
    signing: p256::ecdsa::SigningKey,
    /// The credential id's raw bytes (the browser reports its base64url form).
    credential_id_bytes: Vec<u8>,
}

impl SoftwareAuthenticator {
    /// A new authenticator with a random P-256 key.
    fn new() -> Self {
        Self {
            signing: p256::ecdsa::SigningKey::random(&mut rand::rngs::OsRng),
            credential_id_bytes: Uuid::new_v4().as_bytes().to_vec(),
        }
    }

    /// The credential id as the browser serialises it.
    fn credential_id(&self) -> String {
        B64.encode(&self.credential_id_bytes)
    }

    /// The COSE public key the attestation carries.
    fn cose_key(&self) -> Vec<u8> {
        let point = self.signing.verifying_key().to_encoded_point(false);
        let x = point.x().expect("x").to_vec();
        let y = point.y().expect("y").to_vec();
        cbor_map(&[
            (cbor_uint(1), cbor_uint(2)),
            (cbor_uint(3), cbor_neg(-7)),
            (cbor_neg(-1), cbor_uint(1)),
            (cbor_neg(-2), cbor_bytes(&x)),
            (cbor_neg(-3), cbor_bytes(&y)),
        ])
    }

    /// Sign a message the way the authenticator does (ECDSA, DER).
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        use p256::ecdsa::signature::Signer;
        let signature: p256::ecdsa::Signature = self.signing.sign(message);
        signature.to_der().as_bytes().to_vec()
    }

    /// Authenticator data; with attested credential data when `credential` is true.
    fn auth_data(&self, rp_id: &str, sign_count: u32, credential: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&Sha256::digest(rp_id.as_bytes()));
        out.push(if credential { 0x41 } else { 0x01 }); // UP | AT / UP
        out.extend_from_slice(&sign_count.to_be_bytes());
        if credential {
            out.extend_from_slice(&[0; 16]); // aaguid
            out.extend_from_slice(
                &u16::try_from(self.credential_id_bytes.len())
                    .expect("a credential id fits two bytes")
                    .to_be_bytes(),
            );
            out.extend_from_slice(&self.credential_id_bytes);
            out.extend_from_slice(&self.cose_key());
        }
        out
    }

    /// The client data JSON for a ceremony.
    fn client_data(ceremony: &str, challenge: &str, origin: &str) -> String {
        json!({
            "type": ceremony,
            "challenge": challenge,
            "origin": origin,
            "crossOrigin": false,
        })
        .to_string()
    }

    /// What `register/complete` expects: the registration credential.
    ///
    /// The client data travels as the JSON **text** (it is what gets hashed), while the binary
    /// blobs travel base64url-encoded, exactly as a browser serialises them.
    fn registration(&self, challenge: &str, rp_id: &str, origin: &str) -> Value {
        let auth_data = self.auth_data(rp_id, 0, true);
        let attestation = cbor_map(&[
            (cbor_text("fmt"), cbor_text("none")),
            (cbor_text("attStmt"), cbor_map(&[])),
            (cbor_text("authData"), cbor_bytes(&auth_data)),
        ]);
        json!({
            "id": self.credential_id(),
            "client_data_json": Self::client_data("webauthn.create", challenge, origin),
            "attestation_object": B64.encode(attestation),
            "transports": ["internal"],
        })
    }

    /// What `authenticate/complete` expects: the assertion credential.
    fn assertion(&self, challenge: &str, rp_id: &str, origin: &str, sign_count: u32) -> Value {
        let auth_data = self.auth_data(rp_id, sign_count, false);
        let client_data = Self::client_data("webauthn.get", challenge, origin);
        let mut message = auth_data.clone();
        message.extend_from_slice(&Sha256::digest(client_data.as_bytes()));
        let signature = self.sign(&message);
        json!({
            "id": self.credential_id(),
            "client_data_json": client_data,
            "authenticator_data": B64.encode(&auth_data),
            "signature": B64.encode(&signature),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_passkey_enrols_and_signs_in_end_to_end() {
    let Some((state, db)) = live_state().await else {
        return;
    };

    let slug = format!("webauthn-{}", Uuid::new_v4().simple());
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("WebAuthn Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the test organization must be created");
    let (user_id, email) = create_account(&db, Some(organization_id)).await;

    let session = login(&state, &email).await;
    assert_eq!(session.status, StatusCode::OK, "{}", session.body);
    let token = cookie_token(&session);

    let rp_id = omnion_identity::webauthn::rp_id_from_env();

    // ---- Enrolment -------------------------------------------------------------------------
    let options = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/register/begin",
            Some(&token),
            Some(json!({ "label": "QA key" })),
        ),
    )
    .await;
    assert_eq!(options.status, StatusCode::OK, "{}", options.body);
    assert_eq!(options.body["rp"]["id"], rp_id, "{}", options.body);
    let challenge = options.body["challenge"]
        .as_str()
        .expect("challenge")
        .to_owned();
    let algorithms: Vec<i64> = options.body["pubKeyCredParams"]
        .as_array()
        .expect("pubKeyCredParams")
        .iter()
        .filter_map(|entry| entry["alg"].as_i64())
        .collect();
    assert!(algorithms.contains(&-7), "{algorithms:?}");

    let authenticator = SoftwareAuthenticator::new();
    let registered = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/register/complete",
            Some(&token),
            Some(json!({
                "challenge": challenge,
                "label": "QA key",
                "credential": authenticator.registration(&challenge, &rp_id, ORIGIN),
            })),
        ),
    )
    .await;
    assert_eq!(
        registered.status,
        StatusCode::OK,
        "the registration must verify: {}",
        registered.body
    );
    assert_eq!(registered.body["factor"]["kind"], "webauthn");
    assert_eq!(registered.body["factor"]["confirmed"], true);
    assert_eq!(registered.body["algorithm"], "ES256");
    let factor_id = registered.body["factor"]["id"]
        .as_str()
        .expect("factor id")
        .to_owned();

    // The same ceremony cannot be replayed, and the same credential cannot be enrolled twice.
    let replayed = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/register/complete",
            Some(&token),
            Some(json!({
                "challenge": challenge,
                "credential": authenticator.registration(&challenge, &rp_id, ORIGIN),
            })),
        ),
    )
    .await;
    assert_eq!(
        replayed.status,
        StatusCode::BAD_REQUEST,
        "a spent ceremony challenge must be refused: {}",
        replayed.body
    );

    let second = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/register/begin",
            Some(&token),
            None,
        ),
    )
    .await;
    let second_challenge = second.body["challenge"]
        .as_str()
        .expect("challenge")
        .to_owned();
    let duplicate = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/register/complete",
            Some(&token),
            Some(json!({
                "challenge": second_challenge,
                "credential": authenticator.registration(&second_challenge, &rp_id, ORIGIN),
            })),
        ),
    )
    .await;
    assert_eq!(
        duplicate.status,
        StatusCode::CONFLICT,
        "one credential must not enrol twice: {}",
        duplicate.body
    );

    // A ceremony from an origin the installation does not serve is refused.
    let foreign = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/register/begin",
            Some(&token),
            None,
        ),
    )
    .await;
    let foreign_challenge = foreign.body["challenge"]
        .as_str()
        .expect("challenge")
        .to_owned();
    let refused = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/register/complete",
            Some(&token),
            Some(json!({
                "challenge": foreign_challenge,
                "credential": authenticator.registration(
                    &foreign_challenge,
                    &rp_id,
                    "https://evil.example",
                ),
            })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "webauthn_refused");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not accepted"),
        "{}",
        refused.body
    );

    let passkeys = call(
        &state,
        request(
            Method::GET,
            "/api/v1/auth/webauthn/passkeys",
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(passkeys.status, StatusCode::OK, "{}", passkeys.body);
    assert_eq!(
        passkeys.body["passkeys"].as_array().map(Vec::len),
        Some(1),
        "{}",
        passkeys.body
    );

    // ---- The sign-in -----------------------------------------------------------------------
    let challenged = login(&state, &email).await;
    assert_eq!(challenged.status, StatusCode::OK, "{}", challenged.body);
    assert_eq!(
        challenged.body["mfa_required"], true,
        "a passkey is a confirmed factor, so the password alone is not a session: {}",
        challenged.body
    );
    assert!(challenged.set_cookie.is_none());
    let login_challenge = challenged.body["challenge"]
        .as_str()
        .expect("challenge")
        .to_owned();

    let assertion_options = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/authenticate/begin",
            None,
            Some(json!({ "challenge": login_challenge })),
        ),
    )
    .await;
    assert_eq!(
        assertion_options.status,
        StatusCode::OK,
        "{}",
        assertion_options.body
    );
    let ceremony_challenge = assertion_options.body["challenge"]
        .as_str()
        .expect("ceremony challenge")
        .to_owned();
    let allowed = assertion_options.body["allowCredentials"]
        .as_array()
        .expect("allowCredentials");
    assert_eq!(allowed.len(), 1, "{}", assertion_options.body);
    assert_eq!(allowed[0]["id"], authenticator.credential_id());

    let verified = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/authenticate/complete",
            None,
            Some(json!({
                "challenge": login_challenge,
                "ceremony_challenge": ceremony_challenge,
                "credential": authenticator.assertion(&ceremony_challenge, &rp_id, ORIGIN, 5),
            })),
        ),
    )
    .await;
    assert_eq!(
        verified.status,
        StatusCode::OK,
        "the assertion must verify: {}",
        verified.body
    );
    let signed_in = cookie_token(&verified);

    let me = call(
        &state,
        request(Method::GET, "/api/v1/me", Some(&signed_in), None),
    )
    .await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.body["user"]["email"], email, "{}", me.body);

    // The session says how it was opened.
    let methods: Vec<String> = sqlx::query_scalar(
        "select auth_methods from sessions where user_id = $1 order by created_at desc limit 1",
    )
    .bind(user_id)
    .fetch_one(db.pool())
    .await
    .expect("the session row must be readable");
    assert!(methods.contains(&"password".to_owned()), "{methods:?}");
    assert!(methods.contains(&"webauthn".to_owned()), "{methods:?}");

    // The counter moved forward on the factor.
    let stored: i64 = sqlx::query_scalar("select sign_count from mfa_factors where id = $1")
        .bind(Uuid::parse_str(&factor_id).expect("factor id parses"))
        .fetch_one(db.pool())
        .await
        .expect("the factor row must be readable");
    assert_eq!(stored, 5, "the signature counter must be stored");

    // A replayed assertion (same counter) is refused, because the counter must move forward.
    let replayed_login = login(&state, &email).await;
    let replayed_challenge = replayed_login.body["challenge"]
        .as_str()
        .expect("challenge")
        .to_owned();
    let replayed_options = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/authenticate/begin",
            None,
            Some(json!({ "challenge": replayed_challenge })),
        ),
    )
    .await;
    let replayed_ceremony = replayed_options.body["challenge"]
        .as_str()
        .expect("challenge")
        .to_owned();
    let replayed = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/authenticate/complete",
            None,
            Some(json!({
                "challenge": replayed_challenge,
                "ceremony_challenge": replayed_ceremony,
                "credential": authenticator.assertion(&replayed_ceremony, &rp_id, ORIGIN, 5),
            })),
        ),
    )
    .await;
    assert_eq!(
        replayed.status,
        StatusCode::BAD_REQUEST,
        "a counter that does not move must be refused: {}",
        replayed.body
    );
    assert!(
        replayed.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("counter"),
        "{}",
        replayed.body
    );

    // A challenge this server never issued is refused on the sign-in half too.
    let unknown = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/webauthn/authenticate/begin",
            None,
            Some(json!({ "challenge": "not-a-challenge" })),
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED, "{}", unknown.body);

    // ---- Removal ---------------------------------------------------------------------------
    let refused_revoke = call(
        &state,
        request(
            Method::DELETE,
            &format!("/api/v1/auth/webauthn/passkeys/{factor_id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused_revoke.status,
        StatusCode::FORBIDDEN,
        "removing a confirmed factor demands a step-up: {}",
        refused_revoke.body
    );
    assert_eq!(refused_revoke.body["error"]["code"], "step_up_required");

    let stepped_up = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/step-up",
            Some(&token),
            Some(json!({ "password": PASSWORD })),
        ),
    )
    .await;
    assert_eq!(stepped_up.status, StatusCode::OK, "{}", stepped_up.body);

    let revoked = call(
        &state,
        request(
            Method::DELETE,
            &format!("/api/v1/auth/webauthn/passkeys/{factor_id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body["revoked"], true);

    // With the passkey gone the same sign-in is a plain one again.
    let plain = login(&state, &email).await;
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.body);
    assert_eq!(plain.body["mfa_required"], Value::Null, "{}", plain.body);

    // ---- Cleanup ---------------------------------------------------------------------------
    sqlx::query("delete from users where id = $1")
        .bind(user_id)
        .execute(db.pool())
        .await
        .expect("account cleanup must run");
    sqlx::query("delete from organizations where id = $1")
        .bind(organization_id)
        .execute(db.pool())
        .await
        .expect("organization cleanup must run");
}
