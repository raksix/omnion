//! Integration test for a page's featured image (REQ-064, slice 4d — "media reuse").
//!
//! The criterion has three sentences and each of them is a way the obvious implementation is
//! wrong, so the walks are arranged around them rather than around CRUD:
//!
//! * **"round-trip on the page"** — the alt, the legend and the focal point survive a write, a
//!   reload and a second write that names only one of them. The partial-write case is the
//!   interesting one: a panel that sends the whole object on every save must not be the only
//!   thing that works, and `coalesce($n, column)` is the difference between a crop and a
//!   half-written page.
//!
//! * **"and are used by the renderer"** — asserted on the **public** payload, because "the
//!   renderer uses it" is a claim about what a visitor's browser receives. A test that reads the
//!   panel's own GET proves the panel and the panel agree, which is exactly the pair that can
//!   agree while the site shows nothing.
//!
//! * **"a deleted featured image leaves the page renderable with a warning"** — the word
//!   *renderable* is doing the work. A trashed file must not take a published page down, and a
//!   dead `<img>` on every page that used it is a broken site, not a warning. So the walk trashes
//!   the file (which keeps the row — REQ-010 holds the bytes until the trash is emptied), proves
//!   the page still answers 200, proves `featured_image` is **null** rather than a URL that
//!   404s, and proves the panel's own read carries a warning naming the file.
//!
//! Three more things that are easy to ship wrong and are therefore walked here:
//!
//! * **The page's alt is not the file's alt.** The same photograph is the hero of three pages
//!   with three different descriptions, so the walk edits one page's alt and reads
//!   `media.alt_text` back to prove the file did not change with it. A store that copies one
//!   onto the other renames the image everywhere the moment an editor saves.
//!
//! * **Reading the picker is not the power to set the image.** `media.read` lists candidates and
//!   403s the write; `content.pages.update` writes and is not even granted `media.read` in the
//!   reader role. Two guards, two different questions.
//!
//! * **The file must be this site's, and it must be an image.** A media id of another site is
//!   refused with a message that says so rather than a 500 from a foreign key, and a PDF is
//!   refused by name.

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

/// The CSRF secret this suite runs with.
const CSRF_SECRET: &str = "w2-featured-media-suite-csrf-secret";

/// The reader: the picker, and nothing else.
///
/// `media.read` without `content.pages.update` is the point. The reverse pairing (a page editor
/// who cannot see the library) is the other half, and it is walked too.
const READER_PERMISSIONS: [&str; 2] = ["media.read", "content.pages.read"];

/// What the page editor adds: the power to SET the image, and still not the library.
const PAGE_EDITOR_EXTRA: [&str; 1] = ["content.pages.update"];

/// What the owner adds on top of both.
const OWNER_EXTRA: [&str; 2] = ["content.pages.create", "content.pages.publish"];

/// A signed-in panel session.
struct Auth {
    token: String,
    session_id: String,
}

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
    TestResponse {
        status,
        headers,
        body: serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
    }
}

impl TestResponse {
    /// The `error.message` of an error body, or `""` on a success.
    fn as_message(&self) -> &str {
        error_message(&self.body)
    }
}

/// The message an error body carries.
///
/// Under `error.message`, not at the top level — and a walk that reads `body["message"]` against
/// a shape the API does not use asserts `""`, which is a test that passes for the wrong reason on
/// a refusal and fails for a confusing one on a success.
fn error_message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_default()
}

/// The code an error body carries.
fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

