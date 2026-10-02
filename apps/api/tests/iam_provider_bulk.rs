//! Integration walk for the two halves of acceptance 11 (REQ-065, slice 4 part 11).
//!
//! The criterion reads "Bulk enable/disable works; a disabled provider's button disappears from
//! `/login` within one page load." Neither half existed. The registry had `POST
//! /{id}/enable` and `/{id}/disable` and nothing over a selection; and the public
//! `GET /auth/sso/providers` route — which is exactly the list the sign-in screen needs — had
//! **zero callers** in the panel, so `/login` rendered a password form and nothing else. A
//! criterion can be half *unproven* or half **absent**, and the second is what this was: there
//! was no button for the disappearance to happen to.
//!
//! The walk is built so each claim can fail on its own, because collapsing any two of them
//! produces a product that looks right:
//!
//! * **A partial batch is a row, not a hole.** Switching three providers on when one has never
//!   passed a test does two things and refuses one. An answer that is a bare count, or a
//!   `results` list the refused provider is *absent* from, reports a success the operator did
//!   not get. A missing row and a row that was never sent are indistinguishable, so a refusal
//!   must occupy a row of its own, carrying its own code.
//! * **The gate is asked per row and one refusal does not stop the batch.** If the first
//!   untested provider aborted the request, an operator could not switch off a broken directory
//!   without first fixing the four that are fine — which is the exact moment the batch is wanted.
//! * **A missing id is not a refused id.** A provider in another tenant is a tenancy decision,
//!   not a gate decision; reported in `results` it would blame the gate for something the gate
//!   never saw, and the panel would render "needs a passing test" for an id it is not allowed
//!   to see.
//! * **`disable` is never gated.** A provider that has never been tested can always be switched
//!   off, in a batch and alone. The safe direction must not be behind the same lock.
//! * **The batch is idempotent.** Re-running it reports the same state and writes no second
//!   change; a re-tick is a person asking twice, not two events in the audit.
//! * **A disabled provider leaves the sign-in list within one read.** The public list is
//!   `where enabled`, and the walk reads it before and after the batch — not by counting
//!   widgets, but by asking the route a person standing at `/login` would ask.
//!
//! Everything goes through the real router against a real database, and every provider is
//! created through the same store the panel uses.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_permissions::seed;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// The CSRF key this walk runs under.
///
/// Every route here is a `POST` behind a session cookie, and the CSRF layer **refuses** a
/// cookie-authenticated write when no secret is configured rather than skipping the check —
/// `csrf_unavailable`, `403`, on every single call. That is the right production behaviour and a
/// very confusing test failure: the walk would report that bulk enable is broken when it never
/// reached the bulk handler. So the key is set here, next to the assertions, rather than being
/// discovered from an error message. It is a throwaway value in a throwaway database and is not
/// a credential for anything.
const CSRF_SECRET: &str = "iam-provider-bulk-walk-csrf-secret";


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

/// As [`request`], but able to present a CSRF token.
///
/// The token is derived from the **session id**, which is why the caller has to hand it in
/// rather than the walk deriving it from the cookie string: the cookie carries the token, not
/// the id, and the id only exists inside the login response. Passing `None` here produces a
/// request with no token at all, which is what the read-only calls want.
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

