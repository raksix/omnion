//! Integration test for the content API tokens (REQ-019, slice 1).
//!
//! The slice is done when a created token **authenticates** and a revoked one is **refused**.
//! Everything else in this file exists to make that one sentence hard to satisfy by accident:
//!
//! * a token authenticates with the exact plaintext, and with nothing else — a wrong secret, a
//!   wrong prefix and a malformed shape are each refused before any row is consulted;
//! * a token that was **rotated** stops working *at once*, which is the whole reason rotation
//!   exists and the reason the old secret cannot be quietly tolerated for a grace period;
//! * a **revoked** token answers a different code from an **expired** one, because an integrator
//!   whose credential expired and one whose credential was stolen need different instructions;
//! * a token from **another organization** is refused as `invalid_token`, never as
//!   `forbidden` — a code that says "this exists but is not yours" is a cross-tenant oracle;
//! * the **list never carries a secret**, checked by scanning the serialized body for the
//!   plaintext rather than by asserting a field is absent (a field the serializer grows later is
//!   exactly how this leaks);
//! * a **duplicate name** is a 409 naming the field, because the unique index is on
//!   `lower(name)` and "Prod"/"prod" is the collision the person cannot see;
//! * the two powers are **provably separate**: an account with `content.api.read` sees the list
//!   and cannot mint, which is the claim the panel's tab visibility makes to a person.
//!
//! It runs against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason, so `cargo test`
//! stays usable on a machine without Docker.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// A read-only account: may see the token list, may not mint one.
const READER_PERMISSIONS: [&str; 1] = ["content.api.read"];

/// The curator: read plus manage. Deliberately a *set* rather than `All`, so a key that is
/// missing from the catalogue fails this suite instead of being granted by everything.
const CURATOR_EXTRA: [&str; 1] = ["content.api.manage"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
    /// The body as text, for "this string must not appear anywhere in the response" assertions.
    raw: String,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
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
    let raw = String::from_utf8_lossy(&bytes).to_string();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        set_cookie,
        body,
        raw,
    }
}

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&body).expect("body must serialize"),
            ))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({error}) — start it with \
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

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("token-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Token Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

async fn login(state: &AppState, email: &str) -> String {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": email, "password": PASSWORD })),
        ),
    )
    .await;
    assert!(
        response.status.is_success(),
        "login for {email} answered {}: {}",
        response.status,
        response.body
    );
    response
        .set_cookie
        .as_deref()
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("the cookie has a value")
        .split_once('=')
        .expect("the cookie is name=value")
        .1
        .to_owned()
}