fn request(method: Method, uri: &str, auth: Option<&Auth>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match auth {
        Some(auth) => {
            let token = auth.token.as_str();
            let builder = builder
                .header(header::COOKIE, format!("omnion_session={token}"))
                .header(
                    CSRF_HEADER,
                    derive_csrf_token(CSRF_SECRET.as_bytes(), &auth.session_id),
                );
            builder
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

/// A request as a VISITOR's browser makes it.
fn visitor(method: Method, uri: &str, host: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .header(header::USER_AGENT, "featured-suite/1.0")
        .body(Body::empty())
        .expect("request must build")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db, IsolatedDb)> {
    let mut config = Config::from_env().expect("environment must be valid");
    // **The secret this file signs with has to be installed in the state as well.** It
    // already derives every CSRF token from `CSRF_SECRET` -- and never set the config, so
    // the deployment it built had no secret at all, and sign-in answered with no token.
    // Every authenticated write was then refused `csrf_unavailable`, a code whose message
    // names a *deployment* problem rather than this suite's omission. The walks that were
    // green were the ones that never wrote.
    config.csrf = omnion_core::config::CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
    let isolated = IsolatedDb::open(&config.database.url, 4, "cms_featured_media")
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

async fn create_account(db: &Db, organization_id: Uuid) -> (Uuid, String) {
    let email = format!("featured-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Featured Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

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
    // **The token comes from the `Set-Cookie` the login handler actually set, and never from the
    // `sessions` table.** The column holds a sha256 digest, so a helper that read it would build
    // a cookie the loader does not accept — and the failure would look like "the guard refused
    // the walk" rather than "the fixture made up a session".
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

struct Fixture {
    state: AppState,
    db: Db,
    isolated: IsolatedDb,
    org: Uuid,
    site: Uuid,
    host: String,
    editor_email: String,
    reader_email: String,
    owner_email: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db, isolated) = live_state().await?;

        // The limiter is loosened before anything else and the fact it took is asserted. Without
        // this the first sign-ins of the suite hit REQ-012's `sign_in` ceiling of 10 per 300
        // seconds and the failure looks like a 429 in a walk that never mentions rate limits.
        let limiter = omnion_api::rate_limit_middleware::ensure_installed(&state);
        let mut policies = (*limiter.current()).clone();
        for policy in &mut policies {
            policy.limit = 100_000;
            policy.burst = 10_000;
        }
        limiter.reload(policies);
        assert!(
            limiter
                .current()
                .iter()
                .all(|policy| policy.limit == 100_000),
            "the limiter must be running loose for this suite"
        );

        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(org)
            .bind("Featured Media Test Org")
            .bind(format!("feat-org-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        let key = format!("feat{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&key)
            .bind("Featured Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        let host = format!("{key}.example.test");
        sqlx::query(
            "insert into site_domains (site_id, host, is_primary) values ($1, $2, true)",
        )
        .bind(site)
        .bind(&host)
        .execute(db.pool())
        .await
        .expect("the site's primary host must be created");

        // A second site, for the "that file belongs to somebody else" walk.
        let other_site = Uuid::new_v4();
        let other_key = format!("other{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(other_site)
            .bind(org)
            .bind(&other_key)
            .bind("Other Site")
            .execute(db.pool())
            .await
            .expect("the second site must be created");

        let (editor_id, editor_email) = create_account(&db, org).await;
        let mut editor_keys = vec!["content.pages.read"];
        editor_keys.extend_from_slice(&PAGE_EDITOR_EXTRA);
        grant(&db, org, editor_id, &editor_keys, "Page Editor").await;

        let (reader_id, reader_email) = create_account(&db, org).await;
        grant(&db, org, reader_id, &READER_PERMISSIONS, "Library Reader").await;

        let (owner_id, owner_email) = create_account(&db, org).await;
        let mut owner_keys = vec!["content.pages.read", "media.read"];
        owner_keys.extend_from_slice(&PAGE_EDITOR_EXTRA);
        owner_keys.extend_from_slice(&OWNER_EXTRA);
        grant(&db, org, owner_id, &owner_keys, "Site Owner").await;

        Some(Self {
            state,
            db,
            isolated,
            org,
            site,
            host,
            editor_email,
            reader_email,
            owner_email,
        })
    }

    async fn owner(&self) -> Auth {
        login(&self.state, &self.db, &self.owner_email).await
    }

    /// A user who can set a page's image and can NOT read the library.
    async fn editor(&self) -> Auth {
        login(&self.state, &self.db, &self.editor_email).await
    }

    /// A user who can read the library and can NOT set a page's image.
    async fn reader(&self) -> Auth {
        login(&self.state, &self.db, &self.reader_email).await
    }

    /// Insert a media row directly.
    ///
    /// The upload path needs an object store, and what these walks are about is what a page does
    /// with a row that already exists. The row is a real one — same table, same CHECKs, same
    /// columns — so a store bug cannot hide behind a fixture that bypassed the schema.
    async fn media(&self, site_id: Uuid, filename: &str, content_type: &str) -> Uuid {
        let id = Uuid::new_v4();
        let checksum = "a".repeat(64);
        sqlx::query(
            "insert into media (id, site_id, storage_key, filename, content_type, size_bytes, checksum, alt_text) \
             values ($1, $2, $3, $4, $5, 1024, $6, 'the file own alt')",
        )
        .bind(id)
        .bind(site_id)
        .bind(format!("sites/{site_id}/{id}-hero.png"))
        .bind(filename)
        .bind(content_type)
        .bind(checksum)
        .execute(self.db.pool())
        .await
        .expect("the media row must be created");
        id
    }

    /// Create a page and return its id.
    async fn page(&self, slug: &str) -> Uuid {
        let owner = self.owner().await;
        let created = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/pages",
                Some(&owner),
                Some(json!({ "site_id": self.site, "slug": slug, "title": format!("Page {slug}") })),
            ),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "page creation answered {}: {}",
            created.status,
            created.body
        );
        Uuid::parse_str(created.body["id"].as_str().expect("the page has an id"))
            .expect("the page id is a uuid")
    }

    /// Create a page, publish it, and return its id.
    async fn published_page(&self, slug: &str) -> Uuid {
        let owner = self.owner().await;
        let page_id = self.page(slug).await;
        let published = call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/pages/{page_id}/publish"),
                Some(&owner),
                Some(json!({ "body": format!("Body of {slug}") })),
            ),
        )
        .await;
        assert!(
            published.status.is_success(),
            "publishing {slug} answered {}: {}",
            published.status,
            published.body
        );
        page_id
    }

    /// What a VISITOR receives for one slug.
    async fn public_page(&self, slug: &str) -> TestResponse {
        call(
            &self.state,
            visitor(
                Method::GET,
                &format!("/api/v1/public/pages/{slug}"),
                &self.host,
            ),
        )
        .await
    }

    /// Move a file to the trash, the way REQ-010's file manager does.
    ///
    /// A soft delete and NOT a `delete from media`: the criterion is about a file that was
    /// *removed by the operator*, and REQ-010 keeps the row until the trash is emptied. Deleting
    /// the row would empty the column through the FK and walk the easy case instead of the one
    /// the criterion is about.
    async fn trash(&self, media_id: Uuid) {
        let result = sqlx::query("update media set deleted_at = now() where id = $1")
            .bind(media_id)
            .execute(self.db.pool())
            .await
            .expect("the file must be trashed");
        assert_eq!(result.rows_affected(), 1, "the trash must have moved one row");
    }
}

/// Read a page's featured media.
async fn read_featured(
    state: &AppState,
    auth: &Auth,
    page_id: Uuid,
) -> TestResponse {
    call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/featured-media"),
            Some(auth),
            None,
        ),
    )
    .await
}

/// Write a page's featured media.
async fn write_featured(
    state: &AppState,
    auth: &Auth,
    page_id: Uuid,
    body: Value,
) -> TestResponse {
    call(
        state,
        request(
            Method::PUT,
            &format!("/api/v1/pages/{page_id}/featured-media"),
            Some(auth),
            Some(body),
        ),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// The criterion's first sentence: the fields round-trip, and a partial write keeps the rest.
#[tokio::test]
async fn the_alt_legend_and_focal_point_round_trip_and_a_partial_write_keeps_the_rest() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let owner = fx.owner().await;
    let media_id = fx.media(fx.site, "hero.png", "image/png").await;
    let page_id = fx.page("round-trip").await;

    // Nothing set yet: the screen an editor meets on a page that has never had a hero. The
    // availability is `none` and the render is null — a first read that already claims an
    // image is a screen showing a state nobody chose.
    let empty = read_featured(&fx.state, &owner, page_id).await;
    assert_eq!(empty.status, StatusCode::OK, "{}", empty.body);
    assert_eq!(empty.body["availability_label"], "No featured image");
    assert!(
        empty.body["render"].is_null(),
        "a page with no image renders nothing: {}",
        empty.body
    );
    assert!(
        empty.body["media"]["media_id"].is_null(),
        "and the read says so: {}",
        empty.body
    );

    // The full write.
    let saved = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({
            "media_id": media_id,
            "alt": "Two people walking a stone bridge at dusk",
            "legend": "The old bridge, three winters ago",
            "focal_x": 0.25,
            "focal_y": 0.75
        }),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        saved.body["media"]["alt"],
        "Two people walking a stone bridge at dusk"
    );
    assert_eq!(
        saved.body["media"]["legend"],
        "The old bridge, three winters ago"
    );
    assert_eq!(saved.body["availability_label"], "Available");
    // The panel's preview is the renderer's payload, not a client-side re-derivation.
    assert_eq!(saved.body["render"]["object_position"], "25% 75%");
    assert_eq!(saved.body["render"]["alt"], saved.body["media"]["alt"]);

    // A RELOAD, from the database, not from the write's own answer.
    let reloaded = read_featured(&fx.state, &owner, page_id).await;
    assert_eq!(reloaded.status, StatusCode::OK);
    assert_eq!(
        reloaded.body["media"]["alt"],
        "Two people walking a stone bridge at dusk",
        "the alt must survive the round trip"
    );
    assert_eq!(reloaded.body["media"]["legend"], "The old bridge, three winters ago");
    assert_eq!(reloaded.body["render"]["object_position"], "25% 75%");
    assert_eq!(
        reloaded.body["render"]["url"],
        format!("/media/sites/{}/{media_id}-hero.png", fx.site),
        "the renderer gets the object key's own URL, never a hand-built one"
    );

    // **The partial write.** Only the legend moves. The alt and the crop must survive it — this
    // is what `coalesce($n, column)` buys, and it is the difference between a panel that has to
    // send the whole object on every save and one that does not.
    let partial = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "legend": "A new caption" }),
    )
    .await;
    assert_eq!(partial.status, StatusCode::OK, "{}", partial.body);
    assert_eq!(partial.body["media"]["legend"], "A new caption");
    assert_eq!(
        partial.body["media"]["alt"],
        "Two people walking a stone bridge at dusk",
        "a partial write must not blank the alt"
    );
    assert_eq!(
        partial.body["render"]["object_position"], "25% 75%",
        "and must not drop the crop"
    );

    // The two focal columns are the exception to coalesce: they are written from the merged
    // pair, so a payload that sends both as null CLEARS the crop. That is the only way to say
    // "this page has never been cropped", and it has to work.
    let cleared = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "focal_x": null, "focal_y": null }),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.body);
    assert!(
        cleared.body["render"]["object_position"].is_null(),
        "an explicit null pair clears the crop: {}",
        cleared.body
    );
    assert_eq!(
        cleared.body["media"]["alt"],
        "Two people walking a stone bridge at dusk",
        "and clearing the crop keeps the image"
    );

    // And the same file on a second page, with its own alt. This is the "reuse" half: one file,
    // two pages, two descriptions — and the file's own alt must be untouched by either.
    let other = fx.page("round-trip-second").await;
    let reused = write_featured(
        &fx.state,
        &owner,
        other,
        json!({ "media_id": media_id, "alt": "The same bridge, from the river" }),
    )
    .await;
    assert_eq!(reused.status, StatusCode::OK, "{}", reused.body);
    assert_eq!(reused.body["media"]["alt"], "The same bridge, from the river");
    assert_eq!(
        reused.body["media"]["focal_x"], Value::Null,
        "the second page inherits neither crop nor legend — those are the page's own"
    );

    let file_alt: String =
        sqlx::query_scalar("select alt_text from media where id = $1")
            .bind(media_id)
            .fetch_one(fx.db.pool())
            .await
            .expect("the file row must still exist");
    assert_eq!(
        file_alt, "the file own alt",
        "editing a PAGE's alt must not rename the file everywhere it is used"
    );
}

