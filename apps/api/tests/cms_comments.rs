//! Integration test for page comments (REQ-064, slice 4a).
//!
//! Comments are the one table a stranger writes, so the walks here are not "the happy path and
//! an error" — they are the claims the REQ makes about what the platform does with a stranger's
//! input, each of which is a place a plausible implementation gets it wrong:
//!
//! * **A comment is queued, not published.** A visitor's submission lands `pending` and the
//!   public thread is empty until a moderator approves it, and the *same* endpoint answers both
//!   ways — the same argument the menu audience filter makes, for the same reason.
//! * **The heuristics land it in Spam without anybody acting.** A blocked word, a link farm and
//!   a body typed in under the fill-time floor are three different reasons, and the reason is
//!   stored, because a heuristic whose verdict cannot be read is a verdict nobody can appeal.
//! * **A ban refuses the submission and writes nothing**, and a comment from a banned address
//!   is the one path in the module that leaves no row — everything else is recoverable.
//! * **The thread is two levels and the schema says so.** A reply to a reply is refused, and
//!   the refusal is the two-level rule firing, not a validation error the panel can route
//!   around.
//! * **Reading the inbox is not the power to change it.** An account with `comments.read` alone
//!   gets the queue and 403 on every button, which is the whole reason the two keys exist.
//!
//! It runs against the development stack, and skips itself with a printed reason when
//! PostgreSQL is not reachable — so read the `SKIP` line before believing a green count.

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
use omnion_security::{CSRF_HEADER, derive_csrf_token};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support;
use support::isolated_db::{IsolatedDb, announce_skip, assert_nothing_skipped};

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The CSRF secret this suite runs with. It must match the `OMNION_CSRF_SECRET` the run script
/// exports, because the token is derived from it.
const CSRF_SECRET: &str = "w2-seo-suite-csrf-secret";

/// A signed-in session: the cookie the browser sends and the UUID the CSRF token derives from.
/// The two are different values and the middleware needs both.
struct Auth {
    token: String,
    session_id: String,
}

/// What the community manager may do: the inbox and nothing else. The 403 assertions in
/// `reading_the_inbox_is_not_the_power_to_change_it` are the point of this list.
const MANAGER_PERMISSIONS: [&str; 2] = ["comments.read", "content.pages.read"];

/// What the site owner adds on top.
const OWNER_EXTRA: [&str; 2] = ["comments.manage", "content.pages.create"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    TestResponse {
        status,
        headers,
        body,
    }
}

