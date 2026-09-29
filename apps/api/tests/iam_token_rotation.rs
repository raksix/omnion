//! Integration walk for provisioning-token rotation and expiry (REQ-065, slice 4 part 4).
//!
//! The store's module doc has claimed "tokens are rotatable" since the table was written, and the
//! only thing a caller could actually do was revoke — which is not rotation. This walk is the
//! evidence that the claim is now true, and it is built around the sentences the claim implies:
//!
//! * **A rotation is a chain, not an anonymous delete.** The old row survives, revoked and
//!   pointing at its successor. A reader who finds a leaked secret can walk *forward* to the live
//!   token; without the link, every rotation is indistinguishable from a revoke in the evidence.
//! * **The old secret stops working the moment the rotation returns.** Not "after a refresh",
//!   not "after the connector is reconfigured" — the next request with the old value is refused,
//!   which is the acceptance criterion's own sentence.
//! * **The new secret is returned once, like a minted one.** A rotation that handed back nothing
//!   would force a second mint and leave an orphan token behind.
//! * **A token cannot be minted with no expiry.** The spec asks for "expiry required"; a table
//!   column nobody enforces is a comment, and a token that never expires is the one credential
//!   whose compromise is silent — no user, no password, no login to notice.
//! * **Rotating a dead token is refused by name, not silently a second success.** Idempotence is
//!   right for revoke (a retry learns nothing new) and wrong for rotate (there is no single
//!   answer to "replace this" once it has been replaced, and guessing strands a live connector).
//! * **Cross-tenant rotation is 404**, for the same reason a cross-tenant read is: an id is not
//!   a secret, and a 403 tells a stranger the token exists.
//!
//! Tokens are minted and rotated through the real store, and every call goes through the real
//! router, so the walk proves the surface rather than a fixture that could be wrong twice.

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

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
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
    TestResponse {
        status,
        set_cookie,
        body: if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        },
    }
}

fn request(
    method: Method,
    uri: &str,
    session: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(credential) = session {
        builder = builder.header(header::COOKIE, credential);
    }
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
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
                "SKIP: PostgreSQL is not reachable ({error}) — the token-rotation walk needs a \
                 database with every migration applied"
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

struct Fixture {
    state: AppState,
    db: Db,
    organization_id: Uuid,
    session: String,
    foreign_session: String,
    foreign_organization: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let mut organization_id = Uuid::nil();
        let mut foreign_organization = Uuid::nil();
        let mut session = String::new();
        let mut foreign_session = String::new();

        for index in 0..2 {
            let slug = format!("token-rotate-{index}-{}", Uuid::new_v4().simple());
            let org: Uuid = sqlx::query_scalar(
                "insert into organizations (name, slug) values ($1, $2) returning id",
            )
            .bind("Token Rotation Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the organization must be created");

            let email = format!("token-owner-{index}-{}@omnion.test", Uuid::new_v4().simple());
            let owner = users::create_user(
                db.pool(),
                NewUser {
                    email: email.clone(),
                    password: PASSWORD.to_owned(),
                    display_name: "Token Rotation Test Owner".to_owned(),
                    organization_id: Some(org),
                },
            )
            .await
            .expect("the owner must be created");
            seed::bind_owner(db.pool(), owner.id)
                .await
                .expect("the owner binding must be created");

            let login = call(
                &state,
                request(
                    Method::POST,
                    "/api/v1/auth/login",
                    None,
                    Some(json!({ "email": email, "password": PASSWORD })),
                ),
            )
            .await;
            assert_eq!(login.status, StatusCode::OK, "login: {}", login.body);
            let cookie = format!(
                "omnion_session={}",
                login
                    .set_cookie
                    .as_deref()
                    .expect("login must set the session cookie")
                    .split(';')
                    .next()
                    .expect("the cookie has a value")
                    .split_once('=')
                    .expect("the cookie is name=value")
                    .1
            );

            if index == 0 {
                organization_id = org;
                session = cookie;
            } else {
                foreign_organization = org;
                foreign_session = cookie;
            }
        }

        Some(Self {
            state,
            db,
            organization_id,
            session,
            foreign_session,
            foreign_organization,
        })
    }

    async fn mint(&self, session: &str, name: &str, days: Option<i64>) -> TestResponse {
        // The organization comes from the session, not from the body: an account may only work
        // inside its own organization, so sending an explicit `organization_id` that disagrees
        // with the cookie is itself a refusal. Asking for the body field would test the scope
        // guard rather than the thing this walk is about.
        let mut body = json!({ "name": name });
        if let Some(days) = days {
            body["expires_in_days"] = json!(days);
        }
        call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/iam/provisioning/tokens",
                Some(session),
                Some(body),
            ),
        )
        .await
    }

    async fn rotate(&self, token_id: Uuid, session: &str) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/iam/provisioning/tokens/{token_id}"),
                Some(session),
                Some(json!({})),
            ),
        )
        .await
    }

    /// Present a secret to the SCIM surface exactly as a connector would.
    ///
    /// The status code is the answer, not the body: a live token gets a `200` list (whose
    /// `totalResults` depends on how many users the fixture organization happens to have — it is
    /// not a fixed number and asserting one would be asserting the fixture, not the token), and a
    /// dead one gets a `401` SCIM error document.
    async fn scim_status(&self, secret: &str) -> (StatusCode, Value) {
        let response = call(
            &self.state,
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/scim/v2/Users")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .expect("the SCIM request must build"),
        )
        .await;
        (response.status, response.body)
    }

    /// The one sentence this walk exists to prove, in the form an operator observes it.
    async fn scim_accepted(&self, secret: &str) -> bool {
        self.scim_status(secret).await.0 == StatusCode::OK
    }

    fn token_id(body: &Value) -> Uuid {
        Uuid::parse_str(body["token"]["id"].as_str().expect("a token id")).expect("a uuid")
    }

    fn secret(body: &Value) -> String {
        body["secret"]
            .as_str()
            .expect("a secret is returned exactly once")
            .to_owned()
    }
}