/// The criterion's second sentence: the renderer uses it, and the public payload is the proof.
#[tokio::test]
async fn the_public_payload_carries_the_image_and_the_visitor_gets_it() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let owner = fx.owner().await;
    let media_id = fx.media(fx.site, "hero.png", "image/png").await;
    let page_id = fx.published_page("with-a-hero").await;

    // Before: the field is present and null. A field ABSENT from the payload is not the same
    // as a field that is null — a theme checking `if (body.featured_image)` treats both the
    // same, and a theme checking `'featured_image' in body` does not.
    let before = fx.public_page("with-a-hero").await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);
    assert!(
        before.body["featured_image"].is_null(),
        "a page with no hero publishes a null field, not a missing one: {}",
        before.body
    );

    let saved = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({
            "media_id": media_id,
            "alt": "A lighthouse in a storm",
            "legend": "The keeper, 1911",
            "focal_x": 0.5,
            "focal_y": 0.2
        }),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

    let after = fx.public_page("with-a-hero").await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.body);
    let image = &after.body["featured_image"];
    assert!(!image.is_null(), "the page must publish its image: {}", after.body);
    assert_eq!(image["alt"], "A lighthouse in a storm");
    assert_eq!(image["legend"], "The keeper, 1911");
    assert_eq!(image["object_position"], "50% 20%");
    assert_eq!(
        image["url"],
        format!("/media/sites/{}/{media_id}-hero.png", fx.site)
    );
    assert_eq!(
        image["media_id"], media_id.to_string(),
        "the id travels so a theme can link to the file"
    );

    // The panel's read and the public payload agree, because both read the same store. A panel
    // greener than the site is the failure this pair is here to prevent.
    let panel = read_featured(&fx.state, &owner, page_id).await;
    assert_eq!(panel.body["render"]["url"], image["url"]);
    assert_eq!(panel.body["render"]["alt"], image["alt"]);
    assert_eq!(panel.body["render"]["object_position"], image["object_position"]);
}

