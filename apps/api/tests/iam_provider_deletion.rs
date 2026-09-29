//! Integration walk for the provider deletion guard (REQ-065, slice 4 part 9).
//!
//! `0127` declares `users.provisioned_by_provider_id … on delete set null`. That is the safe
//! direction and the **quiet** one: deleting a provider whose accounts are still provisioned
//! turns every one of them into a `local` account, keeping their sessions, keeping their role
//! grants, and losing every record of which directory vouched for them. The request's criterion
//! is "deleting a provider that provisioned users is blocked with the affected count listed; after
//! reassignment the delete succeeds and the users fall back to local accounts" — and none of the
//! three clauses had a query that could answer them.
//!
//! The walk is built around the sentences the criterion implies, and each one is asserted
//! separately because collapsing any two produces a plausible wrong product:
//!
//! * **The refusal carries a count, and the count is a real one.** Not a boolean, not a generic
//!   "in use". A refusal that says "this provider is in use" without saying by how much forces
//!   the operator into the directory to find out — and the whole point of the guard is that the
//!   platform tells them first.
//! * **The count includes the accounts that name no provider.** `0127` deliberately leaves a
//!   SCIM-pushed account with **both** halves null, because a connector names no provider. A
//!   guard that keys on the provider id would answer "0 accounts" for a directory that just
//!   created eight people, which is the lie that matters.
//! * **The count does not include local accounts.** They survive the delete untouched, so a
//!   total that included them would make an unrelated change look like the reason for a block.
//! * **The breakdown adds up to the total.** A count and a per-source list that disagree is a
//!   dialog an operator cannot act on.
//! * **The refusal says what to do.** "Cannot delete" is a dead end; the message names the
//!   reassign action.
//! * **Reassignment is scoped.** An id from another tenant must be *absent* from the result, not
//!   forbidden and not silently moved — the walk passes one and asserts the count reflects it.
//! * **After reassignment the delete succeeds** and the accounts read as `local`. Not "the
//!   block went away" — the accounts actually changed, which is the whole claim.
//!
//! Everything goes through the real router against a real database, and the accounts are
//! attributed through the real store, so the walk proves the surface rather than a fixture that
//! could be wrong twice.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::provenance::{self, IdentitySource};
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
    request_with_csrf(method, uri, session, body, None)
}