/// The whole slice, in the order the sentences depend on each other.
#[tokio::test]
async fn a_rotated_token_refuses_the_old_value_and_the_chain_survives() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let stamp = Uuid::new_v4().simple().to_string();

    // ---- A minted token carries an expiry, because "expiry required" has to be enforced -------
    let minted = fixture
        .mint(&fixture.session.clone(), &format!("Okta {stamp}"), None)
        .await;
    assert_eq!(minted.status, StatusCode::CREATED, "mint: {}", minted.body);
    assert!(
        minted.body["token"]["expires_at"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "a minted token must carry an expiry: {}",
        minted.body
    );
    assert_eq!(minted.body["token"]["expired"], json!(false));
    assert_eq!(minted.body["token"]["rotated"], json!(false));

    let old_id = Fixture::token_id(&minted.body);
    let old_secret = Fixture::secret(&minted.body);

    // The minted secret really works before anything is done to it — otherwise "the old value is
    // refused" below would pass against a token that never worked in the first place.
    assert!(
        fixture.scim_accepted(&old_secret).await,
        "a fresh token must reach the SCIM surface"
    );

    // ---- The secret is shown once, and the list afterwards shows only the prefix -------------
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/provisioning/tokens?organization_id={}",
                fixture.organization_id
            ),
            Some(&fixture.session.clone()),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "list: {}", listed.body);
    let row = listed.body["tokens"]
        .as_array()
        .expect("an array of tokens")
        .iter()
        .find(|entry| entry["id"] == json!(old_id.to_string()))
        .expect("the minted token is listed")
        .clone();
    assert_eq!(
        row["prefix"], minted.body["token"]["prefix"],
        "the list shows the public prefix"
    );
    assert!(
        row.get("secret").is_none(),
        "the list must never carry a secret: {row}"
    );
    assert!(
        !listed.body.to_string().contains(&old_secret),
        "the minted secret must not appear anywhere in the list response"
    );

    // ---- Rotation returns a new secret and refuses the old one, immediately ------------------
    let rotated = fixture.rotate(old_id, &fixture.session.clone()).await;
    assert_eq!(rotated.status, StatusCode::OK, "rotate: {}", rotated.body);
    let new_secret = Fixture::secret(&rotated.body);
    assert_ne!(
        new_secret, old_secret,
        "a rotation that hands back the same secret has rotated nothing"
    );
    let new_id = Fixture::token_id(&rotated.body);
    assert_ne!(new_id, old_id, "the successor must be a new row");

    // THE acceptance sentence, asserted against the real endpoint and not against a flag.
    let (refused_status, refused_body) = fixture.scim_status(&old_secret).await;
    assert_eq!(
        refused_status,
        StatusCode::UNAUTHORIZED,
        "the old secret must be refused the moment the rotation returns: {refused_body}"
    );
    assert!(
        refused_body["detail"]
            .as_str()
            .is_some_and(|detail| !detail.is_empty()),
        "the refusal must carry a SCIM detail line, not a bare status: {refused_body}"
    );
    assert!(
        fixture.scim_accepted(&new_secret).await,
        "the rotated secret must work"
    );

    // ---- The chain survives: the old row is kept, revoked, and linked to its successor -------
    let replacement_link: Option<(Uuid, Uuid)> = sqlx::query_as(
        "select rotated_to, rotated_by from provisioning_token_rotations where token_id = $1",
    )
    .bind(old_id)
    .fetch_optional(fixture.db.pool())
    .await
    .expect("the rotation link must read");
    let (linked_to, rotated_by) =
        replacement_link.expect("a rotation must leave a link from the old token");
    assert_eq!(linked_to, new_id, "the link points at the successor");
    assert!(
        !rotated_by.is_nil(),
        "the link records who rotated it"
    );

    let old_row: (Option<time::OffsetDateTime>, Option<time::OffsetDateTime>) = sqlx::query_as(
        "select revoked_at, rotated_at from provisioning_tokens where id = $1",
    )
    .bind(old_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the replaced token must still exist — a rotation that deletes it is a revoke");
    assert!(
        old_row.0.is_some() && old_row.1.is_some(),
        "the replaced token is revoked and marked rotated"
    );

    // The response names the replacement, so the panel can render "replaced by" without a lookup.
    assert_eq!(rotated.body["replaced"]["id"], json!(old_id.to_string()));
    assert_eq!(rotated.body["replaced"]["rotated"], json!(true));
    assert_eq!(rotated.body["token"]["name"], json!(format!("Okta {stamp}")));
    assert_eq!(
        rotated.body["token"]["rotated"],
        json!(false),
        "the successor is not itself rotated"
    );

    // ---- Rotating a dead token is refused by name, not a second silent success ---------------
    let again = fixture.rotate(old_id, &fixture.session.clone()).await;
    assert_eq!(
        again.status,
        StatusCode::CONFLICT,
        "rotating an already-replaced token must be refused: {}",
        again.body
    );
    assert_eq!(again.body["error"]["code"], json!("token_not_live"));

    // ---- The old value is still refused after the failed second rotation ---------------------
    // A refusal that quietly minted a third token would leave the list with two live secrets and
    // the operator believing the old one was the only problem.
    let live_after: i64 = sqlx::query_scalar(
        "select count(*) from provisioning_tokens where organization_id = $1 and revoked_at is null",
    )
    .bind(fixture.organization_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the live token count must read");
    assert_eq!(live_after, 1, "exactly one token may be live after a rotation");

    // ---- Expiry is real: a token past its expiry is refused ---------------------------------
    let short = fixture
        .mint(&fixture.session.clone(), &format!("Short {stamp}"), Some(1))
        .await;
    assert_eq!(short.status, StatusCode::CREATED, "mint: {}", short.body);
    let short_secret = Fixture::secret(&short.body);
    assert!(
        fixture.scim_accepted(&short_secret).await,
        "a token inside its lifetime must work"
    );

    // Move the expiry just past the present, rather than back before the creation: the table
    // refuses `expires_at <= created_at` (a token cannot be born expired), so "make it expired"
    // has to be done the way a real expiry arrives — by time passing. Shortening the window to
    // one second and waiting for it keeps the assertion about the *check* rather than about a
    // backdated write the database correctly rejects.
    let short_id = Fixture::token_id(&short.body);
    sqlx::query(
        "update provisioning_tokens set expires_at = now() + interval '1 second' where id = $1",
    )
    .bind(short_id)
    .execute(fixture.db.pool())
    .await
    .expect("the expiry must be movable for the test");
    tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
    assert_eq!(
        fixture.scim_status(&short_secret).await.0,
        StatusCode::UNAUTHORIZED,
        "an expired token must be refused exactly as a revoked one is"
    );

    // The panel must be able to say so without the client comparing timestamps.
    let after_expiry = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/iam/provisioning/tokens?organization_id={}",
                fixture.organization_id
            ),
            Some(&fixture.session.clone()),
            None,
        ),
    )
    .await;
    let expired_row = after_expiry.body["tokens"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|entry| entry["id"] == json!(short_id.to_string()))
        .expect("the expired token is still listed — expiry is not a delete")
        .clone();
    assert_eq!(
        expired_row["expired"], json!(true),
        "the list must mark an expired token as expired: {expired_row}"
    );

    // ---- A zero-day token is refused where the mistake is made -------------------------------
    let zero = fixture
        .mint(&fixture.session.clone(), &format!("Zero {stamp}"), Some(0))
        .await;
    assert_eq!(
        zero.status,
        StatusCode::BAD_REQUEST,
        "a token that is born expired must be refused: {}",
        zero.body
    );

    // ---- A stranger's token is invisible, not rotatable -------------------------------------
    let foreign = fixture.mint(&fixture.foreign_session.clone(), &format!("Foreign {stamp}"), None).await;
    assert_eq!(foreign.status, StatusCode::CREATED, "mint: {}", foreign.body);
    let foreign_id = Fixture::token_id(&foreign.body);
    // Re-point the foreign token at this walk's organization is impossible from the outside, so
    // the cross-tenant read is asserted by asking for a token that is simply not ours.
    let stranger = fixture.rotate(foreign_id, &fixture.session.clone()).await;
    assert_eq!(
        stranger.status,
        StatusCode::NOT_FOUND,
        "another organization's token must be invisible, not rotatable: {}",
        stranger.body
    );
    // And the foreign token is untouched by the attempt.
    let still_live: bool = sqlx::query_scalar(
        "select revoked_at is null from provisioning_tokens where id = $1",
    )
    .bind(foreign_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the foreign token must still exist");
    assert!(still_live, "a refused cross-tenant rotation must change nothing");
    assert_ne!(fixture.foreign_organization, fixture.organization_id);
}