async fn grant(db: &Db, organization_id: Uuid, user_id: Uuid, keys: &[&str], label: &str) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!(
                "{}-{}",
                label.to_lowercase().replace(' ', "-"),
                &Uuid::new_v4().simple().to_string()[..8]
            ),
            name: label.to_owned(),
            description: format!("{label} role"),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");
    let entries: Vec<RolePermissionInput> = keys
        .iter()
        .map(|key| RolePermissionInput {
            key: (*key).to_owned(),
            effect: Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(db.pool(), role.id, &entries)
        .await
        .expect("the role permission set must be written");
    let binding = NewBinding {
        role_id: role.id,
        user_id,
        scope: Scope::Organization { organization_id },
        granted_by: None,
        expires_at: None,
    };
    bindings::validate(db.pool(), &binding)
        .await
        .expect("the binding must validate");
    bindings::grant(db.pool(), binding)
        .await
        .expect("the binding must be granted");
}

struct Fixture {
    state: AppState,
    db: Db,
    org: Uuid,
    site: Uuid,
    reader_email: String,
    curator_email: String,
    /// An account in a *different* organization, to prove the cross-tenant refusal.
    outsider_email: String,
    outsider_org: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(org)
            .bind("Token Test Org")
            .bind(format!("tok-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let outsider_org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(outsider_org)
            .bind("Token Outsider Org")
            .bind(format!("out-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the outsider organization must be created");

        let site = Uuid::new_v4();
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(format!("tok{}", &Uuid::new_v4().simple().to_string()[..8]))
            .bind("Token Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        let (reader_id, reader_email) = create_account(&db, Some(org)).await;
        grant(
            &db,
            org,
            reader_id,
            &READER_PERMISSIONS.to_vec(),
            "Token Reader",
        )
        .await;

        let (curator_id, curator_email) = create_account(&db, Some(org)).await;
        let mut curator_keys = READER_PERMISSIONS.to_vec();
        curator_keys.extend_from_slice(&CURATOR_EXTRA);
        grant(&db, org, curator_id, &curator_keys, "Token Curator").await;

        // The outsider may manage tokens in their OWN organization, so the only thing standing
        // between them and this suite's token is the organization check.
        let (outsider_id, outsider_email) = create_account(&db, Some(outsider_org)).await;
        let mut outsider_keys = READER_PERMISSIONS.to_vec();
        outsider_keys.extend_from_slice(&CURATOR_EXTRA);
        grant(
            &db,
            outsider_org,
            outsider_id,
            &outsider_keys,
            "Outsider Curator",
        )
        .await;

        Some(Self {
            state,
            db,
            org,
            site,
            reader_email,
            curator_email,
            outsider_email,
            outsider_org,
        })
    }

    async fn reader(&self) -> String {
        login(&self.state, &self.reader_email).await
    }
    async fn curator(&self) -> String {
        login(&self.state, &self.curator_email).await
    }
    async fn outsider(&self) -> String {
        login(&self.state, &self.outsider_email).await
    }

    async fn create_token(
        &self,
        token: &str,
        name: &str,
        scopes: &[&str],
    ) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/content-api/tokens",
                Some(token),
                Some(json!({
                    "name": name,
                    "site_id": self.site,
                    "scopes": scopes,
                })),
            ),
        )
        .await
    }
}

/// The store's own verdict, called directly — the HTTP surface has no content read route until
/// slice 2, so authentication is proved against the store the route will call.
async fn authenticate(fixture: &Fixture, plaintext: &str) -> Result<(), String> {
    omnion_content::api_tokens::authenticate(fixture.db.pool(), fixture.org, plaintext)
        .await
        .map(|_| ())
        .map_err(|failure| failure.code().to_owned())
}

#[tokio::test]
async fn a_created_token_authenticates_and_a_revoked_one_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;

    let created = fixture
        .create_token(&token, "Frontend", &["content:read"])
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let plaintext = created.body["plaintext"]
        .as_str()
        .expect("the create response must carry the plaintext")
        .to_owned();
    assert!(
        plaintext.starts_with("omn_"),
        "a content token must be recognisable at a glance: {plaintext}"
    );
    assert_eq!(
        created.body["plaintext_shown_once"], true,
        "the client must be told from the payload that this is the only copy: {}",
        created.body
    );

    // The done line: it authenticates.
    assert_eq!(
        authenticate(&fixture, &plaintext).await,
        Ok(()),
        "a freshly created token must authenticate"
    );

    // …and a revoked one is refused, with its own code.
    let id = created.body["token"]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let revoked = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/content-api/tokens/{id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.body);

    assert_eq!(
        authenticate(&fixture, &plaintext).await,
        Err("token_revoked".to_owned()),
        "a revoked token must be refused, and named as revoked rather than as wrong"
    );

    // Revoking twice is the same answer: a double click on a confirm button is not a 404.
    let again = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/content-api/tokens/{id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::NO_CONTENT,
        "revocation must be idempotent: {}",
        again.body
    );
}