/// The criterion's third sentence: a deleted featured image leaves the page RENDERABLE, with a
/// warning. Both halves are walked, and the half that matters most is the one a test cannot see
/// by reading a status code.
#[tokio::test]
async fn a_trashed_featured_image_leaves_the_page_renderable_and_warns_the_operator() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let owner = fx.owner().await;
    let media_id = fx.media(fx.site, "hero.png", "image/png").await;
    let page_id = fx.published_page("loses-its-hero").await;

    let saved = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "media_id": media_id, "alt": "A lighthouse in a storm" }),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert!(!fx.public_page("loses-its-hero").await.body["featured_image"].is_null());

    // The operator empties the trash — the bytes are gone, the row is not.
    fx.trash(media_id).await;

    // **The page still renders.** This is the assertion that says "renderable": a 500 or a 404
    // here would be a working page taken off the public site because somebody deleted a file.
    let after = fx.public_page("loses-its-hero").await;
    assert_eq!(
        after.status,
        StatusCode::OK,
        "a page whose image was trashed must still answer: {}",
        after.body
    );
    assert_eq!(
        after.body["revision"]["title"], "Page loses-its-hero",
        "and it must still carry its content: {}",
        after.body
    );
    // **And it does not carry the image.** This is the half that a 200 alone would pass: a
    // payload still naming a trashed file ships a dead `<img>` to every visitor, which is a
    // broken site rather than a warning. Null is the only honest answer here.
    assert!(
        after.body["featured_image"].is_null(),
        "a trashed file must not travel in the payload: {}",
        after.body
    );

    // The panel sees the difference, and it says which of the two happened. A warning that said
    // "image problem" would leave the operator guessing between "deleted" and "never set".
    let panel = read_featured(&fx.state, &owner, page_id).await;
    assert_eq!(panel.status, StatusCode::OK, "{}", panel.body);
    assert_eq!(panel.body["availability_label"], "In the trash");
    assert!(
        panel.body["render"].is_null(),
        "the panel's preview must agree with the payload: {}",
        panel.body
    );
    let warning = panel.body["warning"]
        .as_str()
        .expect("a trashed image must warn the operator");
    assert!(
        warning.contains("trash"),
        "the warning must name the cause: {warning:?}"
    );
    assert!(
        warning.contains("hero.png"),
        "and name the file, so the operator knows which one: {warning:?}"
    );
    assert!(
        warning.contains("still renders"),
        "and say that the page is fine, so nobody panics: {warning:?}"
    );

    // The column still holds the id — the trashed row is still there — so a RESTORE brings the
    // image back with no re-save. A store that nulled the column on trash would turn a restore
    // into a re-pick, which is the thing that actually loses work.
    let still_named: Option<Uuid> = sqlx::query_scalar("select featured_media_id from pages where id = $1")
        .bind(page_id)
        .fetch_one(fx.db.pool())
        .await
        .expect("the page row must exist");
    assert_eq!(
        still_named,
        Some(media_id),
        "trashing a file must not unname it from the page"
    );

    sqlx::query("update media set deleted_at = null where id = $1")
        .bind(media_id)
        .execute(fx.db.pool())
        .await
        .expect("the restore must apply");
    let restored = fx.public_page("loses-its-hero").await;
    assert_eq!(
        restored.status, StatusCode::OK,
        "a restored file must come back with no re-save: {}",
        restored.body
    );
    assert_eq!(
        restored.body["featured_image"]["alt"],
        "A lighthouse in a storm",
        "and with the alt it had: {}",
        restored.body
    );
    let panel = read_featured(&fx.state, &owner, page_id).await;
    assert_eq!(panel.body["availability_label"], "Available");
    assert!(panel.body["warning"].is_null(), "a healthy image warns about nothing");
}