async fn live_state() -> Option<(AppState, Db)> {
    // Set before `Config::from_env`, because that is what reads it and caches it into the
    // `AppState` every later request checks against. Setting it afterwards would leave the
    // router holding a config with no secret, and the failure would surface as `403` on every
    // write rather than as anything that names CSRF.
    //
    // `unsafe` around `set_var` is required by the 2024 edition's new safety rule; tests run in
    // one process per target and the value is constant, so there is no concurrent writer to race
    // with — and the alternative, exporting it from the walk script, is what made this walk fail
    // in the first place: the value belongs with the code that depends on it.
    unsafe { std::env::set_var("OMNION_CSRF_SECRET", CSRF_SECRET) };

    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({error}) — the provider-bulk walk needs a \
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
    /// The CSRF token derived from this session's id.
    ///
    /// Derived **once**, at sign-in, and then presented on every write: the token is bound to the
    /// session id, so it stays valid for the life of this fixture and would stop being valid the
    /// moment the session rotated. Computing it per call would hide that binding, which is the
    /// whole property being relied on.
    csrf: String,
    foreign_provider: Uuid,
    /// The hostname this fixture's organization is registered under.
    ///
    /// Per-fixture rather than a shared constant because `site_domains.host` is **globally**
    /// unique, and this file builds a fixture per test against one database: a constant host is
    /// registered by the first test and every later one dies on `23505`. Making it unique per
    /// fixture is also what keeps the tests independent — no test can pass because another one
    /// already registered the domain.
    host: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let mut organization_id = Uuid::nil();
        let mut session = String::new();
        let mut csrf = String::new();
        let mut foreign_provider = Uuid::nil();
        // Unique per fixture: `site_domains.host` is globally unique, so a shared constant is
        // registered once and every later test dies on the duplicate.
        let host = format!("w9-signin-{}.omnion.test", Uuid::new_v4().simple());

        for index in 0..2 {
            let slug = format!("prov-bulk-{index}-{}", Uuid::new_v4().simple());
            let org: Uuid = sqlx::query_scalar(
                "insert into organizations (name, slug) values ($1, $2) returning id",
            )
            .bind("Provider Bulk Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the organization must be created");

            // A site on the organization, with `SIGN_IN_HOST` as its registered domain.
            //
            // Without it `GET /auth/sso/providers` cannot tell which organization a request is
            // for: the fixture holds two, so the route resolves the host through
            // `sites join site_domains` and answers `501 organization_required` for anything it
            // cannot place. That refusal is correct — a real multi-tenant installation reached
            // on an unknown hostname *should* be refused — so the walk cannot be fixed by
            // relaxing the check; it has to reach the panel the way the panel is reached.
            //
            // Only the **first** organization gets one. The second has to stay host-less, so a
            // provider there is unaddressable by host and the cross-tenant assertion is about a
            // provider this call genuinely cannot reach, not one it merely chose not to.
            if index == 0 {
                let site: Uuid = sqlx::query_scalar(
                    "insert into sites (organization_id, key, name) values ($1, $2, $3) \
                     returning id",
                )
                .bind(org)
                .bind("w9-sign-in")
                .bind("w9 Sign-in Site")
                .fetch_one(db.pool())
                .await
                .expect("the site must be created");
                sqlx::query("insert into site_domains (site_id, host, is_primary) \
                             values ($1, $2, true)")
                    .bind(site)
                    .bind(&host)
                    .execute(db.pool())
                    .await
                    .expect("the domain must be registered");
            }

            // One provider per organization, disabled. A second organization's provider is what
            // makes the cross-tenant row real: a row that never existed as a state any code
            // path produces would only be proving that a UUID does not match a UUID.
            let provider: Uuid = sqlx::query_scalar(
                "insert into auth_providers (organization_id, slug, name, kind, enabled, config) \
                 values ($1, $2, $3, 'oidc', false, $4) returning id",
            )
            .bind(org)
            .bind(format!("seeded-{index}"))
            .bind(format!("Seeded Provider {index}"))
            .bind(json!({ "issuer": "https://directory.invalid" }))
            .fetch_one(db.pool())
            .await
            .expect("the seeded provider must be created");

            let email = format!("prov-bulk-owner-{index}-{}@omnion.test", Uuid::new_v4().simple());
            let owner = omnion_identity::users::create_user(
                db.pool(),
                omnion_identity::users::NewUser {
                    email: email.clone(),
                    password: PASSWORD.to_owned(),
                    display_name: "Provider Bulk Test Owner".to_owned(),
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
            // The token is derived from the session id, so it can only be built once the cookie
            // value is in hand. Deriving it here — rather than inside `call` — is what keeps the
            // binding honest: a helper that quietly derived a token per request would keep
            // working even if the session rotated, and this walk depends on the rotation being
            // what invalidates it.
            let token = omnion_security::derive_csrf_token(
                CSRF_SECRET.as_bytes(),
                &omnion_identity::sessions::resolve_session(
                    db.pool(),
                    // The bare token, not the `name=value` cookie prefix — this is the function
                    // the CSRF layer itself calls, so deriving from its result is what makes the
                    // walk's token the same token the server expects rather than one that merely
                    // looks like it.
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

            if index == 0 {
                organization_id = org;
                session = cookie;
                // Only the first organization's session is the one the walks act as; the second
                // exists only to own `foreign_provider`, and its token is never needed.
                csrf = token;
            } else {
                foreign_provider = provider;
            }
        }

        Some(Self {
            state,
            db,
            organization_id,
            session,
            csrf,
            foreign_provider,
            host,
        })
    }

    /// A provider in this organization, disabled and never tested unless asked otherwise.
    async fn provider(&self, slug: &str, tested: Option<bool>) -> Uuid {
        sqlx::query_scalar(
            "insert into auth_providers (organization_id, slug, name, kind, enabled, config, \
                                     last_test_ok) \
             values ($1, $2, $3, 'oidc', false, $4, $5) returning id",
        )
        .bind(self.organization_id)
        .bind(slug)
        .bind(format!("Provider {slug}"))
        .bind(json!({ "issuer": "https://directory.invalid" }))
        .bind(tested)
        .fetch_one(self.db.pool())
        .await
        .expect("the provider must be created")
    }

    async fn enabled(&self, id: Uuid) -> bool {
        sqlx::query_scalar("select enabled from auth_providers where id = $1")
            .bind(id)
            .fetch_one(self.db.pool())
            .await
            .expect("the provider must be readable")
    }

    /// The public list the sign-in screen renders, as a person at `/login` would ask for it.
    ///
    /// The `Host` header is not decoration. This installation holds two organizations, so
    /// `organization_for_request` resolves the host through `sites join site_domains` and
    /// answers `501 organization_required` for anything it cannot place — the correct refusal
    /// for a multi-tenant panel reached on a hostname nobody registered. Sending this fixture's
    /// host is therefore the only way to make the request the one a person standing at `/login`
    /// makes, and it is what the criterion's "within one page load" is actually about.
    async fn sign_in_list(&self) -> Vec<String> {
        let answer = call(
            &self.state,
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/auth/sso/providers")
                .header(header::HOST, &self.host)
                .body(Body::empty())
                .expect("the request must build"),
        )
        .await;
        assert_eq!(
            answer.status,
            StatusCode::OK,
            "the public provider list must answer: {}",
            answer.body
        );
        answer.body["providers"]
            .as_array()
            .expect("providers is a list")
            .iter()
            .filter_map(|row| row["slug"].as_str().map(str::to_owned))
            .collect()
    }

    async fn bulk(&self, action: &str, ids: &[Uuid]) -> TestResponse {
        call(
            &self.state,
            request_with_csrf(
                Method::POST,
                "/api/v1/iam/providers/bulk",
                Some(&self.session),
                Some(json!({ "action": action, "ids": ids })),
                Some(&self.csrf),
            ),
        )
        .await
    }
}

#[tokio::test]
async fn a_bulk_enable_refuses_the_untested_row_and_still_switches_the_others_on() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };

    let okta = fixture.provider("okta", Some(true)).await;
    let azure = fixture.provider("azure", Some(true)).await;
    // The one the gate is about. `last_test_ok` is NULL, not false: "never tested" and "tested
    // and failed" are different sentences and the walk has to hold both, so this one is null
    // and the refusal is asserted to be the *never tested* one rather than merely non-empty.
    let untested = fixture.provider("untested", None).await;

    let answer = fixture
        .bulk("enable", &[okta, azure, untested])
        .await;

    // A partial batch is not an error. If this were 4xx the walk would be measuring the wrong
    // product: refusing the whole request is the behaviour the criterion's "works" is about
    // *not* meaning.
    assert_eq!(
        answer.status,
        StatusCode::OK,
        "a partial batch must still answer 200: {}",
        answer.body
    );
    assert_eq!(answer.body["action"], "enable");
    assert_eq!(answer.body["applied"], 2, "two of three changed: {}", answer.body);
    assert_eq!(
        answer.body["refused"], 1,
        "the untested provider is refused: {}",
        answer.body
    );
    assert!(answer.body["missing"].as_array().expect("missing is a list").is_empty());

    // **A refusal is a row, not a hole.** This is the assertion that fails first against an
    // implementation that filters refusals out of the answer, and that failure is the whole
    // point: a list of three that comes back with two is indistinguishable from a request for
    // two.
    let results = answer.body["results"]
        .as_array()
        .expect("results is a list");
    assert_eq!(results.len(), 3, "one row per requested id: {results:?}");
    // In the order the ids arrived, so the panel's list lines up with what was ticked.
    assert_eq!(results[0]["id"], okta.to_string());
    assert_eq!(results[1]["id"], azure.to_string());
    assert_eq!(results[2]["id"], untested.to_string());
    assert_eq!(results[0]["slug"], "okta");

    // The refusal names itself, and names the *reason*: an operator reading "provider_not_ready"
    // with no sentence has to guess between the three states the gate can refuse.
    assert_eq!(results[2]["enabled"], Value::Null, "a refused row has no state");
    assert_eq!(results[2]["error"], "provider_not_ready");
    let message = results[2]["message"]
        .as_str()
        .expect("the refusal carries a sentence");
    assert!(
        message.contains("never passed a connection test"),
        "the refusal must name the never-tested state, not merely refuse: {message}"
    );

    // The refusals did not stop the other two, and the refusals did not sneak through.
    assert!(fixture.enabled(okta).await, "okta is live after the batch");
    assert!(fixture.enabled(azure).await, "azure is live after the batch");
    assert!(
        !fixture.enabled(untested).await,
        "the untested provider must still be off — the refusal is not a warning"
    );
}

#[tokio::test]
async fn a_provider_with_another_tenants_id_is_missing_rather_than_refused() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let own = fixture.provider("own", Some(true)).await;

    let answer = fixture
        .bulk("enable", &[own, fixture.foreign_provider, Uuid::new_v4()])
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);

    // The two that are not ours are named in `missing` and get **no row at all** — a row with
    // `provider_not_ready` would blame the gate for a tenancy decision the gate never saw, and
    // the panel would render "needs a passing test" for an id the caller is not allowed to read.
    let missing = answer.body["missing"].as_array().expect("missing is a list");
    assert_eq!(missing.len(), 2, "both foreign ids are reported apart: {}", answer.body);
    assert_eq!(missing[0], fixture.foreign_provider.to_string());
    assert_eq!(answer.body["refused"], 0, "a tenancy decision is not a gate refusal");
    assert_eq!(answer.body["applied"], 1);

    let results = answer.body["results"].as_array().expect("results is a list");
    assert_eq!(results.len(), 1, "only the caller's own provider has a row: {results:?}");
    assert_eq!(results[0]["id"], own.to_string());

    // The other organization's provider must be untouched, not switched on by accident.
    let foreign_enabled: bool =
        sqlx::query_scalar("select enabled from auth_providers where id = $1")
            .bind(fixture.foreign_provider)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the foreign provider must be readable");
    assert!(
        !foreign_enabled,
        "a cross-tenant id must not change the other organization's provider"
    );
}