#[tokio::test]
async fn nothing_but_the_exact_plaintext_authenticates() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let created = fixture
        .create_token(&token, "Exact", &["content:read"])
        .await;
    let plaintext = created.body["plaintext"].as_str().expect("a plaintext");
    let (prefix, secret) = plaintext
        .split_once('_')
        .and_then(|(_, rest)| rest.split_once('_'))
        .expect("omn_<prefix>_<secret>");

    for wrong in [
        plaintext.to_string(),
        // Right shape, wrong secret: the row is found and the comparison refuses.
        format!("omn_{prefix}_{}", "0".repeat(32)),
        // A different token's prefix: the lookup misses entirely.
        format!("omn_{}_{secret}", "f".repeat(8)),
        // Malformed shapes never reach the database at all.
        "not-a-token".to_string(),
        format!("omn_{prefix}"),
        format!("{plaintext}extra"),
    ] {
        assert_eq!(
            authenticate(&fixture, &wrong).await,
            Err("invalid_token".to_owned()),
            "{wrong:?} must not authenticate"
        );
    }
    // The real one still does, after all that: a store that refuses everything is also green.
    assert_eq!(authenticate(&fixture, plaintext).await, Ok(()));
}

#[tokio::test]
async fn rotation_kills_the_previous_secret_immediately() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let created = fixture
        .create_token(&token, "Rotating", &["content:read"])
        .await;
    let original = created.body["plaintext"].as_str().expect("a plaintext");
    let id = created.body["token"]["id"].as_str().expect("an id").to_owned();
    assert_eq!(authenticate(&fixture, original).await, Ok(()));

    let rotated = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/content-api/tokens/{id}/rotate"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.body);
    let fresh = rotated.body["plaintext"].as_str().expect("a plaintext");

    assert_ne!(
        fresh, original,
        "rotation must issue a different secret, not re-issue the old one"
    );
    assert_eq!(
        authenticate(&fixture, fresh).await,
        Ok(()),
        "the new secret must work"
    );
    assert_eq!(
        authenticate(&fixture, original).await,
        Err("invalid_token".to_owned()),
        "the previous secret must stop working the moment the new one exists"
    );
    // Same row: a rotation is a new secret, not a new token.
    assert_eq!(
        rotated.body["token"]["id"].as_str(),
        Some(id.as_str()),
        "rotation must keep the token's identity: {}",
        rotated.body
    );
}

#[tokio::test]
async fn an_expired_token_says_so_rather_than_saying_it_is_wrong() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let created = fixture
        .create_token(&token, "Stale", &["content:read"])
        .await;
    let plaintext = created.body["plaintext"].as_str().expect("a plaintext");
    let id = created.body["token"]["id"].as_str().expect("an id").to_owned();
    assert_eq!(authenticate(&fixture, plaintext).await, Ok(()));

    // Expire it by writing the column directly: a 30-day preset cannot be waited for, and the
    // store's own `expires_at` comparison is what is under test, not the dialog.
    sqlx::query("update api_tokens set expires_at = now() - interval '1 minute' where id = $1")
        .bind(Uuid::parse_str(&id).expect("a uuid"))
        .execute(fixture.db.pool())
        .await
        .expect("the expiry must be backdated");

    assert_eq!(
        authenticate(&fixture, plaintext).await,
        Err("token_expired".to_owned()),
        "an expired token must be distinguishable from a wrong one"
    );

    // …and the panel's list agrees with the store, or the two disagree about the same row.
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/content-api/tokens",
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let row = listed
        .body
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["id"] == id.as_str())
        .expect("the expired token must still be listed");
    assert_eq!(
        row["status"], "expired",
        "the list must read the same state the store does: {row}"
    );
}

#[tokio::test]
async fn a_token_from_another_organization_is_invalid_and_never_forbidden() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let created = fixture
        .create_token(&token, "Private", &["content:read"])
        .await;
    let plaintext = created.body["plaintext"].as_str().expect("a plaintext");

    assert_eq!(authenticate(&fixture, plaintext).await, Ok(()));

    // The same secret, resolved against the *other* organization, is an unknown token. If this
    // ever answers `insufficient_scope` or `forbidden`, it has become a cross-tenant oracle: it
    // told the caller that this prefix exists.
    assert_eq!(
        omnion_content::api_tokens::authenticate(fixture.db.pool(), fixture.outsider_org, plaintext)
            .await
            .map_err(|failure| failure.code().to_owned()),
        Err("invalid_token".to_owned()),
        "another organization must not learn that this token exists"
    );

    // And the outsider's own panel answers with an empty list, not this suite's token.
    let outsider = fixture.outsider().await;
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/content-api/tokens",
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let theirs = listed.body.as_array().expect("an array");
    assert!(
        theirs.iter().all(|row| row["name"] != "Private"),
        "the list must be scoped to the caller's organization: {theirs:?}"
    );
}