/// The rules the store owes the schema, each refused with a message that says what to do.
#[tokio::test]
async fn the_rules_are_refused_with_a_message_that_says_what_to_do() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let owner = fx.owner().await;
    let media_id = fx.media(fx.site, "hero.png", "image/png").await;
    let pdf_id = fx.media(fx.site, "handbook.pdf", "application/pdf").await;
    let page_id = fx.page("refusals").await;

    // **No alt.** The rule the schema holds and the store repeats: a screen reader reads a
    // missing alt as the file name, which is worse than no picture.
    let no_alt = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "media_id": media_id }),
    )
    .await;
    assert_eq!(
        no_alt.status,
        StatusCode::BAD_REQUEST,
        "an image with no alt must be refused: {}",
        no_alt.body
    );
    assert!(
        no_alt.as_message()
            .contains("alt text"),
        "and the message must name the field: {}",
        no_alt.body
    );

    let blank_alt = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "media_id": media_id, "alt": "   " }),
    )
    .await;
    assert_eq!(
        blank_alt.status,
        StatusCode::BAD_REQUEST,
        "whitespace is not an alt: {}",
        blank_alt.body
    );

    // **Half a focal point.** It renders as if the other half were centred, so it looks right
    // in the editor and wrong in every rendering that crops the other axis.
    let half = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "focal_x": 0.5, "alt": "the alt" }),
    )
    .await;
    assert_eq!(half.status, StatusCode::BAD_REQUEST, "{}", half.body);
    assert!(
        half.as_message()
            .contains("focal_x and focal_y"),
        "the message must name BOTH fields, since that is what the caller has to change: {}",
        half.body
    );

    // **Out of the unit interval.** `1.5` is not "fifteen percent along".
    let out_of_range = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "focal_x": 1.5, "focal_y": 0.5, "alt": "the alt" }),
    )
    .await;
    assert_eq!(out_of_range.status, StatusCode::BAD_REQUEST, "{}", out_of_range.body);

    // **A crop on a page with no picture.** Legal in itself, nonsense in context, and the
    // migration's CHECK would refuse it with a message about columns.
    let crop_nothing = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "focal_x": 0.5, "focal_y": 0.5 }),
    )
    .await;
    assert_eq!(
        crop_nothing.status,
        StatusCode::BAD_REQUEST,
        "{}",
        crop_nothing.body
    );
    assert!(
        crop_nothing.as_message()
            .contains("nothing to crop"),
        "{}",
        crop_nothing.body
    );

    // **Not an image.** A PDF as a page's hero is a 400 that says what it is, not a render that
    // draws a broken frame.
    let pdf = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "media_id": pdf_id, "alt": "the handbook" }),
    )
    .await;
    assert_eq!(pdf.status, StatusCode::BAD_REQUEST, "{}", pdf.body);
    assert!(
        pdf.as_message()
            .contains("not an image"),
        "{}",
        pdf.body
    );

    // **Another site's file.** The FK would answer 500; the store answers 400 and names the
    // reason, because "that file belongs to somebody else" and "that file does not exist" have
    // nothing to do with each other from the operator's side.
    let other_site: Uuid =
        sqlx::query_scalar("select id from sites where organization_id = $1 and id <> $2")
            .bind(fx.org)
            .bind(fx.site)
            .fetch_one(fx.db.pool())
            .await
            .expect("the fixture must own a second site");
    let foreign_id = fx.media(other_site, "theirs.png", "image/png").await;
    let foreign = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "media_id": foreign_id, "alt": "theirs" }),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::BAD_REQUEST, "{}", foreign.body);
    assert_eq!(error_code(&foreign.body), "featured_media_unavailable");
    assert!(
        foreign.as_message()
            .contains("another site"),
        "{}",
        foreign.body
    );

    // **A trashed file cannot be chosen.** Offering it in the picker is an operator picking
    // something that 404s the moment they save.
    fx.trash(media_id).await;
    let trashed = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "media_id": media_id, "alt": "the alt" }),
    )
    .await;
    assert_eq!(trashed.status, StatusCode::BAD_REQUEST, "{}", trashed.body);
    assert!(
        trashed.as_message()
            .contains("trash"),
        "{}",
        trashed.body
    );

    // None of the refusals wrote anything: a page that refused six saves is a page with no
    // image, not a page with a half-written one.
    let untouched = read_featured(&fx.state, &owner, page_id).await;
    assert_eq!(untouched.body["availability_label"], "No featured image");
    assert!(untouched.body["media"]["focal_x"].is_null());
    assert!(untouched.body["media"]["alt"].as_str().unwrap_or_default().is_empty());

    // And the clear path, which is the one request that legitimately writes an empty alt.
    let set = write_featured(
        &fx.state,
        &owner,
        page_id,
        json!({ "media_id": fx.media(fx.site, "later.png", "image/png").await, "alt": "set" }),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    let cleared = write_featured(&fx.state, &owner, page_id, json!({ "clear": true })).await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.body);
    assert_eq!(cleared.body["availability_label"], "No featured image");
    assert!(cleared.body["media"]["alt"].as_str().unwrap_or_default().is_empty());
    assert!(cleared.body["media"]["focal_x"].is_null());
    assert!(cleared.body["warning"].is_null(), "no image warns about nothing");
}