#[tokio::test]
async fn disable_is_never_gated_and_a_repeat_batch_changes_nothing() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    // Never tested. Switching it OFF must work, because the safe direction must never sit behind
    // the same lock as the dangerous one: an operator staring at a half-configured directory has
    // to be able to stop it.
    let broken = fixture.provider("broken", None).await;
    let tested = fixture.provider("tested", Some(true)).await;

    // First: both on, through the gate (the never-tested one cannot be), then straight off.
    sqlx::query("update auth_providers set enabled = true where id = $1")
        .bind(tested)
        .execute(fixture.db.pool())
        .await
        .expect("the tested provider must be switched on for the fixture");

    let first = fixture.bulk("disable", &[broken, tested]).await;
    assert_eq!(first.status, StatusCode::OK, "a disable must never be gated: {}", first.body);
    assert_eq!(first.body["refused"], 0, "nothing is refused: {}", first.body);
    assert_eq!(first.body["applied"], 1, "only the one that was on changed: {}", first.body);
    assert!(
        !fixture.enabled(broken).await && !fixture.enabled(tested).await,
        "both are off"
    );

    // **Idempotence, asserted as a fact and not as a shape.** A re-tick is a person asking the
    // same question twice; the second run must report the state without claiming a change. A
    // batch that reports `applied: 2` on a no-op would put two audit rows over one decision.
    let second = fixture.bulk("disable", &[broken, tested]).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    assert_eq!(
        second.body["applied"], 0,
        "re-running the same batch changes nothing: {}",
        second.body
    );
    // Both rows are still present though — the answer is about the requested ids, not about the
    // ones that happened to move.
    assert_eq!(
        second.body["results"].as_array().expect("results").len(),
        2,
        "an unchanged row is still a row"
    );
}