#[tokio::test]
async fn the_list_never_carries_a_secret() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let created = fixture
        .create_token(&token, "Listed", &["content:read", "media:read"])
        .await;
    let plaintext = created.body["plaintext"].as_str().expect("a plaintext");
    let (_, secret) = plaintext
        .split_once('_')
        .and_then(|(_, rest)| rest.split_once('_'))
        .expect("omn_<prefix>_<secret>");

    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/content-api/tokens",
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    // Scanning the raw body, not asserting a field is absent: a serializer that grows a field
    // later is exactly how this leaks, and "the JSON has no `secret` key" would still pass.
    assert!(
        !listed.raw.contains(secret),
        "the list must not contain any part of the secret: {}",
        listed.raw
    );
    assert!(
        !listed.raw.contains("token_hash"),
        "the list must not expose the digest column: {}",
        listed.raw
    );
    // The prefix IS shown — that is the point of it.
    let row = listed.body[0]
        .as_object()
        .expect("a row object");
    assert!(
        row["prefix"].as_str().expect("a prefix").starts_with("omn_"),
        "the copyable prefix must be present: {row:?}"
    );
    assert_eq!(row["status"], "active", "{row:?}");
    assert_eq!(row["site_key"].as_str(), Some(
        sqlx::query_scalar::<_, String>("select key from sites where id = $1")
            .bind(fixture.site)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the site key")
            .as_str()
    ));
}

#[tokio::test]
async fn a_duplicate_name_is_a_conflict_naming_the_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let first = fixture
        .create_token(&token, "Production", &["content:read"])
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);

    let exact = fixture
        .create_token(&token, "Production", &["content:read"])
        .await;
    assert_eq!(
        exact.status,
        StatusCode::CONFLICT,
        "a duplicate name must be refused, not stored twice: {}",
        exact.body
    );
    assert_eq!(exact.body["error"]["code"], "name_taken", "{}", exact.body);
    assert_eq!(
        exact.body["error"]["details"]["field"], "name",
        "the refusal must name the field to change: {}",
        exact.body
    );

    // The collision the person cannot SEE is the case-insensitive one, and it is the one the
    // index on `lower(name)` exists for.
    let differently_cased = fixture
        .create_token(&token, "production", &["content:read"])
        .await;
    assert_eq!(
        differently_cased.status,
        StatusCode::CONFLICT,
        "\"Production\" and \"production\" are one name to the person reading the list: {}",
        differently_cased.body
    );
}

#[tokio::test]
async fn each_bad_field_is_refused_with_its_own_message() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;

    // An empty scope list.
    let no_scopes = fixture.create_token(&token, "No scopes", &[]).await;
    assert_eq!(
        no_scopes.status,
        StatusCode::BAD_REQUEST,
        "{}",
        no_scopes.body
    );
    assert_eq!(
        no_scopes.body["error"]["details"]["field"], "scopes",
        "{}",
        no_scopes.body
    );

    // An origin that is not an origin.
    let bad_origin = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/content-api/tokens",
            Some(&token),
            Some(json!({
                "name": "Bad origin",
                "scopes": ["content:read"],
                "allowed_origins": ["https://app.example.com/*"],
            })),
        ),
    )
    .await;
    assert_eq!(
        bad_origin.status,
        StatusCode::BAD_REQUEST,
        "a wildcard origin is not an origin: {}",
        bad_origin.body
    );

    // A rate limit off the two tiers.
    let bad_rate = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/content-api/tokens",
            Some(&token),
            Some(json!({
                "name": "Bad rate",
                "scopes": ["content:read"],
                "rate_limit_per_minute": 10_000,
            })),
        ),
    )
    .await;
    assert_eq!(
        bad_rate.status,
        StatusCode::BAD_REQUEST,
        "{}",
        bad_rate.body
    );

    // A valid origin alongside a bad one still fails: partial validation is how a token ends up
    // scoped to something nobody typed.
    let mixed = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/content-api/tokens",
            Some(&token),
            Some(json!({
                "name": "Mixed origins",
                "scopes": ["content:read"],
                "allowed_origins": ["https://ok.example.com", "not-an-origin"],
            })),
        ),
    )
    .await;
    assert_eq!(
        mixed.status,
        StatusCode::BAD_REQUEST,
        "one bad origin must refuse the whole list: {}",
        mixed.body
    );
}