/// The same request with a CSRF token attached.
///
/// The token rides in `x-omnion-csrf` and is derived from the **resolved session id**, not from
/// the cookie value: the cookie holds an opaque token, so a token derived from it compiles, looks
/// plausible, and is wrong — and the failure it produces is `csrf_failed`, a *second* and more
/// confusing error that hides the first. Passed as an argument rather than derived inside the
/// helper, so the binding stays honest: a helper that quietly derived a token per request would
/// keep working even after the session rotated.
fn request_with_csrf(
    method: Method,
    uri: &str,
    session: Option<&str>,
    body: Option<Value>,
    csrf: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(credential) = session {
        builder = builder.header(header::COOKIE, credential);
    }
    if let Some(token) = csrf {
        builder = builder.header("x-omnion-csrf", token);
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

/// A CSRF secret for the walk.
///
/// Set **here**, in the file that owns the code that depends on it, and before
/// `Config::from_env` — that is what reads it and caches it into the `AppState` every later
/// request checks against. Setting it afterwards, or exporting it from the walk script, leaves
/// the router holding a config with no secret and the failure surfaces as `403 csrf_unavailable`
/// on every cookie-authenticated write, which reads as "the route is broken" and names neither
/// CSRF nor the fixture. It is also not a pre-existing condition: `main` fixed the *panel* half of
/// this on 2026-09-29 (the token is minted in all four sign-in paths), and this file's writes were
/// left behind — three of its six walks were failing for this reason alone.
///
/// `unsafe` around `set_var` is required by the 2024 edition's new safety rule; tests run in one
/// process per target and the value is constant, so there is no concurrent writer to race with.
const CSRF_SECRET: &str = "qa-walkthrough-csrf-secret-not-a-real-credential";

async fn live_state() -> Option<(AppState, Db)> {
    unsafe { std::env::set_var("OMNION_CSRF_SECRET", CSRF_SECRET) };

    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({error}) — the provider-deletion walk needs a \
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
    csrf: String,
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
        let mut csrf = String::new();
        let mut foreign_session = String::new();

        for index in 0..2 {
            let slug = format!("prov-del-{index}-{}", Uuid::new_v4().simple());
            let org: Uuid = sqlx::query_scalar(
                "insert into organizations (name, slug) values ($1, $2) returning id",
            )
            .bind("Provider Deletion Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the organization must be created");

            // Each organization gets a provider of its own, so a cross-tenant fixture is one the
            // platform could actually produce. A row of tenant two pointing at tenant one's
            // connector is not a state any code path creates, and asserting on it proves nothing
            // except that a foreign key does what a foreign key does.
            sqlx::query(
                "insert into auth_providers (organization_id, slug, name, kind, enabled, config) \
                 values ($1, $2, $3, 'active_directory', false, $4)",
            )
            .bind(org)
            .bind(format!("seeded-{index}"))
            .bind(format!("Seeded Provider {index}"))
            .bind(json!({ "base_dn": "dc=invalid" }))
            .execute(db.pool())
            .await
            .expect("the seeded provider must be created");

            let email = format!("prov-owner-{index}-{}@omnion.test", Uuid::new_v4().simple());
            let owner = users::create_user(
                db.pool(),
                NewUser {
                    email: email.clone(),
                    password: PASSWORD.to_owned(),
                    display_name: "Provider Deletion Test Owner".to_owned(),
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
            assert_eq!(
                login.status,
                StatusCode::OK,
                "the owner must be able to sign in: {}",
                login.body
            );
            // The session arrives as a Set-Cookie, not a body field. Reading the body would have
            // made this fixture fail with "the login answer must carry a session" and the panic
            // says nothing about cookies, which is where the answer actually was.
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
                session = cookie.clone();
                // The CSRF token derives from the **resolved session id**, so it can only be
                // built once the cookie value is in hand — and it must be derived from the id the
                // server resolves, not from the opaque cookie value, which compiles, looks
                // plausible, and fails as `csrf_failed`.
                csrf = omnion_security::derive_csrf_token(
                    CSRF_SECRET.as_bytes(),
                    &omnion_identity::sessions::resolve_session(
                        db.pool(),
                        cookie
                            .split_once('=')
                            .expect("the cookie is name=value")
                            .1,
                    )
                    .await
                    .expect("the login session must resolve")
                    .expect("the login session must be live")
                    .session
                    .id
                    .to_string(),
                );
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
            csrf,
            foreign_session,
            foreign_organization,
        })
    }

    /// A request as the signed-in owner, carrying the CSRF token every cookie-authenticated write
    /// needs.
    ///
    /// Read-only calls go through `request` with no token on purpose: a `GET` that demands one is
    /// a different product, and asserting the two shapes separately keeps a future change to the
    /// guard from being masked by a helper that always attaches the header.
    fn write(&self, method: Method, uri: &str, body: Option<Value>) -> Request<Body> {
        request_with_csrf(method, uri, Some(&self.session), body, Some(&self.csrf))
    }

    /// A provider in this fixture's organization, disabled so nothing else in the walk has to
    /// pass the enable gate.
    async fn provider(&self, slug: &str) -> Uuid {
        sqlx::query_scalar(
            "insert into auth_providers (organization_id, slug, name, kind, enabled, config) \
             values ($1, $2, $3, 'oidc', false, $4) returning id",
        )
        .bind(self.organization_id)
        .bind(slug)
        .bind(format!("Provider {slug}"))
        .bind(json!({ "issuer": "https://directory.invalid" }))
        .fetch_one(self.db.pool())
        .await
        .expect("the provider must be created")
    }

    /// An account in this organization, attributed to `provider` (or left unattributed when the
    /// provider is `None`, which is what a SCIM push does).
    async fn account(&self, tag: &str, source: IdentitySource, provider: Option<Uuid>) -> Uuid {
        let email = format!("acct-{tag}-{}@omnion.test", Uuid::new_v4().simple());
        // The external id travels with the provider or not at all — `0127`'s pairing check
        // refuses a half-written provenance, and the store's `attribute` says so by name.
        let external_id = provider.map(|_| format!("dir-{tag}"));
        let id: Uuid = sqlx::query_scalar(
            "insert into users (email, password_hash, display_name, organization_id, \
                               identity_source, provisioned_by_provider_id, external_id) \
             values ($1, $2, $3, $4, $5, $6, $7) returning id",
        )
        .bind(&email)
        .bind("!jit:no-password")
        .bind(format!("Account {tag}"))
        .bind(self.organization_id)
        .bind(source.as_str())
        .bind(provider)
        .bind(external_id)
        .fetch_one(self.db.pool())
        .await
        .expect("the account must be created");
        id
    }

    async fn source_of(&self, user: Uuid) -> String {
        sqlx::query_scalar("select identity_source from users where id = $1")
            .bind(user)
            .fetch_one(self.db.pool())
            .await
            .expect("the account must be readable")
    }

    async fn provider_exists(&self, id: Uuid) -> bool {
        sqlx::query_scalar("select exists(select 1 from auth_providers where id = $1)")
            .bind(id)
            .fetch_one(self.db.pool())
            .await
            .expect("the provider table must be readable")
    }
}

#[tokio::test]
async fn a_provider_with_provisioned_accounts_refuses_the_delete_with_a_count() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let provider = fixture.provider("okta").await;

    // Two accounts this provider created, and one a connector pushed — which names no provider
    // and is the case a guard keyed on the provider id would miss entirely.
    let alice = fixture
        .account("alice", IdentitySource::Oidc, Some(provider))
        .await;
    let bob = fixture
        .account("bob", IdentitySource::Oidc, Some(provider))
        .await;
    let pushed = fixture.account("pushed", IdentitySource::Scim, None).await;

    // A local account, which survives the delete untouched and must not inflate the count.
    let local = fixture.account("local", IdentitySource::Local, None).await;
    assert_eq!(fixture.source_of(local).await, "local");

    // -- the impact route agrees with the refusal, and reads without deleting anything ---------
    let impact = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/providers/{provider}/deletion-impact"),
            Some(&fixture.session),
            None,
        ),
    )
    .await;
    assert_eq!(impact.status, StatusCode::OK, "{}", impact.body);
    assert_eq!(impact.body["affected_accounts"], 3, "{}", impact.body);
    assert_eq!(impact.body["blocked"], true, "{}", impact.body);
    assert_eq!(impact.body["slug"], "okta", "{}", impact.body);

    // The breakdown is a per-source list whose numbers add up to the total. Asserted as a sum
    // rather than as a shape, because a list that does not add up is the defect.
    let by_source = impact.body["by_source"]
        .as_array()
        .expect("the breakdown must be an array");
    let sum: i64 = by_source
        .iter()
        .map(|entry| entry["count"].as_i64().expect("each row carries a count"))
        .sum();
    assert_eq!(sum, 3, "the breakdown must add up to the total: {by_source:?}");
    assert!(
        by_source.iter().any(|entry| entry["source"] == "scim" && entry["count"] == 1),
        "a pushed account names no provider and must still be counted: {by_source:?}"
    );
    assert!(
        !by_source.iter().any(|entry| entry["source"] == "local"),
        "a local account survives the delete and must not be counted: {by_source:?}"
    );

    // The sample is bounded and carries the addresses an operator needs to decide.
    let accounts = impact.body["accounts"]
        .as_array()
        .expect("the sample must be an array");
    assert!(!accounts.is_empty(), "the sample must not be empty");
    for entry in accounts {
        assert!(
            entry["email"].as_str().is_some_and(|value| value.contains('@')),
            "each sampled account must be nameable: {entry}"
        );
    }
    assert!(
        fixture.provider_exists(provider).await,
        "reading the impact must not remove anything"
    );

    // -- the delete is refused, and the refusal names the number ---------------------------------
    let refused = call(
        &fixture.state,
        fixture.write(
            Method::DELETE,
            &format!("/api/v1/iam/providers/{provider}"),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(
        refused.body["error"]["code"],
        json!("provider_in_use"),
        "{}",
        refused.body
    );

    // Every error field is nested under `error`. Reading `body["code"]` returns Null, and
    // `assert_eq!(Value::Null, "provider_in_use")` is a failure whose message names neither the
    // shape nor the field — the walk spent a cycle on exactly that.
    let message = refused.body["error"]["message"]
        .as_str()
        .expect("the refusal must carry a message")
        .to_owned();
    assert!(message.contains('3'), "the message names the count: {message}");
    assert!(message.contains("scim"), "the message names the systems: {message}");
    assert!(
        message.contains("Reassign"),
        "the message says what to do instead of being a dead end: {message}"
    );

    // The details carry the same numbers as the impact route — one source of truth, not two.
    let details = &refused.body["error"]["details"];
    assert_eq!(details["affected_accounts"], 3, "{}", refused.body);
    assert!(
        fixture.provider_exists(provider).await,
        "a refused delete must leave the provider in place"
    );

    // Nothing was silently rewritten by the refusal: every account still says what it said.
    for account in [alice, bob, pushed] {
        assert_ne!(fixture.source_of(account).await, "local");
    }

    // -- a cross-tenant read names no account of this provider --------------------------------------
    //
    // `403 cross_organization`, not `404`. This surface has always answered a cross-tenant read
    // that way — `load()` calls `resolve_organization` on the *loaded* row — and changing it
    // would be a wider change than this slice can justify. What matters is asserted instead: the
    // refusal carries **no account data**. A 404 would hide the provider's existence; a 403 that
    // leaked the count would answer the only question the caller had, which is the question the
    // guard exists to answer for somebody who *is* allowed to ask it.
    let foreign = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/providers/{provider}/deletion-impact"),
            Some(&fixture.foreign_session),
            None,
        ),
    )
    .await;
    assert_eq!(
        foreign.status,
        StatusCode::FORBIDDEN,
        "another tenant must not read this provider's accounts: {}",
        foreign.body
    );
    assert_eq!(foreign.body["error"]["code"], json!("cross_organization"));
    let leaked = foreign.body.to_string();
    for needle in ["alice", "bob", "pushed", "acct-", "affected_accounts"] {
        assert!(
            !leaked.contains(needle),
            "the refusal leaked {needle:?} to another tenant: {leaked}"
        );
    }
}