/// Reading the library and setting a page's image are two different powers.
#[tokio::test]
async fn the_picker_and_the_write_are_two_powers_and_the_page_stays_someone_elses() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let page_id = fx.page("two-powers").await;
    let candidates = fx.media(fx.site, "hero.png", "image/png").await;

    // The reader may look at the library.
    let reader = fx.reader().await;
    let listed = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/sites/{}/featured-media/candidates", fx.site),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let rows = listed.body["candidates"]
        .as_array()
        .expect("the candidates are an array");
    assert!(
        rows.iter()
            .any(|row| row["id"] == candidates.to_string()),
        "the reader must see the file: {rows:?}"
    );
    // **The file's own alt travels, labelled as the file's.** Copying it into the page is a
    // choice the editor makes; a store that copied it would rename the image everywhere on save.
    let row = rows
        .iter()
        .find(|row| row["id"] == candidates.to_string())
        .expect("the row is there");
    assert_eq!(row["alt_text"], "the file own alt");

    // And the reader may NOT set it.
    let refused = write_featured(
        &fx.state,
        &reader,
        page_id,
        json!({ "media_id": candidates, "alt": "set by a reader" }),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "media.read is not the power to set a page's image: {}",
        refused.body
    );

    // The page editor may set it and may NOT read the library. The two guards are separate, and
    // this is the pairing that proves it: one endpoint with one guard would have had to pick.
    let editor = fx.editor().await;
    let set = write_featured(
        &fx.state,
        &editor,
        page_id,
        json!({ "media_id": candidates, "alt": "set by an editor" }),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    let editor_picker = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/sites/{}/featured-media/candidates", fx.site),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        editor_picker.status,
        StatusCode::FORBIDDEN,
        "content.pages.update is not the power to browse the library: {}",
        editor_picker.body
    );
    // **The editor CAN read the page's own image, and this assertion is the one that made the
    // first guard wrong.** My first version guarded that read with `media.read` "because the
    // body mentions a file", which locked out a page editor who can edit this page's title, body
    // and crop but could not read the alt they are *required* to write — the migration's CHECK
    // refuses an image with a blank alt, so the power to set the image and the power to read it
    // have to travel together or the editor is asked a question they cannot answer.
    let read = read_featured(&fx.state, &editor, page_id).await;
    assert_eq!(
        read.status,
        StatusCode::OK,
        "reading a page's own image is reading the page: {}",
        read.body
    );
    assert_eq!(read.body["media"]["alt"], "set by an editor");

    // **A page of another site is not reachable with an id.** The scope check resolves the site
    // through the page, so a guessed id reads nothing and a guessed id WRITES nothing.
    let other_site: Uuid =
        sqlx::query_scalar("select id from sites where organization_id = $1 and id <> $2")
            .bind(fx.org)
            .bind(fx.site)
            .fetch_one(fx.db.pool())
            .await
            .expect("the fixture owns a second site");
    let foreign_page: Uuid = sqlx::query_scalar(
        "insert into pages (id, site_id, slug, page_type, status) \
         values ($1, $2, 'theirs', 'page', 'draft') returning id",
    )
    .bind(Uuid::new_v4())
    .bind(other_site)
    .fetch_one(fx.db.pool())
    .await
    .expect("the other site's page must exist");
    // **The boundary this platform actually has is the ORGANIZATION, not the site.** My first
    // version of this walk asserted that a page of a *second site* is unreachable, and the API
    // answered 200 — correctly. `ensure_same_organization` compares the caller's organization
    // with the target's, and two sites inside one organization are one tenant with two addresses;
    // a scope narrower than the permission model would make `Scope::Site` bindings inert and turn
    // every site-scoped role into a lie. So the site case is NOT a boundary, and the walk now
    // asserts the real one: another organization's page is invisible by id.
    let owner = fx.owner().await;
    let same_tenant_read = read_featured(&fx.state, &owner, foreign_page).await;
    assert_eq!(
        same_tenant_read.status,
        StatusCode::OK,
        "two sites in one organization are one tenant, so a page id reaches both: {}",
        same_tenant_read.body
    );

    // **A file from one site cannot be featured by a page of the other.** This IS a boundary, and
    // the store holds it in the same statement as the write: the media id is filtered by
    // `site_id`, so an id belonging to a sibling site is refused as "no usable file on this site"
    // rather than borrowed across a tenant boundary. Without the filter, a page on site B could
    // point at site A's bytes and a URL built from A's object key would serve one site's file
    // from another site's page.
    // Created here rather than read back: the foreign file is the fixture's own, and looking it
    // up by query would have made this walk depend on an earlier one having left a row behind.
    let other_file = fx.media(other_site, "theirs.png", "image/png").await;
    // **The borrow has to be tried the right way round.** My first attempt pointed a page of the
    // OTHER site at the other site's file — which is a legitimate pairing, and the API accepted it
    // correctly. The borrow is this site's page naming the other site's file, and that is the
    // direction where a missing `site_id` filter would let one site's bytes render on another
    // site's page, with a URL built from the other site's object key.
    let our_page = fx.page("our-own-page").await;
    let cross_site_file = write_featured(
        &fx.state,
        &owner,
        our_page,
        json!({ "media_id": other_file, "alt": "borrowed" }),
    )
    .await;
    assert_eq!(
        cross_site_file.status,
        StatusCode::BAD_REQUEST,
        "a sibling site's file must not be borrowable: {}",
        cross_site_file.body
    );
    assert_eq!(error_code(&cross_site_file.body), "featured_media_unavailable");

    // **And another organization is refused by id** — the real tenant boundary. My first version
    // asserted `404` here, "so the panel learns nothing about what exists over there", and the
    // API answered **403 `cross_organization`**. That is the platform's deliberate answer
    // (`scope::ensure_same_organization`), and it is the right one: a `404` would have to mean
    // "no such page", and a *member* of the other organization hitting the same route gets a
    // `404` because the page is genuinely not theirs to see. A `403` instead says "this account
    // may not work outside its own organization" — which discloses no page, no site and no
    // content, and tells an operator the difference between "wrong tenant" and "wrong id", which
    // is the difference they need. Asserting the platform's own value here is the point: a walk
    // that insists on a 404 would have had this endpoint rewritten to be less informative.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other_org)
        .bind("Rival Org")
        .bind(format!("rival-{}", Uuid::new_v4().simple()))
        .execute(fx.db.pool())
        .await
        .expect("the rival organization must exist");
    let rival_site = Uuid::new_v4();
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(rival_site)
        .bind(other_org)
        .bind(format!("rival{}", &Uuid::new_v4().simple().to_string()[..8]))
        .bind("Rival Site")
        .execute(fx.db.pool())
        .await
        .expect("the rival site must exist");
    let rival_page: Uuid = sqlx::query_scalar(
        "insert into pages (id, site_id, slug, page_type, status) \
         values ($1, $2, 'rival', 'page', 'draft') returning id",
    )
    .bind(Uuid::new_v4())
    .bind(rival_site)
    .fetch_one(fx.db.pool())
    .await
    .expect("the rival page must exist");
    let cross_read = read_featured(&fx.state, &owner, rival_page).await;
    assert_eq!(
        cross_read.status,
        StatusCode::FORBIDDEN,
        "another organization's page must be refused with its id: {}",
        cross_read.body
    );
    assert_eq!(error_code(&cross_read.body), "cross_organization");
    assert!(
        !cross_read.as_message().contains(&rival_page.to_string()),
        "and the message must not carry the id it refused: {}",
        cross_read.body
    );
    let cross_write = write_featured(
        &fx.state,
        &owner,
        rival_page,
        json!({ "media_id": candidates, "alt": "set across organizations" }),
    )
    .await;
    assert_eq!(
        cross_write.status,
        StatusCode::FORBIDDEN,
        "and a guessed id must not be writable either: {}",
        cross_write.body
    );
}