#[tokio::test]
async fn a_disabled_provider_leaves_the_sign_in_list_within_one_read() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let okta = fixture.provider("okta", Some(true)).await;
    let keep = fixture.provider("keep", Some(true)).await;

    // Both on to begin with, so the list has something to lose.
    sqlx::query("update auth_providers set enabled = true where id = any($1)")
        .bind(vec![okta, keep])
        .execute(fixture.db.pool())
        .await
        .expect("both providers must be switched on for the fixture");

    let before = fixture.sign_in_list().await;
    assert!(
        before.iter().any(|slug| slug == "okta"),
        "an enabled provider is on the sign-in list: {before:?}"
    );
    assert!(
        before.iter().any(|slug| slug == "keep"),
        "both are offered to begin with: {before:?}"
    );

    // The *bulk* path, not the single verb: the criterion is about what the batch does to the
    // list, and a list observed after the single verb would be proving the other route.
    let answer = fixture.bulk("disable", &[okta]).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body["applied"], 1);

    // "Within one page load" is not a duration, it is a *read*: the next person who loads
    // `/login` asks the list and must not see the provider. Reading the route is the same ask
    // the button list is built from, so this is the claim the screen actually makes.
    let after = fixture.sign_in_list().await;
    assert!(
        !after.iter().any(|slug| slug == "okta"),
        "a provider switched off in a batch is gone from the sign-in list: {after:?}"
    );
    assert!(
        after.iter().any(|slug| slug == "keep"),
        "the provider that was not in the batch is untouched: {after:?}"
    );

    // And back on again, because "gone" is a statement about one read and not about the provider
    // being deleted — the list must come back.
    sqlx::query("update auth_providers set enabled = true where id = $1")
        .bind(okta)
        .execute(fixture.db.pool())
        .await
        .expect("the provider must be switched back on");
    let restored = fixture.sign_in_list().await;
    assert!(
        restored.iter().any(|slug| slug == "okta"),
        "a switched-off provider is unreachable, not gone: {restored:?}"
    );
}

#[tokio::test]
async fn a_batch_refuses_an_unknown_action_and_an_empty_selection_by_name() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("SKIP: no database");
        return;
    };
    let provider = fixture.provider("okta", Some(true)).await;

    // An action nobody offered. `sync` is in the API (it exists, per provider); making a batch
    // out of it would mean nine directory walks from one click, and the request's bulk row says
    // Enable and Disable. The refusal names both so the caller is not left guessing.
    let unknown = fixture.bulk("sync", &[provider]).await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST, "{}", unknown.body);
    let message = unknown.body["error"]["message"]
        .as_str()
        .expect("the refusal carries a sentence");
    assert!(
        message.contains("enable") && message.contains("disable"),
        "the refusal must name the two actions that exist: {message}"
    );
    assert!(
        !fixture.enabled(provider).await,
        "a refused action changes nothing"
    );

    // An empty selection. `apply to nothing` is a success on a screen the operator will read as
    // "done", and it is a panel bug; the refusal names it.
    let empty = fixture.bulk("enable", &[]).await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{}", empty.body);
    assert_eq!(empty.body["error"]["code"], "no_providers");
}