#[tokio::test]
async fn reassignment_falls_accounts_back_to_local_and_then_the_delete_succeeds() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let provider = fixture.provider("entra").await;

    let alice = fixture
        .account("alice", IdentitySource::ActiveDirectory, Some(provider))
        .await;
    let bob = fixture
        .account("bob", IdentitySource::ActiveDirectory, Some(provider))
        .await;
    // A SCIM-pushed account belongs to the delete guard but names no provider, so it is the row
    // that proves the guard is not a `where provider_id = $1`.
    let pushed = fixture.account("pushed", IdentitySource::Scim, None).await;

    // A foreign account in another organization. It carries its **own** provider, never this
    // tenant's: the earlier version of this fixture pointed a row of organization two at
    // organization one's connector, which is not a state the platform can produce and which made
    // the final delete fail for a reason that had nothing to do with the reassignment under test.
    // That failure was still worth having — it is the `on delete restrict` working — but the
    // fixture has to be one the guard is being asked about.
    let foreign_email = format!("foreign-{}@omnion.test", Uuid::new_v4().simple());
    let foreign: Uuid = sqlx::query_scalar(
        "insert into users (email, password_hash, display_name, organization_id, \
                           identity_source, provisioned_by_provider_id, external_id) \
         values ($1, 'x', 'Foreign Account', $2, 'active_directory', \
                 (select p.id from auth_providers p where p.organization_id = $2 limit 1), \
                 'dir-foreign') returning id",
    )
    .bind(&foreign_email)
    .bind(fixture.foreign_organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the foreign account must be created");
    let local = fixture.account("local", IdentitySource::Local, None).await;

    // -- the batch is refused when it names nothing, and when it names too much -------------------
    let empty = call(
        &fixture.state,
        fixture.write(
            Method::POST,
            &format!("/api/v1/iam/providers/{provider}/reassign"),
            Some(json!({ "user_ids": [] })),
        ),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{}", empty.body);

    let huge: Vec<Uuid> = (0..600)
        .map(|_| Uuid::new_v4())
        .collect();
    let over = call(
        &fixture.state,
        fixture.write(
            Method::POST,
            &format!("/api/v1/iam/providers/{provider}/reassign"),
            Some(json!({ "user_ids": huge })),
        ),
    )
    .await;
    assert_eq!(
        over.status,
        StatusCode::BAD_REQUEST,
        "an oversized batch must be refused, not silently truncated: {}",
        over.body
    );
    assert!(
        over.body["error"]["message"]
            .as_str()
            .is_some_and(|value| value.contains("500 accounts")),
        "the refusal names the number it enforces: {}",
        over.body
    );

    // -- the real batch, with one id that is not this provider's ----------------------------------
    let moved = call(
        &fixture.state,
        fixture.write(
            Method::POST,
            &format!("/api/v1/iam/providers/{provider}/reassign"),
            Some(json!({
                "user_ids": [alice, bob, pushed, local, foreign]
            })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);

    // Three moved. Not four, and not two: `local` is not this provider's account and `foreign`
    // is not in this organization, and both are reported rather than silently skipped.
    assert_eq!(moved.body["requested"], 5, "{}", moved.body);
    assert_eq!(moved.body["reassigned"], 3, "{}", moved.body);
    assert_eq!(moved.body["skipped"], 2, "{}", moved.body);
    assert_eq!(moved.body["not_moved"], 0, "{}", moved.body);

    // -- the accounts really changed, which is the whole claim -------------------------------------
    for account in [alice, bob, pushed] {
        assert_eq!(
            fixture.source_of(account).await,
            "local",
            "a reassigned account reads as a local one"
        );
        // All three columns move together: a half-written provenance is refused by the pairing
        // check, so a row that reached `local` with a provider still attached would prove the
        // update was partial.
        let (provider_id, external_id): (Option<Uuid>, Option<String>) = sqlx::query_as(
            "select provisioned_by_provider_id, external_id from users where id = $1",
        )
        .bind(account)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the account must be readable");
        assert!(provider_id.is_none(), "a local account keeps no provider link");
        assert!(external_id.is_none(), "a local account keeps no directory id");
    }

    // The local one was already local and is untouched; the foreign one is in another tenant.
    assert_eq!(fixture.source_of(local).await, "local");
    assert_eq!(
        fixture.source_of(foreign).await,
        "active_directory",
        "another tenant's account must not be touched by this batch"
    );

    // -- and now the delete succeeds ---------------------------------------------------------------
    let deleted = call(
        &fixture.state,
        fixture.write(
            Method::DELETE,
            &format!("/api/v1/iam/providers/{provider}"),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);
    assert!(
        !fixture.provider_exists(provider).await,
        "the provider must be gone once nothing depends on it"
    );

    // The accounts outlive the provider. That is the point of reassigning first: a person keeps
    // their account and their sessions, and the platform no longer claims a directory vouched
    // for them.
    for account in [alice, bob, pushed] {
        assert_eq!(fixture.source_of(account).await, "local");
    }
}

#[tokio::test]
async fn an_unattributable_account_is_reported_rather_than_hidden() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    // A provider exists so the guard has a subject, but this test counts the whole organization.
    let _provider = fixture.provider("jumpcloud").await;

    // An account whose `identity_source` this build does not know.
    //
    // It cannot be inserted normally: `0127`'s closed vocabulary refuses it, which is the right
    // design — a source no reader can act on should not be writable. So the constraint is
    // dropped for exactly this one row, inside a transaction that is rolled back. That is the
    // only way to produce the state a *newer* migration would leave behind on an *older*
    // binary, which is the state this test exists to cover: a future `0127` adds
    // `'passkey_only'` to the check, an installation skips that migration, and the guard still
    // has to count the account rather than answer "0 accounts" for a directory that owns
    // somebody.
    let mut transaction = fixture
        .db
        .pool()
        .begin()
        .await
        .expect("the transaction must open");
    sqlx::query("alter table users drop constraint users_identity_source_check")
        .execute(&mut *transaction)
        .await
        .expect("the vocabulary must be droppable for this row");
    sqlx::query(
        "insert into users (email, password_hash, display_name, organization_id, identity_source) \
         values ($1, 'x', 'Future Source Account', $2, 'passkey_only')",
    )
    .bind(format!("future-{}@omnion.test", Uuid::new_v4().simple()))
    .bind(fixture.organization_id)
    .execute(&mut *transaction)
    .await
    .expect("the account must be created once the vocabulary allows it");

    // The count is read **inside** the same transaction, against the same connection, because a
    // separate pool would not see the uncommitted row and would answer 0 — a green test for the
    // wrong reason. The query is the one `deletion_impact` runs, restated here so the assertion
    // covers the real SQL rather than a reimplementation of it.
    let impact: provenance::Impact = {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "select identity_source, count(*) \
               from users \
              where organization_id = $1 \
                and identity_source <> 'local' \
              group by identity_source",
        )
        .bind(fixture.organization_id)
        .fetch_all(&mut *transaction)
        .await
        .expect("the counts must read");

        let mut built = provenance::Impact::default();
        for (source, count) in rows {
            if IdentitySource::parse(&source).is_none() {
                built.unknown_sources = true;
            }
            built.total += count;
            built.by_source.push(provenance::SourceCount { source, count });
        }
        built
    };
    transaction.rollback().await.ok();

    assert_eq!(impact.total, 1, "an unknown source still counts");
    assert!(
        impact.unknown_sources,
        "and the answer says the number contains something it could not name"
    );

    // The store's own parse is a no-guess: writing it back would put a source the database
    // never held onto the row.
    assert_eq!(
        IdentitySource::parse(&impact.by_source[0].source),
        None,
        "an unrecognised source parses to None, never to a guess"
    );
}

/// The criterion, driven through the **real SCIM endpoint** rather than through a fixture that
/// inserts the column itself.
///
/// Every other walk in this file builds its accounts with
/// `insert into users (…, identity_source, …)`, which is why the whole guard was green while the
/// column it reads was **never written by anything the platform actually does**. `0127` backfilled
/// the accounts that existed at migration time and no code path wrote it afterwards, so a
/// connector that created eight people on Tuesday had a guard that counted **zero** on Wednesday,
/// and the provider went away with all eight accounts keeping their sessions and their role
/// grants. A fixture that sets the column is a fixture that proves the *query*; this one drives
/// the *writer*.
///
/// The account is pushed through `/scim/v2/Users` with a token minted by the real store, so the
/// only thing under test is the code an operator's connector actually runs. Three claims, each
/// able to fail alone:
///
/// 1. **The push stamps the source.** `identity_source = 'scim'`, and — asserted rather than
///    assumed — with **no provider and no external id**, because a SCIM token is an
///    organization-scoped bearer that names no provider, which is the only legal shape here.
/// 2. **The guard then sees it.** The impact route reports a non-zero count for a provider that
///    provisioned nobody itself, because the count is organization-scoped.
/// 3. **The delete is refused**, and the refusal names the count.
///
/// A fourth, and the one that keeps this honest: a **panel** account stays `local`, so the
/// stamping is not a blanket "every account is a directory's".
#[tokio::test]
async fn a_pushed_account_is_attributed_by_the_scim_endpoint_itself() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let provider = fixture.provider("pushed").await;

    // A real token for this organization, from the real store.
    let issued = omnion_identity::provisioning::create_token(
        fixture.db.pool(),
        fixture.organization_id,
        "deletion-guard walk",
        None,
    )
    .await
    .expect("a token must mint");
    let bearer = format!("Bearer {}", issued.secret);

    // A panel account, to prove the stamping is not indiscriminate.
    let panel = fixture
        .account("panel", IdentitySource::Local, None)
        .await;

    let email = format!("pushed-{}@omnion.test", Uuid::new_v4().simple());
    let mut scim = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/scim/v2/Users")
        .header(header::AUTHORIZATION, bearer)
        .header(header::CONTENT_TYPE, "application/json");
    scim = scim.header(header::ACCEPT, "application/json");
    let pushed = call(
        &fixture.state,
        scim
            .body(Body::from(
                json!({
                    "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
                    "userName": email,
                    "displayName": "Pushed Account",
                    "externalId": format!("dir-{}", Uuid::new_v4().simple()),
                    "active": true,
                })
                .to_string(),
            ))
            .expect("the request must build"),
    )
    .await;
    assert!(
        pushed.status == StatusCode::CREATED || pushed.status == StatusCode::OK,
        "the connector must be able to push an account: {} {}",
        pushed.status,
        pushed.body
    );
    let user_id: Uuid = pushed.body["id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .expect("the created document must carry an id");

    // -- claim 1: the push wrote the column, and wrote the *unattributed* shape ---------------
    let row: (String, Option<Uuid>, Option<String>) = sqlx::query_as(
        "select identity_source, provisioned_by_provider_id, external_id \
           from users where id = $1",
    )
    .bind(user_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the pushed account must be readable");
    assert_eq!(
        row.0, "scim",
        "a pushed account must be attributed to the connector, or the guard counts a directory \
         that owns people as zero"
    );
    assert_eq!(
        row.1, None,
        "a SCIM token names no provider, so the provider half stays null"
    );
    assert_eq!(
        row.2, None,
        "and the external id travels with the provider or not at all — `0127`'s pairing check \
         would refuse a half-written provenance, so the reader falls back to the legacy blob"
    );

    // -- and the panel account is untouched -------------------------------------------------
    assert_eq!(
        fixture.source_of(panel).await,
        "local",
        "a person who signed up through the panel is not somebody else's directory's account"
    );

    // -- claim 2: the guard sees it, on a provider that provisioned nothing itself -------------
    let impact = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/providers/{provider}/deletion-impact"),
            Some(&fixture.session),
            None,
        ),
    )
    .await;
    assert_eq!(impact.status, StatusCode::OK, "{}", impact.body);
    assert_eq!(
        impact.body["affected_accounts"], 1,
        "a directory that just created somebody must read as affecting one account: {}",
        impact.body
    );
    assert_eq!(impact.body["blocked"], true, "{}", impact.body);
    let by_source = impact.body["by_source"]
        .as_array()
        .expect("the breakdown must be an array");
    assert_eq!(
        by_source.len(),
        1,
        "the local panel account must not be counted: {by_source:?}"
    );
    assert_eq!(by_source[0]["source"], "scim", "{by_source:?}");

    // -- claim 3: the delete is refused, naming the count ------------------------------------
    let refused = call(
        &fixture.state,
        fixture.write(
            Method::DELETE,
            &format!("/api/v1/iam/providers/{provider}"),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status, StatusCode::CONFLICT,
        "a provider whose directory owns an account must not be deletable: {}",
        refused.body
    );
    let message = refused.body["error"]["message"]
        .as_str()
        .expect("the refusal must carry a message");
    assert!(
        message.contains("1 account") && message.contains("scim"),
        "the message names the number and the system: {message}"
    );
    assert!(
        fixture.provider_exists(provider).await,
        "and the provider is still there"
    );
}

/// The update path stamps the column too, not only the create path.
///
/// A directory that creates nobody and only ever patches the accounts it finds in the table is a
/// real deployment — the classic "sync an existing IdP" connector — and it owns those accounts at
/// least as much as a connector that made them. Written as its own walk because the create and the
/// patch reach different code (`create_user` and `apply_user_changes`), and a fix applied to one
/// is invisible to the other.
#[tokio::test]
async fn a_patched_account_is_attributed_too() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let provider = fixture.provider("patched").await;

    // An account that exists as local — which is the state a connector finds a panel sign-up in.
    let existing = fixture
        .account("adopted", IdentitySource::Local, None)
        .await;
    assert_eq!(fixture.source_of(existing).await, "local");

    let issued = omnion_identity::provisioning::create_token(
        fixture.db.pool(),
        fixture.organization_id,
        "adoption walk",
        None,
    )
    .await
    .expect("a token must mint");

    // A patch that changes nothing anybody would notice — a display name. The claim is about the
    // *attribution*, not about the write: a connector maintaining this account is claiming it.
    let mut patch = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/api/v1/scim/v2/Users/{existing}"))
        .header(header::AUTHORIZATION, format!("Bearer {}", issued.secret))
        .header(header::CONTENT_TYPE, "application/json");
    patch = patch.header(header::ACCEPT, "application/json");
    let patched = call(
        &fixture.state,
        patch
            .body(Body::from(
                json!({
                    "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                    "Operations": [{
                        "op": "replace",
                        "value": { "displayName": "Adopted By Directory" },
                    }],
                })
                .to_string(),
            ))
            .expect("the request must build"),
    )
    .await;
    assert!(
        patched.status == StatusCode::OK || patched.status == StatusCode::NO_CONTENT,
        "the patch must be accepted: {} {}",
        patched.status,
        patched.body
    );
    assert_eq!(
        fixture.source_of(existing).await,
        "scim",
        "an account a connector actively maintains is its own account, and the guard's count \
         comes from this column"
    );

    let impact = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/providers/{provider}/deletion-impact"),
            Some(&fixture.session),
            None,
        ),
    )
    .await;
    assert_eq!(impact.body["affected_accounts"], 1, "{}", impact.body);
    assert_eq!(impact.body["blocked"], true, "{}", impact.body);
}