fn request(method: Method, uri: &str, auth: Option<&Auth>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match auth {
        Some(auth) => {
            let token = auth.token.as_str();
            let builder = builder.header(header::COOKIE, format!("omnion_session={token}"));
            let csrf = derive_csrf_token(CSRF_SECRET.as_bytes(), &auth.session_id);
            builder.header(CSRF_HEADER, csrf)
        }
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

/// A public request, with the client hints a real browser sends.
///
/// The hints are part of what is being tested: the rate limit and the ban both key on the
/// fingerprint these two headers produce, so a walk that sends neither is testing a different
/// submission than the one the screen makes.
fn public_request(
    method: Method,
    uri: &str,
    host: &str,
    ip: &str,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .header("x-forwarded-for", ip)
        .header(header::USER_AGENT, "comment-suite/1.0");
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

async fn live_state() -> Option<(AppState, Db, IsolatedDb)> {
    let config = Config::from_env().expect("environment must be valid");
    let isolated = IsolatedDb::open(&config.database.url, 4, "cms_comments")
        .await
        .expect("the throwaway database must open");
    let Some(isolated) = isolated else {
        announce_skip("no throwaway database, this walk did not run");
        return None;
    };
    let db = isolated.db.clone();
    // Migrations are applied by `IsolatedDb::open`, before the router is built.
    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    Some((state, db, isolated))
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("comment-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Comment Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

/// Sign in and return both halves of the session.
async fn login(state: &AppState, db: &Db, email: &str) -> Auth {
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
    let cookie = response
        .headers
        .iter()
        .find(|(name, _)| name == "set-cookie")
        .map(|(_, value)| value.clone())
        .expect("login must set the session cookie");
    let token = cookie
        .split(';')
        .next()
        .expect("the cookie has a value")
        .split_once('=')
        .expect("the cookie is name=value")
        .1
        .to_owned();
    let session_id: Uuid = sqlx::query_scalar("select id from sessions where token_hash = $1")
        .bind(omnion_identity::sessions::hash_token(&token))
        .fetch_one(db.pool())
        .await
        .expect("the session row the cookie names must exist");
    Auth {
        token,
        session_id: session_id.to_string(),
    }
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

/// A site with comments turned on, one page, and two accounts.
struct Fixture {
    state: AppState,
    db: Db,
    isolated: IsolatedDb,
    org: Uuid,
    site: Uuid,
    host: String,
    page_slug: String,
    manager_email: String,
    owner_email: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db, isolated) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(org)
            .bind("Comment Test Org")
            .bind(format!("cmt-org-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        let key = format!("cmt{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&key)
            .bind("Comment Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        // The host lives in `site_domains`, not on `sites` — the public surface resolves a site
        // by the domain that addresses it, and a fixture that inserts a `host` column answers
        // `column "host" of relation "sites" does not exist` on every test in the file.
        let host = format!("{key}.example.test");
        sqlx::query(
            "insert into site_domains (site_id, host, is_primary) values ($1, $2, true)",
        )
        .bind(site)
        .bind(&host)
        .execute(db.pool())
        .await
        .expect("the site's primary host must be created");

        let (manager_id, manager_email) = create_account(&db, Some(org)).await;
        grant(
            &db,
            org,
            manager_id,
            &MANAGER_PERMISSIONS,
            "Comment Manager",
        )
        .await;

        let (owner_id, owner_email) = create_account(&db, Some(org)).await;
        let mut owner_keys = MANAGER_PERMISSIONS.to_vec();
        owner_keys.extend_from_slice(&OWNER_EXTRA);
        grant(&db, org, owner_id, &owner_keys, "Comment Owner").await;

        // The site with comments ON. A fixture that left them off would be testing a
        // submission route that answers 400 for everything, and every walk below would be
        // asserting on the refusal rather than the rule.
        let site_row = omnion_identity::sites::find_site(db.pool(), site)
            .await
            .expect("the site lookup must answer")
            .expect("the site exists");
        let store = omnion_content::page_comments::CommentStore::new(db.pool().clone());
        let mut settings = store
            .settings(site_row.id, site_row.organization_id)
            .await
            .expect("the settings row must be readable");
        settings.comments_enabled = true;
        settings.min_fill_seconds = 0;
        store
            .save_settings(&settings)
            .await
            .expect("the settings must save");

        let owner = login(&state, &db, &owner_email).await;
        let page_slug = format!("post-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let created = call(
            &state,
            request(
                Method::POST,
                "/api/v1/pages",
                Some(&owner),
                Some(json!({ "site_id": site, "slug": page_slug, "title": "A post" })),
            ),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);

        Some(Self {
            state,
            db,
            isolated,
            org,
            site,
            host,
            page_slug,
            manager_email,
            owner_email,
        })
    }

    async fn owner(&self) -> Auth {
        login(&self.state, &self.db, &self.owner_email).await
    }

    async fn manager(&self) -> Auth {
        login(&self.state, &self.db, &self.manager_email).await
    }

    /// The public thread, as a theme reads it.
    async fn public_thread(&self) -> Value {
        let response = call(
            &self.state,
            public_request(
                Method::GET,
                &format!("/api/v1/public/comments/{}", self.page_slug),
                &self.host,
                "203.0.113.10",
                None,
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        response.body
    }

    /// A visitor's submission, with a fill time long enough to pass the default floor.
    async fn submit(
        &self,
        ip: &str,
        name: &str,
        email: &str,
        body: &str,
        parent: Option<Uuid>,
    ) -> TestResponse {
        call(
            &self.state,
            public_request(
                Method::POST,
                &format!("/api/v1/public/comments/{}", self.page_slug),
                &self.host,
                ip,
                Some(json!({
                    "author_name": name,
                    "author_email": email,
                    "body": body,
                    "parent_id": parent,
                    "filled_at_ms": 30_000,
                })),
            ),
        )
        .await
    }

    /// The stored state of one comment, read from the table.
    async fn stored_status(&self, comment_id: Uuid) -> (String, Option<String>) {
        let row: (String, Option<String>) =
            sqlx::query_as("select status, spam_reason from cms_comments where id = $1")
                .bind(comment_id)
                .fetch_one(self.db.pool())
                .await
                .expect("the comment row must exist");
        row
    }

    async fn inbox(&self, auth: &Auth, status: &str) -> Value {
        let response = call(
            &self.state,
            request(
                Method::GET,
                &format!(
                    "/api/v1/comments?site_id={}&status={}",
                    self.site, status
                ),
                Some(auth),
                None,
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        response.body
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_comment_is_queued_and_appears_only_after_a_moderator_approves_it() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner().await;

    let submitted = fixture
        .submit(
            "203.0.113.11",
            "Ada",
            "ada@example.test",
            "This is a careful and useful remark about the article.",
            None,
        )
        .await;
    assert_eq!(
        submitted.status,
        StatusCode::ACCEPTED,
        "a visitor's submission is accepted: {}",
        submitted.body
    );
    let comment_id = Uuid::parse_str(
        submitted.body["id"]
            .as_str()
            .expect("the answer carries the comment's id"),
    )
    .expect("an id");

    // The same endpoint, before and after: the public thread is empty while the comment waits,
    // and the answer is identical in shape both times — which is the only thing a theme can be
    // written against.
    let before = fixture.public_thread().await;
    assert_eq!(
        before.as_array().expect("a list").len(),
        0,
        "an unapproved comment must not be public"
    );

    let (status, reason) = fixture.stored_status(comment_id).await;
    assert_eq!(status, "pending", "a first-time poster is queued");
    assert_eq!(reason, None, "a queued comment has no spam reason");

    let approved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/comments/{comment_id}?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "status": "approved" })),
        ),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(
        approved.body["status"],
        json!("approved"),
        "the answer is the comment in its new state: {}",
        approved.body
    );
    assert_ne!(
        approved.body["approved_at"],
        json!(null),
        "approving stamps a time, so the panel can show who decided and when: {}",
        approved.body
    );
    assert_ne!(
        approved.body["approved_by"],
        json!(null),
        "and records which account did it: {}",
        approved.body
    );

    let after = fixture.public_thread().await;
    let threads = after.as_array().expect("a list");
    assert_eq!(threads.len(), 1, "the approved comment is public: {after}");
    assert_eq!(threads[0]["author_name"], json!("Ada"));
    assert_eq!(threads[0]["replies"].as_array().expect("a list").len(), 0);

    // The public shape carries no address, no client hint and no moderation reason. A payload
    // is the easiest place to leak one, so the assertion is on the absence of the value, not
    // on the presence of the field.
    let rendered = after.to_string();
    assert!(
        !rendered.contains("ada@example.test"),
        "the public thread must not carry an address: {rendered}"
    );
    assert!(
        !rendered.contains("203.0.113.11"),
        "the public thread must not carry a client hint: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_comment_tripping_a_heuristic_lands_in_spam_with_the_reason_and_no_moderator() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner().await;

    // Turn the policy on, with a word and a link budget.
    let settings = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/comment-settings", fixture.site),
            Some(&owner),
            Some(json!({
                "comments_enabled": true,
                "blocked_words": ["casino", "viagra"],
                "max_links_per_comment": 2,
                "min_fill_seconds": 0,
                "per_ip_per_hour": 5,
            })),
        ),
    )
    .await;
    assert_eq!(settings.status, StatusCode::OK, "{}", settings.body);
    assert_eq!(
        settings.body["settings"]["blocked_words"],
        json!(["casino", "viagra"]),
        "the words are stored normalised: {}",
        settings.body
    );

    // 1. A blocked word. Nobody touched the inbox between the submit and the read.
    let blocked = fixture
        .submit(
            "203.0.113.12",
            "Spammer",
            "spam@example.test",
            "Try the best CASINO tonight, cheap pills here.",
            None,
        )
        .await;
    assert_eq!(blocked.status, StatusCode::ACCEPTED, "{}", blocked.body);
    let blocked_id = Uuid::parse_str(blocked.body["id"].as_str().expect("an id")).expect("an id");
    let (status, reason) = fixture.stored_status(blocked_id).await;
    assert_eq!(status, "spam", "a blocked word is spam without a moderator");
    assert_eq!(reason.as_deref(), Some("contains a blocked word"));

    // 2. A link farm: three links against a budget of two.
    let farm = fixture
        .submit(
            "203.0.113.13",
            "Promoter",
            "links@example.test",
            r#"Buy <a href="http://a.test">one</a> <a href="http://b.test">two</a> <a href="http://c.test">three</a>"#,
            None,
        )
        .await;
    assert_eq!(farm.status, StatusCode::ACCEPTED, "{}", farm.body);
    let farm_id = Uuid::parse_str(farm.body["id"].as_str().expect("an id")).expect("an id");
    let (status, reason) = fixture.stored_status(farm_id).await;
    assert_eq!(status, "spam", "three links against a budget of two is spam");
    assert_eq!(reason.as_deref(), Some("too many links"));

    // 3. Typed faster than the floor. The fixture lowered the floor to 0 so the other walks
    //    are not fighting it, so this walk raises it back.
    let policy = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/comment-settings", fixture.site),
            Some(&owner),
            Some(json!({ "comments_enabled": true, "min_fill_seconds": 30 })),
        ),
    )
    .await;
    assert_eq!(policy.status, StatusCode::OK, "{}", policy.body);
    assert_eq!(
        policy.body["settings"]["min_fill_seconds"],
        json!(30),
        "a partial save must not reset the other fields: {}",
        policy.body
    );
    assert_eq!(
        policy.body["settings"]["blocked_words"],
        json!(["casino", "viagra"]),
        "a PUT that omits a field must keep the stored value, not default it"
    );

    let fast = call(
        &fixture.state,
        public_request(
            Method::POST,
            &format!("/api/v1/public/comments/{}", fixture.page_slug),
            &fixture.host,
            "203.0.113.14",
            Some(json!({
                "author_name": "Quick",
                "author_email": "quick@example.test",
                "body": "typed in a hurry",
                "filled_at_ms": 200,
            })),
        ),
    )
    .await;
    assert_eq!(fast.status, StatusCode::ACCEPTED, "{}", fast.body);
    let fast_id = Uuid::parse_str(fast.body["id"].as_str().expect("an id")).expect("an id");
    let (status, reason) = fixture.stored_status(fast_id).await;
    assert_eq!(status, "spam", "a body under the fill floor is spam");
    assert!(reason.is_some(), "and the reason is recorded");

    // All three are in the Spam tab with their reasons, and none of them is public.
    let inbox = fixture.inbox(&owner, "spam").await;
    let rows = inbox["comments"].as_array().expect("rows");
    assert_eq!(rows.len(), 3, "all three heuristics landed: {inbox}");
    assert!(
        rows.iter()
            .any(|row| row["spam_reason"] == json!("contains a blocked word")),
        "the blocked word is labelled: {inbox}"
    );
    assert_eq!(inbox["total"], json!(3));
    assert_eq!(fixture.public_thread().await.as_array().expect("a list").len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_answers_a_comment_and_a_reply_to_a_reply_is_refused_by_the_schema() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner().await;

    let root = fixture
        .submit(
            "203.0.113.20",
            "Ada",
            "ada2@example.test",
            "A question about the third paragraph.",
            None,
        )
        .await;
    let root_id = Uuid::parse_str(root.body["id"].as_str().expect("an id")).expect("an id");
    call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/comments/{root_id}?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "status": "approved" })),
        ),
    )
    .await;

    // A visitor's reply to the approved comment.
    let reply = fixture
        .submit(
            "203.0.113.21",
            "Grace",
            "grace@example.test",
            "I had the same question.",
            Some(root_id),
        )
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.body);
    let reply_id = Uuid::parse_str(reply.body["id"].as_str().expect("an id")).expect("an id");
    call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/comments/{reply_id}?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "status": "approved" })),
        ),
    )
    .await;

    let threads = fixture.public_thread().await;
    let list = threads.as_array().expect("a list");
    assert_eq!(list.len(), 1, "a reply is not a second thread: {threads}");
    assert_eq!(list[0]["replies"].as_array().expect("a list").len(), 1);
    assert_eq!(list[0]["replies"][0]["author_name"], json!("Grace"));

    // Now the third level. The store checks the parent it can check in SQL and names the rule
    // itself; the SCHEMA's trigger is the backstop for a writer that is not this store, and the
    // route translates its refusal into the same code so a client sees one rule, not two.
    let third = fixture
        .submit(
            "203.0.113.22",
            "Someone",
            "third@example.test",
            "and one more thing",
            Some(reply_id),
        )
        .await;
    assert_eq!(
        third.status,
        StatusCode::BAD_REQUEST,
        "a reply to a reply is refused: {}",
        third.body
    );
    assert_eq!(third.body["error"]["code"], json!("comment_thread_too_deep"));

    // And the row does not exist: a refused thread is not a hidden thread.
    let stored: i64 =
        sqlx::query_scalar("select count(*) from cms_comments where parent_id = $1")
            .bind(reply_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must answer");
    assert_eq!(stored, 0, "the refused third level wrote nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_moderator_reply_is_published_immediately_and_a_hidden_parent_hides_its_reply() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner().await;

    let root = fixture
        .submit(
            "203.0.113.30",
            "Alan",
            "alan@example.test",
            "Does the export include attachments?",
            None,
        )
        .await;
    let root_id = Uuid::parse_str(root.body["id"].as_str().expect("an id")).expect("an id");
    call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/comments/{root_id}?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "status": "approved" })),
        ),
    )
    .await;

    let replied = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/comments/{root_id}/reply"),
            Some(&owner),
            Some(json!({
                "site_id": fixture.site,
                "author_name": "The team",
                "body": "It does, as a separate archive.",
            })),
        ),
    )
    .await;
    assert_eq!(replied.status, StatusCode::CREATED, "{}", replied.body);
    assert_eq!(replied.body["is_staff_reply"], json!(true));
    assert_eq!(
        replied.body["status"],
        json!("approved"),
        "a moderator has already moderated their own reply"
    );

    let threads = fixture.public_thread().await;
    let list = threads.as_array().expect("a list");
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0]["has_staff_reply"],
        json!(true),
        "the thread says it was answered: {threads}"
    );
    assert_eq!(list[0]["replies"].as_array().expect("a list").len(), 1);

    // Hiding the parent hides the whole thread: a reply whose question is gone is not promoted
    // to a top-level comment, because the moderator who hid the question never agreed to show
    // its answer.
    call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/comments/{root_id}?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "status": "trash" })),
        ),
    )
    .await;
    let after = fixture.public_thread().await;
    assert_eq!(
        after.as_array().expect("a list").len(),
        0,
        "a hidden parent hides its reply: {after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ban_refuses_the_submission_and_writes_no_row_at_all() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner().await;

    let placed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sites/{}/comment-bans", fixture.site),
            Some(&owner),
            Some(json!({
                "kind": "email",
                "value": "Nuisance@Example.test",
                "reason": "three link farms in a row",
            })),
        ),
    )
    .await;
    assert_eq!(placed.status, StatusCode::CREATED, "{}", placed.body);
    assert_eq!(
        placed.body["value"],
        json!("nuisance@example.test"),
        "the address is stored the way a submission would compare it: {}",
        placed.body
    );
    assert_eq!(placed.body["active"], json!(true));

    // The exact address, in the mixed case a moderator would type.
    let refused = fixture
        .submit(
            "203.0.113.40",
            "Nuisance",
            "nuisance@example.test",
            "A perfectly ordinary sentence.",
            None,
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], json!("comment_banned"));

    let rows: i64 = sqlx::query_scalar(
        "select count(*) from cms_comments \
         where site_id = $1 and lower(author_email) = 'nuisance@example.test'",
    )
    .bind(fixture.site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must answer");
    assert_eq!(rows, 0, "a ban is the one path that writes no row");

    // A ban on the address does not ban the site: somebody else's comment is unaffected.
    let allowed = fixture
        .submit(
            "203.0.113.41",
            "Ada",
            "ada3@example.test",
            "A perfectly ordinary sentence from somebody else.",
            None,
        )
        .await;
    assert_eq!(allowed.status, StatusCode::ACCEPTED, "{}", allowed.body);

    // Lifting the ban restores the ability to comment, and the ban is really gone from SQL —
    // asserted there, because "the button worked" and "the row is gone" are different claims.
    let ban_id = Uuid::parse_str(placed.body["id"].as_str().expect("an id")).expect("an id");
    let lifted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/sites/{}/comment-bans/{ban_id}", fixture.site),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(lifted.status, StatusCode::NO_CONTENT, "{}", lifted.body);
    let bans: i64 = sqlx::query_scalar("select count(*) from cms_comment_bans where id = $1")
        .bind(ban_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the count must answer");
    assert_eq!(bans, 0, "the ban row is really gone");

    let again = fixture
        .submit(
            "203.0.113.42",
            "Nuisance",
            "nuisance@example.test",
            "Now that the ban is lifted, this is fine.",
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::ACCEPTED, "{}", again.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hourly_limit_is_counted_against_the_fingerprint_not_the_address() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner().await;

    let policy = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/comment-settings", fixture.site),
            Some(&owner),
            Some(json!({
                "comments_enabled": true,
                "min_fill_seconds": 0,
                "per_ip_per_hour": 2,
            })),
        ),
    )
    .await;
    assert_eq!(policy.status, StatusCode::OK, "{}", policy.body);

    // Two from the same address, then a third. Each body is different, so the duplicate rule
    // is not what is being measured.
    for n in 1..=2 {
        let response = fixture
            .submit(
                "198.51.100.7",
                "Chatty",
                &format!("chatty{n}@example.test"),
                &format!("A different remark, number {n}."),
                None,
            )
            .await;
        assert_eq!(response.status, StatusCode::ACCEPTED, "{n}: {}", response.body);
        let id = Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id");
        let (status, _) = fixture.stored_status(id).await;
        assert_eq!(status, "pending", "{n} is inside the allowance");
    }

    let third = fixture
        .submit(
            "198.51.100.7",
            "Chatty",
            "chatty3@example.test",
            "A different remark, number 3.",
            None,
        )
        .await;
    assert_eq!(third.status, StatusCode::ACCEPTED, "{}", third.body);
    let third_id = Uuid::parse_str(third.body["id"].as_str().expect("an id")).expect("an id");
    let (status, reason) = fixture.stored_status(third_id).await;
    assert_eq!(status, "spam", "the third from one address is over the limit");
    assert_eq!(
        reason.as_deref(),
        Some("too many comments from one address")
    );

    // A different address is unaffected: the limit is per client, not global.
    let other = fixture
        .submit(
            "198.51.100.8",
            "Quiet",
            "quiet@example.test",
            "A first remark from a different network.",
            None,
        )
        .await;
    assert_eq!(other.status, StatusCode::ACCEPTED, "{}", other.body);
    let other_id = Uuid::parse_str(other.body["id"].as_str().expect("an id")).expect("an id");
    assert_eq!(fixture.stored_status(other_id).await.0, "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_the_inbox_is_not_the_power_to_change_it() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let manager = fixture.manager().await;

    let submitted = fixture
        .submit(
            "203.0.113.50",
            "Ada",
            "ada4@example.test",
            "A remark for the queue.",
            None,
        )
        .await;
    let id = Uuid::parse_str(submitted.body["id"].as_str().expect("an id")).expect("an id");

    // `comments.read` opens the queue.
    let inbox = fixture.inbox(&manager, "pending").await;
    assert_eq!(inbox["total"], json!(1), "{inbox}");

    // …and every button on it answers 403. This is the assertion the two keys exist for.
    let refused = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/comments/{id}?site_id={}", fixture.site),
            Some(&manager),
            Some(json!({ "status": "approved" })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "reading the queue is not approving it: {}",
        refused.body
    );

    let stored = fixture.stored_status(id).await;
    assert_eq!(stored.0, "pending", "the refusal changed nothing");

    // The policy is behind the other key too: a manager who cannot approve a comment should not
    // be able to turn moderation off.
    let policy = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/comment-settings", fixture.site),
            Some(&manager),
            Some(json!({ "comments_enabled": true, "per_ip_per_hour": 999 })),
        ),
    )
    .await;
    assert_eq!(policy.status, StatusCode::FORBIDDEN, "{}", policy.body);
    assert_eq!(fixture.stored_status(id).await.0, "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_inbox_counts_every_tab_and_a_bulk_action_reports_what_it_skipped() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner().await;

    let mut ids = Vec::new();
    for n in 1..=3 {
        let response = fixture
            .submit(
                "203.0.113.60",
                "Ada",
                &format!("bulk{n}@example.test"),
                &format!("Remark number {n}, long enough to be its own sentence."),
                None,
            )
            .await;
        ids.push(
            Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id"),
        );
    }
    let blocked = fixture
        .submit(
            "203.0.113.61",
            "Spammer",
            "spam2@example.test",
            "nothing to see",
            None,
        )
        .await;
    ids.push(
        Uuid::parse_str(blocked.body["id"].as_str().expect("an id")).expect("an id"),
    );

    // The tab counts are a grouped query over the whole inbox, not a count of the visible
    // rows: a tab bar that says "50" the moment a site passes fifty comments is a tab bar that
    // cannot be trusted to say "there is nothing in Spam".
    let inbox = fixture.inbox(&owner, "pending").await;
    let counts: Vec<(String, i64)> = inbox["counts"]
        .as_array()
        .expect("counts")
        .iter()
        .map(|row| {
            (
                row["status"].as_str().expect("a status").to_owned(),
                row["count"].as_i64().expect("a count"),
            )
        })
        .collect();
    assert_eq!(
        counts,
        vec![
            ("pending".to_owned(), 4),
            ("approved".to_owned(), 0),
            ("spam".to_owned(), 0),
            ("trash".to_owned(), 0),
        ],
        "every tab is present even at zero: {inbox}"
    );
    assert_eq!(
        inbox["statuses"],
        json!(["pending", "approved", "spam", "trash"]),
        "the tab vocabulary comes from the API, not from the panel's own list"
    );

    // Bulk approve three of the four — the fourth is already in the state asked for, which is
    // the interesting case: a bulk action that is half-done must SAY so.
    let bulk = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/comments/bulk?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "status": "approved", "comment_ids": ids[..3] })),
        ),
    )
    .await;
    assert_eq!(bulk.status, StatusCode::OK, "{}", bulk.body);
    assert_eq!(bulk.body["updated"].as_array().expect("a list").len(), 3);
    assert_eq!(bulk.body["complete"], json!(true));
    assert_eq!(bulk.body["requested"], json!(3));

    // The same request again: all three are already approved, so all three are refused and
    // the answer is a report rather than a silent success.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/comments/bulk?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "status": "approved", "comment_ids": ids[..3] })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(again.body["updated"].as_array().expect("a list").len(), 0);
    assert_eq!(again.body["refused"].as_array().expect("a list").len(), 3);
    assert_eq!(
        again.body["complete"],
        json!(false),
        "a partial bulk action must not report itself complete"
    );

    // A comment that has been deleted is reported as missing, not as refused: the panel can
    // tell a moderator "that one is gone" and "that one did not move".
    let ghost = Uuid::new_v4();
    let with_ghost = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/comments/bulk?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "status": "trash", "comment_ids": [ids[3], ghost] })),
        ),
    )
    .await;
    assert_eq!(with_ghost.status, StatusCode::OK, "{}", with_ghost.body);
    assert_eq!(with_ghost.body["updated"].as_array().expect("a list").len(), 1);
    assert_eq!(
        with_ghost.body["missing"],
        json!([ghost.to_string()]),
        "a comment that never existed is missing, not refused: {}",
        with_ghost.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_body_from_one_address_is_spam_rather_than_a_second_row() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    // No moderator touches the inbox in this walk, on purpose: the point is that the STORE
    // decided, so a moderator's presence would only make the test weaker.
    let body = "Exactly the same sentence, twice over, from the same person.";
    let first = fixture
        .submit("203.0.113.70", "Ada", "dupe@example.test", body, None)
        .await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.body);
    let first_id = Uuid::parse_str(first.body["id"].as_str().expect("an id")).expect("an id");

    // A double-clicked Send button is the case this exists for. It is NOT a hard duplicate
    // index: the row is written and marked, so a moderator can see that somebody submitted
    // twice rather than losing the evidence.
    let second = fixture
        .submit("203.0.113.70", "Ada", "dupe@example.test", body, None)
        .await;
    assert_eq!(second.status, StatusCode::ACCEPTED, "{}", second.body);
    let second_id = Uuid::parse_str(second.body["id"].as_str().expect("an id")).expect("an id");
    let (status, reason) = fixture.stored_status(second_id).await;
    assert_eq!(status, "spam");
    assert_eq!(reason.as_deref(), Some("duplicate of an earlier comment"));

    // The first one is untouched: a duplicate does not retroactively poison the original.
    assert_eq!(fixture.stored_status(first_id).await.0, "pending");

    // Different case on the address is the SAME person, because an address is stored lower-case.
    let third = fixture
        .submit(
            "203.0.113.70",
            "Ada",
            "DUPE@Example.test",
            "A different sentence entirely, from the same address.",
            None,
        )
        .await;
    assert_eq!(third.status, StatusCode::ACCEPTED, "{}", third.body);
    let third_id = Uuid::parse_str(third.body["id"].as_str().expect("an id")).expect("an id");
    assert_eq!(
        fixture.stored_status(third_id).await.0,
        "pending",
        "a different body is not a duplicate"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_honeypot_and_a_disabled_site_are_the_two_submissions_that_write_nothing() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner().await;

    let caught = call(
        &fixture.state,
        public_request(
            Method::POST,
            &format!("/api/v1/public/comments/{}", fixture.page_slug),
            &fixture.host,
            "203.0.113.80",
            Some(json!({
                "author_name": "Bot",
                "author_email": "bot@example.test",
                "body": "buy things",
                "honeypot": "http://spam.example",
                "filled_at_ms": 30_000,
            })),
        ),
    )
    .await;
    assert_eq!(caught.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", caught.body);
    // Scoped to THIS site, not to the table: a development database is shared with every other
    // suite, and a count of the whole table asserts something about the database rather than
    // about the route.
    let written: i64 = sqlx::query_scalar(
        "select count(*) from cms_comments where site_id = $1",
    )
    .bind(fixture.site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must answer");
    assert_eq!(written, 0, "a filled honeypot writes nothing");

    // Comments turned off.
    let policy = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/comment-settings", fixture.site),
            Some(&owner),
            Some(json!({ "comments_enabled": false })),
        ),
    )
    .await;
    assert_eq!(policy.status, StatusCode::OK, "{}", policy.body);
    let refused = fixture
        .submit(
            "203.0.113.81",
            "Ada",
            "ada5@example.test",
            "A remark on a site with comments turned off.",
            None,
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], json!("invalid_comment"));

    let still: i64 = sqlx::query_scalar(
        "select count(*) from cms_comments where site_id = $1",
    )
    .bind(fixture.site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must answer");
    assert_eq!(still, 0, "a site with comments off accepts nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_address_the_owner_trusts_skips_the_queue_and_another_sites_comments_are_not_ours() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.owner().await;

    let policy = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sites/{}/comment-settings", fixture.site),
            Some(&owner),
            Some(json!({
                "comments_enabled": true,
                "min_fill_seconds": 0,
                "auto_approve_after_comments": 1,
            })),
        ),
    )
    .await;
    assert_eq!(policy.status, StatusCode::OK, "{}", policy.body);

    // The first comment is queued, an administrator approves it, and the second is published
    // without anybody looking.
    let first = fixture
        .submit(
            "203.0.113.90",
            "Regular",
            "regular@example.test",
            "The first remark, which a human will look at.",
            None,
        )
        .await;
    let first_id = Uuid::parse_str(first.body["id"].as_str().expect("an id")).expect("an id");
    assert_eq!(fixture.stored_status(first_id).await.0, "pending");

    call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/comments/{first_id}?site_id={}", fixture.site),
            Some(&owner),
            Some(json!({ "status": "approved" })),
        ),
    )
    .await;

    let second = fixture
        .submit(
            "203.0.113.91",
            "Regular",
            "regular@example.test",
            "The second remark, which the policy publishes by itself.",
            None,
        )
        .await;
    assert_eq!(second.status, StatusCode::ACCEPTED, "{}", second.body);
    let second_id = Uuid::parse_str(second.body["id"].as_str().expect("an id")).expect("an id");
    let (status, reason) = fixture.stored_status(second_id).await;
    assert_eq!(status, "approved", "a trusted address skips the queue");
    assert_eq!(reason, None, "an auto-approved comment is not spam");

    let threads = fixture.public_thread().await;
    let list = threads.as_array().expect("a list");
    assert_eq!(list.len(), 2, "both are public: {threads}");

    // A stranger's inbox is not this site's inbox. The site id is in the query, so a caller
    // who has never heard of this site gets nothing rather than an existence oracle.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other_org)
        .bind("Comment Outsider Org")
        .bind(format!("cmt-out-{}", Uuid::new_v4().simple()))
        .execute(fixture.db.pool())
        .await
        .expect("the organization must be created");
    let other_site = Uuid::new_v4();
    let other_key = format!("out{}", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(other_site)
        .bind(other_org)
        .bind(&other_key)
        .bind("Other Site")
        .execute(fixture.db.pool())
        .await
        .expect("the site must be created");

    let (outsider_id, outsider_email) = create_account(&fixture.db, Some(other_org)).await;
    let outsider_keys: Vec<&str> = MANAGER_PERMISSIONS.to_vec();
    let mut with_manage = outsider_keys.clone();
    with_manage.push("comments.manage");
    grant(
        &fixture.db,
        other_org,
        outsider_id,
        &with_manage,
        "Outsider Manager",
    )
    .await;
    let outsider = login(&fixture.state, &fixture.db, &outsider_email).await;

    let theirs = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/comments?site_id={}&status=pending", fixture.site),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert!(
        theirs.status.is_client_error(),
        "another organization's inbox is refused: {} {}",
        theirs.status,
        theirs.body
    );
    assert!(
        !theirs.body.to_string().contains("regular@example.test"),
        "and it leaks nothing: {}",
        theirs.body
    );
    assert_ne!(fixture.org, other_org, "the two tenants are distinct");
}

/// A walk in this file that declined to run is a run that measured nothing.
///
/// Cargo reports a skipped walk as `ok` and captures the message that said so, so the summary
/// a person or a CI job reads cannot tell it apart from success. This file returns early when
/// its database cannot be opened, so that is a state it can reach; asserting the count is what
/// turns it red instead.
#[test]
fn no_walk_in_this_file_skipped() {
    assert_nothing_skipped();
}