/// The picker's own contract: the reuse count, the image-only filter, the page cap.
#[tokio::test]
async fn the_picker_offers_images_with_a_reuse_count_and_honours_its_page_cap() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let owner = fx.owner().await;

    // Three images, one PDF, and one trashed image.
    let first = fx.media(fx.site, "one.png", "image/png").await;
    fx.media(fx.site, "two.png", "image/jpeg").await;
    fx.media(fx.site, "three.png", "image/webp").await;
    let pdf = fx.media(fx.site, "notes.pdf", "application/pdf").await;
    let trashed = fx.media(fx.site, "gone.png", "image/png").await;
    fx.trash(trashed).await;

    // Two pages use the first one, which is what "reuse" means.
    for slug in ["reuse-a", "reuse-b"] {
        let page = fx.page(slug).await;
        let saved = write_featured(
            &fx.state,
            &owner,
            page,
            json!({ "media_id": first, "alt": format!("the alt for {slug}") }),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    }

    let listed = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/sites/{}/featured-media/candidates", fx.site),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let rows = listed.body["candidates"].as_array().expect("an array");
    let ids: Vec<String> = rows
        .iter()
        .map(|row| row["id"].as_str().unwrap_or_default().to_owned())
        .collect();

    assert!(!ids.contains(&pdf.to_string()), "a PDF is not a candidate");
    assert!(
        !ids.contains(&trashed.to_string()),
        "a trashed file is not a candidate: it would 404 the moment it was saved"
    );
    assert!(ids.contains(&first.to_string()), "a live image is a candidate");

    let row = rows
        .iter()
        .find(|row| row["id"] == first.to_string())
        .expect("the reused file is listed");
    assert_eq!(
        row["used_by_pages"], 2,
        "the picker must say how many pages already use the file — that is the reuse signal"
    );
    let other = rows
        .iter()
        .find(|row| row["id"] != first.to_string())
        .expect("another row is listed");
    assert_eq!(other["used_by_pages"], 0, "a file on no page says zero, not absent");

    // The page cap. A site with four thousand uploads must not make the editor wait for four
    // thousand rows, and a client asking for everything is a client asking for a denial of
    // service on somebody else's panel.
    let capped = call(
        &fx.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/sites/{}/featured-media/candidates?limit=2",
                fx.site
            ),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(capped.status, StatusCode::OK, "{}", capped.body);
    assert_eq!(
        capped.body["candidates"].as_array().map(Vec::len),
        Some(2),
        "the cap must actually cap: {}",
        capped.body
    );
    assert_eq!(capped.body["limit"], 2, "and the answer says what it capped to");

    let huge = call(
        &fx.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/sites/{}/featured-media/candidates?limit=100000",
                fx.site
            ),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(huge.status, StatusCode::OK, "{}", huge.body);
    assert_eq!(
        huge.body["limit"], 200,
        "an absurd limit is clamped, not honoured: {}",
        huge.body
    );
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