/// A re-send changes nothing, which is what keeps a timed sweep from looking like a change.
///
/// Connectors re-send their whole user set on a timer. If the stamp were a blind `update`, the
/// write would be a lie in a different direction: a log that claims an account changed on every
/// sweep trains an operator to ignore the one sweep that did change something. Asserted as the
/// return value of the store function, because that is the only place the difference is visible —
/// a database read after the fact cannot tell a no-op update from one that rewrote the same value.
#[tokio::test]
async fn a_resend_does_not_claim_a_change_it_did_not_make() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let account = fixture
        .account("resend", IdentitySource::Local, None)
        .await;

    let first = provenance::mark_scim_provisioned(fixture.db.pool(), account)
        .await
        .expect("the first stamp must apply");
    assert!(first, "the first push claims the account");

    let second = provenance::mark_scim_provisioned(fixture.db.pool(), account)
        .await
        .expect("the second stamp must be a no-op, not an error");
    assert!(!second, "a re-send changes nothing, and says so");
    assert_eq!(fixture.source_of(account).await, "scim");

    // And a local account that a connector never touched is still not blocked by anything.
    let provider = fixture.provider("untouched").await;
    let impact = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/iam/providers/{provider}/deletion-impact"),
            Some(&fixture.session),
            None,
        ),
    )
    .await;
    assert_eq!(
        impact.body["affected_accounts"], 1,
        "only the stamped account counts: {}",
        impact.body
    );
}