#[tokio::test]
async fn reading_and_managing_tokens_are_separate_powers() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let reader = fixture.reader().await;

    // The reader sees the list…
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/content-api/tokens",
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);

    // …and the vocabulary the dialog needs, which is also a read.
    let vocabulary = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/content-api/tokens/vocabulary",
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(vocabulary.status, StatusCode::OK, "{}", vocabulary.body);
    let scopes = vocabulary.body["scopes"].as_array().expect("an array");
    assert_eq!(
        scopes.len(),
        2,
        "v1 offers the two read scopes: {scopes:?}"
    );
    assert_eq!(
        vocabulary.body["reserved_scopes"][0]["scope"], "content:write",
        "the write scope is reserved by name and must be visible as reserved: {}",
        vocabulary.body
    );

    // …and cannot mint. This is the claim the panel's tab visibility makes to a person.
    let refused = fixture
        .create_token(&reader, "Reader tried", &["content:read"])
        .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "`content.api.read` must not be enough to mint a token: {}",
        refused.body
    );

    // An anonymous caller gets nothing at all, and not a 403 — an unauthenticated request that
    // answers 403 tells a prober that the route exists.
    let anonymous = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/content-api/tokens",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        anonymous.body
    );
}

#[tokio::test]
async fn a_token_cannot_be_scoped_to_another_organizations_site() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let outsider = fixture.outsider().await;

    let other_site = Uuid::new_v4();
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(other_site)
        .bind(fixture.org)
        .bind(format!("oth{}", &Uuid::new_v4().simple().to_string()[..8]))
        .bind("Foreign Site")
        .execute(fixture.db.pool())
        .await
        .expect("the foreign site must exist");

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/content-api/tokens",
            Some(&outsider),
            Some(json!({
                "name": "Trespassing",
                "site_id": other_site,
                "scopes": ["content:read"],
            })),
        ),
    )
    .await;
    assert!(
        response.status.is_client_error(),
        "a token may not be scoped to a site of another organization: {}",
        response.status
    );
    // The row must not exist, not merely be invisible: a token written and not shown is a token
    // that still works.
    let rows = sqlx::query_scalar::<_, i64>(
        "select count(*) from api_tokens where name = 'Trespassing'",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must read");
    assert_eq!(rows, 0, "a refused token must leave no row behind");
}

#[tokio::test]
async fn a_reserved_write_scope_is_stored_but_not_honoured() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    // `content:write` is reserved by name in v1. The store accepts it so the future write surface
    // needs no migration — and the *route* is what refuses to act on it. What this suite pins is
    // that storing it is not the same as granting it: the scope must come back as stored, and
    // no read scope may be implied by it.
    let created = fixture
        .create_token(&token, "Writer", &["content:write"])
        .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "a reserved scope must be storable: {}",
        created.body
    );
    let scopes = created.body["token"]["scopes"].as_array().expect("an array");
    assert_eq!(scopes, &vec![json!("content:write")], "{scopes:?}");
    assert!(
        !scopes.iter().any(|scope| scope == "content:read"),
        "a write scope must not imply a read one: {scopes:?}"
    );
}
