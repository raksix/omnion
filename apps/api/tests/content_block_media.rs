//! Integration test for the block system's media-deleted degradation (REQ-063, slice 4).
//!
//! Slice 4's last sentence is "the media-deleted degradation path", and it is the one claim in the
//! REQ that is easy to ship as silence. Everything else in the block system either refuses a
//! mistake (validation) or draws content (the renderer); this one has to do neither — the page
//! must **keep rendering** — which is exactly the property that makes a broken implementation
//! invisible. A page that loses its picture and shows a dead `<img>` is not red anywhere: it
//! looks like a page.
//!
//! So the walks are arranged around the three ways this fails:
//!
//! * **A trashed file takes the page down.** The file moves to the trash (REQ-010 keeps the row
//!   until the trash is emptied, so this is a soft delete and *not* a `delete from media`) and the
//!   public payload must still answer `200` with the rest of the tree intact. A 500 here would be
//!   a working page off the site because a file was deleted on purpose.
//!
//! * **The dead id reaches the visitor.** The degradation is asserted on the **public** payload,
//!   not on the panel's: the claim "no broken image is served" is about what a browser receives.
//!   The walk looks for the id *anywhere* in the served tree rather than at one prop, because the
//!   failure mode is a value surviving in a place nobody thought to check.
//!
//! * **"Gone" is answered with one word.** A trashed file and a purged one are both broken and
//!   only one is undoable, so the report distinguishes them and the panel's advice differs. A
//!   report that collapsed them would send an author to empty the trash for a file that is not in
//!   it.
//!
//! Two more claims that are cheap to claim and expensive to be true of:
//!
//! * **A hand-written URL is not a file.** Warning about `https://…/photo.jpg` would train
//!   authors that the panel nags about every external image, so a tree of URLs resolves to no
//!   query at all and reports nothing.
//!
//! * **The simulation writes nothing.** `?media=<id>` is how the frame answers "what if I deleted
//!   this?" without trashing a real file, and the walk proves it by naming a *live* file and then
//!   reading the media row back. A simulation that mutated state would change the page for every
//!   visitor and would be a genuinely destructive endpoint wearing a query parameter.

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
///
/// Read from the environment and **falling back to the run script's own throwaway value**, rather
/// than declaring a secret of its own. Two constants can only ever agree by luck: the token is
/// derived from the secret the *process* carries, so a suite that hard-codes one and a runner
/// that exports another produces a token the middleware refuses — and the refusal is
/// `csrf_unavailable`, which is exactly what a deployment with no secret answers. Every
/// page-creating walk in this file would then fail on the *harness* while reading as the
/// product refusing a write, so the fallback is the runner's own documented value.
fn csrf_secret() -> String {
    std::env::var("OMNION_CSRF_SECRET")
        .unwrap_or_else(|_| "qa-pass-throwaway-secret-not-a-real-key".to_owned())
}

/// What the author of this suite holds: the pages, the block registry and the library.
const OWNER_PERMISSIONS: [&str; 7] = [
    "content.pages.read",
    "content.pages.create",
    "content.pages.update",
    "content.pages.publish",
    "content.blocks.read",
    "media.read",
    "content.patterns.manage",
];

struct Auth {
    token: String,
    session_id: String,
}

struct TestResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Value,
}

/// Under `error.message`, not at the top level — a walk reading `body["message"]` against this
/// shape asserts `""`, which is a test that passes for the wrong reason on a refusal.
fn error_message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_default()
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

fn request(method: Method, uri: &str, auth: Option<&Auth>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match auth {
        Some(auth) => {
            let token = auth.token.as_str();
            builder
                .header(header::COOKIE, format!("omnion_session={token}"))
                .header(
                    CSRF_HEADER,
                    derive_csrf_token(csrf_secret().as_bytes(), &auth.session_id),
                )
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
        .header(header::USER_AGENT, "block-media-suite/1.0")
        .body(Body::empty())
        .expect("request must build")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db, IsolatedDb)> {
    let config = Config::from_env().expect("environment must be valid");
    let isolated = IsolatedDb::open(&config.database.url, 4, "content_block_media")
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
    let email = format!("block-media-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Block Media Tester".to_owned(),
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
    // The token comes from the `Set-Cookie` the handler set, never from the `sessions` table:
    // that column holds a sha256 digest, and a cookie built from it would fail to load with an
    // error that reads like a refused walk.
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
    site: Uuid,
    host: String,
    owner_email: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db, isolated) = live_state().await?;

        // The limiter runs loose before anything else, and the fact it took is asserted: without
        // this the first sign-ins hit the `sign_in` ceiling and the failure reads as a 429 in a
        // walk that never mentions rate limits.
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
            .bind("Block Media Test Org")
            .bind(format!("bm-org-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        let key = format!("bm{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&key)
            .bind("Block Media Site")
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

        let (owner_id, owner_email) = create_account(&db, org).await;
        grant(&db, org, owner_id, &OWNER_PERMISSIONS, "Block Media Owner").await;

        Some(Self {
            state,
            db,
            isolated,
            site,
            host,
            owner_email,
        })
    }

    async fn owner(&self) -> Auth {
        login(&self.state, &self.db, &self.owner_email).await
    }

    /// A live media row — same table, same CHECKs, same columns as an upload produces.
    ///
    /// The upload path needs an object store, and what these walks are about is what a page does
    /// with a row that already exists. A fixture that bypassed the schema could hide a store bug
    /// behind a row the database would never have allowed.
    async fn media(&self, filename: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into media (id, site_id, storage_key, filename, content_type, size_bytes, checksum, alt_text) \
             values ($1, $2, $3, $4, 'image/png', 1024, $5, 'the file own alt')",
        )
        .bind(id)
        .bind(self.site)
        .bind(format!("sites/{}/{id}-file.png", self.site))
        .bind(filename)
        .bind("b".repeat(64))
        .execute(self.db.pool())
        .await
        .expect("the media row must be created");
        id
    }

    /// Move a file to the trash, the way REQ-010's file manager does.
    ///
    /// A soft delete, NOT a `delete from media`: the criterion is about a file the operator
    /// removed and can undo, and deleting the row would empty the column through the FK and walk
    /// the purge case instead of the trash case.
    async fn trash(&self, media_id: Uuid) {
        let result = sqlx::query("update media set deleted_at = now() where id = $1")
            .bind(media_id)
            .execute(self.db.pool())
            .await
            .expect("the file must be trashed");
        assert_eq!(result.rows_affected(), 1, "the trash must have moved one row");
    }

    /// Delete the row entirely — a purge, and a different state from a trash.
    async fn purge(&self, media_id: Uuid) {
        let result = sqlx::query("delete from media where id = $1")
            .bind(media_id)
            .execute(self.db.pool())
            .await
            .expect("the file must be purged");
        assert_eq!(result.rows_affected(), 1, "the purge must have removed one row");
    }

    /// Create a page carrying `blocks`, published, and return its id.
    async fn published_page(&self, slug: &str, blocks: Value) -> Uuid {
        let owner = self.owner().await;
        let created = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/pages",
                Some(&owner),
                // `site_id` is named, not inferred from the session: a page belongs to a site,
                // and a create that left it to the server would be a fixture guessing which site
                // a walk means. Naming it is also what makes the public read below addressable by
                // host rather than by whichever site the account happened to be in.
                Some(json!({
                    "site_id": self.site,
                    "slug": slug,
                    "title": format!("Page {slug}"),
                })),
            ),
        )
        .await;
        assert!(
            created.status.is_success(),
            "creating {slug} answered {}: {}",
            created.status,
            created.body
        );
        let page_id = created.body["id"]
            .as_str()
            .expect("a created page carries its id")
            .parse::<Uuid>()
            .expect("the id is a uuid");

        let saved = call(
            &self.state,
            request(
                Method::PATCH,
                &format!("/api/v1/pages/{page_id}"),
                Some(&owner),
                Some(json!({ "blocks": blocks })),
            ),
        )
        .await;
        assert!(
            saved.status.is_success(),
            "saving the blocks of {slug} answered {}: {}",
            saved.status,
            saved.body
        );

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
            error_message(&published.body)
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

    /// The frame payload an author opens.
    async fn preview(&self, page_id: Uuid, query: &str) -> TestResponse {
        let owner = self.owner().await;
        call(
            &self.state,
            request(
                Method::GET,
                &format!("/api/v1/pages/{page_id}/preview{query}"),
                Some(&owner),
                None,
            ),
        )
        .await
    }
}

/// An `image` block pointing at `media_id`, with a caption the degradation can fall back to.
fn image_block(media_id: Uuid, caption: &str) -> Value {
    json!({
        "id": Uuid::new_v4().to_string(),
        "type": "image",
        "props": { "src": media_id.to_string(), "alt": "A bridge", "caption": caption },
    })
}

/// A `gallery` block over the given ids.
fn gallery_block(ids: &[Uuid]) -> Value {
    json!({
        "id": Uuid::new_v4().to_string(),
        "type": "gallery",
        "props": {
            "images": ids.iter().map(Uuid::to_string).collect::<Vec<_>>(),
            "columns": 3,
        },
    })
}

/// A `columns` container whose cells are the given block trees.
///
/// The `column` wrappers are what the validator requires and what the renderer reads one grid
/// cell per, so a walk that built the container any other way would not be testing the shape a
/// stored page actually has.
fn columns_block(cells: Vec<Value>) -> Value {
    let children: Vec<Value> = cells
        .into_iter()
        .map(|blocks| {
            json!({
                "id": Uuid::new_v4().to_string(),
                "type": "column",
                "props": {},
                "children": blocks,
            })
        })
        .collect();
    json!({
        "id": Uuid::new_v4().to_string(),
        "type": "columns",
        "props": { "columns": children.len() },
        "children": children,
    })
}

/// Whether a media id appears anywhere in a served tree.
///
/// Deliberately a whole-subtree search rather than a check of one prop: the failure being hunted
/// is a dead id surviving in a place nobody thought to look, and an assertion that only reads
/// `blocks[1].props.src` passes while the same id is still in the gallery.
fn tree_mentions(blocks: &Value, media_id: Uuid) -> bool {
    fn walk(value: &Value, wanted: &str) -> bool {
        match value {
            Value::String(text) => text == wanted,
            Value::Array(items) => items.iter().any(|item| walk(item, wanted)),
            Value::Object(map) => map.values().any(|item| walk(item, wanted)),
            _ => false,
        }
    }
    walk(blocks, &media_id.to_string())
}

/// The media refs of a frame payload, as `(path, state, advice)`.
fn media_refs(body: &Value) -> Vec<(String, String, String)> {
    body["media"]["refs"]
        .as_array()
        .map(|refs| {
            refs.iter()
                .map(|entry| {
                    (
                        entry["path"].as_str().unwrap_or_default().to_owned(),
                        entry["state"].as_str().unwrap_or_default().to_owned(),
                        entry["advice"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The first report row for a media id, as `(path, state, advice)`.
///
/// Matched on the row's own `media_id` rather than on its path. A path is a *rendering* of where
/// a file was found and it changes whenever the author reorders the page, so a helper that found
/// a row by path would report a match against a different block than the id it was asked about —
/// and the assertion after it would pass while proving something else.
fn ref_for(body: &Value, media_id: Uuid) -> (String, String, String) {
    let wanted = media_id.to_string();
    body["media"]["refs"]
        .as_array()
        .expect("the report carries a ref list")
        .iter()
        .find(|row| row["media_id"].as_str() == Some(wanted.as_str()))
        .map(|row| {
            (
                row["path"].as_str().unwrap_or_default().to_owned(),
                row["state"].as_str().unwrap_or_default().to_owned(),
                row["advice"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .unwrap_or_else(|| panic!("the report must carry a ref for {wanted}"))
}

/// A trashed file leaves the page renderable, the dead id off the wire, and the author told.
#[tokio::test]
async fn a_trashed_image_leaves_the_page_renderable_and_the_visitor_never_gets_the_dead_id() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let media_id = fx.media("hero.png").await;
    let alive_id = fx.media("second.png").await;
    let page_id = fx
        .published_page(
            "loses-an-image",
            json!([
                { "id": Uuid::new_v4().to_string(), "type": "heading", "props": { "text": "About us" } },
                image_block(media_id, "The old bridge"),
                image_block(alive_id, "The new bridge"),
            ]),
        )
        .await;

    // Before: the page renders and both files are present. A walk that only checked the state
    // *after* a deletion would pass on a payload that never carried the image at all.
    let before = fx.public_page("loses-an-image").await;
    assert!(before.status.is_success(), "the page must answer before the deletion");
    assert!(
        tree_mentions(&before.body["revision"]["blocks"], media_id),
        "the stored id must reach the visitor before the file is trashed"
    );

    fx.trash(media_id).await;

    // The page is still a page. This is the assertion that matters most: a trashed file must not
    // take a working page off the site.
    let after = fx.public_page("loses-an-image").await;
    assert!(
        after.status.is_success(),
        "a trashed image must not take the page down: {} {}",
        after.status,
        after.body
    );

    let served = &after.body["revision"]["blocks"];
    assert!(
        !tree_mentions(served, media_id),
        "the dead id must not be served: {served}"
    );
    assert!(
        tree_mentions(served, alive_id),
        "the surviving file must still be served"
    );
    // The caption is the degradation: the author wrote words for that picture, and a page that
    // silently loses them is a page that lost content rather than a page that degraded.
    assert!(
        served.to_string().contains("The old bridge"),
        "the caption the image degraded to must be on the page: {served}"
    );
    // The heading is untouched — the degradation is per block, not a rebuild of the tree.
    assert!(
        served.to_string().contains("About us"),
        "the rest of the page must be intact: {served}"
    );

    // The frame names it, with the trash's own words rather than a generic "missing".
    let frame = fx.preview(page_id, "").await;
    assert!(frame.status.is_success(), "the frame must answer");
    assert_eq!(frame.body["media_broken_count"], 1, "one file is gone");
    assert_eq!(frame.body["media_file_count"], 2, "two files are named");
    assert_eq!(frame.body["media_warning"], "1 image is gone");
    let (path, state, advice) = ref_for(&frame.body, media_id);
    assert_eq!(path, "[1].props.src", "the path has to name the block");
    assert_eq!(state, "trashed");
    assert!(
        advice.contains("trash") && advice.contains("restore"),
        "a trashed file must be undoable in the author's words: {advice:?}"
    );
    // A trash is not a broken publish: the page still renders, so the author is not blocked.
    assert!(
        frame.body["can_publish"].as_bool().unwrap_or(false),
        "a gone image must not block the publish"
    );
}

/// A purged file and a trashed one are different problems with different answers.
#[tokio::test]
async fn a_purged_file_and_a_trashed_one_are_answered_differently() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let trashed_id = fx.media("trashed.png").await;
    let purged_id = fx.media("purged.png").await;
    let page_id = fx
        .published_page(
            "two-kinds-of-gone",
            json!([
                image_block(trashed_id, "Still in the trash"),
                image_block(purged_id, "Purged for good"),
            ]),
        )
        .await;

    fx.trash(trashed_id).await;
    fx.purge(purged_id).await;

    let frame = fx.preview(page_id, "").await;
    assert!(frame.status.is_success());
    let (trash_path, trash_state, trash_advice) = ref_for(&frame.body, trashed_id);
    let (purge_path, purge_state, purge_advice) = ref_for(&frame.body, purged_id);
    assert_eq!(trash_state, "trashed");
    assert_eq!(purge_state, "purged", "a deleted row is purged, not trashed");
    assert_ne!(
        trash_advice, purge_advice,
        "the two states must not share one sentence — 'restore it' is meaningless for a purge"
    );
    assert!(trash_advice.contains("restore"));
    assert!(
        !purge_advice.contains("restore"),
        "a purged file cannot be restored and must not be offered: {purge_advice:?}"
    );
    assert_ne!(trash_path, purge_path, "each ref names its own block");

    // Both degrade, both keep the page alive, and the two captions survive as text.
    let served = fx.public_page("two-kinds-of-gone").await;
    assert!(served.status.is_success());
    let text = served.body["revision"]["blocks"].to_string();
    assert!(text.contains("Still in the trash"));
    assert!(text.contains("Purged for good"));
    assert_eq!(frame.body["media_broken_count"], 2);
    assert_eq!(frame.body["media_warning"], "2 images are gone");
}

/// A gallery keeps the files that survived, and a gallery that lost them all is not an empty grid.
#[tokio::test]
async fn a_gallery_keeps_what_survived_and_does_not_draw_an_empty_grid() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let alive = fx.media("keep.png").await;
    let gone = fx.media("drop.png").await;
    fx.published_page(
        "partial-gallery",
        json!([gallery_block(&[alive, gone])]),
    )
    .await;

    fx.trash(gone).await;

    let served = fx.public_page("partial-gallery").await;
    assert!(served.status.is_success());
    let text = served.body["revision"]["blocks"].to_string();
    assert!(
        text.contains(&alive.to_string()),
        "the surviving file must still be in the gallery: {text}"
    );
    assert!(
        !text.contains(&gone.to_string()),
        "the dead id must be out of the gallery: {text}"
    );
    // One block in, one block out: a gallery of four with one missing is still a gallery.
    assert_eq!(
        served.body["revision"]["blocks"].as_array().map(Vec::len),
        Some(1),
        "the gallery keeps its place rather than vanishing"
    );

    // Now the other half: nothing survives, so there is no gallery to draw.
    let only = fx.media("only.png").await;
    let empty_page = fx
        .published_page("empty-gallery", json!([gallery_block(&[only])]))
        .await;
    fx.trash(only).await;
    let empty = fx.preview(empty_page, "").await;
    assert!(empty.status.is_success());
    assert_eq!(
        empty.body["visible_count"], 0,
        "a gallery that lost every image draws nothing — an empty grid saying '0 images' is a lie"
    );
}

/// A container whose cells lost their files serves the layout it actually has, not the one it lost.
///
/// The renderer reads a `columns` block's grid from its `columns` PROP and draws one cell per
/// CHILD, so those two have to agree. The public route filters for the viewport and *then*
/// degrades, which means a container can lose cells twice — once to `hide_on` and once to a file
/// somebody deleted — and the degradation walk did neither half of what its sibling already did:
/// a container that kept one cell of two still shipped `columns: 2`, and one that kept none
/// shipped as an empty grid section.
///
/// Asserted on the payload a VISITOR receives, because "the page lays out correctly" is a claim
/// about what the theme receives. The unit tests next door cover the walk's rule; this one is
/// what proves the rule is on the serving path at all.
#[tokio::test]
async fn a_container_that_lost_a_cell_serves_the_layout_it_still_has() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let gone = fx.media("gone.png").await;
    fx.published_page(
        "half-empty-columns",
        json!([columns_block(vec![
            // This cell's only content was a file that is about to be trashed.
            json!([gallery_block(&[gone])]),
            json!([json!({
                "id": Uuid::new_v4().to_string(),
                "type": "text",
                "props": { "text": "This cell survives" },
            })]),
        ])]),
    )
    .await;

    // The state BEFORE the deletion, so this walk cannot pass on a payload that never carried
    // two cells in the first place.
    let before = fx.public_page("half-empty-columns").await;
    assert!(before.status.is_success());
    assert_eq!(
        before.body["revision"]["blocks"][0]["props"]["columns"].as_i64(),
        Some(2),
        "the page starts as a two-cell layout: {}",
        before.body["revision"]["blocks"]
    );

    fx.trash(gone).await;

    let served = fx.public_page("half-empty-columns").await;
    assert!(served.status.is_success());
    let block = &served.body["revision"]["blocks"][0];
    let cells = block["children"]
        .as_array()
        .expect("the container keeps its surviving cells");
    assert_eq!(cells.len(), 1, "one cell left: {block}");
    assert_eq!(
        block["props"]["columns"].as_i64(),
        Some(1),
        "the prop has to say what is drawn — a prop of 2 over one cell leaves an empty grid track: {block}"
    );

    // And the half that is not about counts: the file the visitor must not receive is gone, while
    // the text of the cell that did not use a file is untouched.
    assert!(
        !tree_mentions(&served.body["revision"]["blocks"], gone),
        "the dead id must not reach the visitor"
    );
    assert!(
        block.to_string().contains("This cell survives"),
        "degrading a file must not take a neighbouring cell with it: {block}"
    );
}

/// A container that lost every cell is not served as an empty grid section.
#[tokio::test]
async fn a_container_that_lost_every_cell_is_not_served_at_all() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let gone = fx.media("all-gone.png").await;
    fx.published_page(
        "empty-columns",
        json!([columns_block(vec![
            json!([gallery_block(&[gone])]),
            json!([gallery_block(&[gone])]),
        ])]),
    )
    .await;
    fx.trash(gone).await;

    let served = fx.public_page("empty-columns").await;
    assert!(served.status.is_success());
    let blocks = served.body["revision"]["blocks"].as_array().expect("an array");
    assert!(
        blocks.is_empty(),
        "a container with no cells is the shell of a layout with nothing in it: {}",
        served.body["revision"]["blocks"]
    );
}

/// A URL the author typed is not a file, and the platform does not nag about it.
#[tokio::test]
async fn a_hand_written_url_is_not_a_file_and_is_never_reported() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let page_id = fx
        .published_page(
            "external-images",
            json!([
                {
                    "id": Uuid::new_v4().to_string(),
                    "type": "image",
                    "props": { "src": "https://cdn.example.test/hero.jpg", "alt": "A bridge" },
                },
                {
                    "id": Uuid::new_v4().to_string(),
                    "type": "image",
                    "props": { "src": "/assets/logo.svg", "alt": "A logo" },
                },
            ]),
        )
        .await;

    let frame = fx.preview(page_id, "").await;
    assert!(frame.status.is_success());
    assert_eq!(
        frame.body["media_file_count"], 0,
        "a URL is not a media id, so there is no file to report"
    );
    assert_eq!(frame.body["media_broken_count"], 0);
    assert_eq!(frame.body["media_warning"], "");
    assert!(
        media_refs(&frame.body).is_empty(),
        "nothing to warn about means no rows at all, not a row per URL"
    );

    // And the render is untouched — the degradation must not rewrite what the author wrote.
    let served = fx.public_page("external-images").await;
    assert!(served.status.is_success());
    let text = served.body["revision"]["blocks"].to_string();
    assert!(text.contains("https://cdn.example.test/hero.jpg"));
    assert!(text.contains("/assets/logo.svg"));
}

/// The frame can ask "what if this file were gone?" without touching the library.
#[tokio::test]
async fn the_frame_simulates_a_deletion_without_touching_the_media_row() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let media_id = fx.media("simulated.png").await;
    let page_id = fx
        .published_page("simulation", json!([image_block(media_id, "A bridge at dusk")]))
        .await;

    // Before: the frame draws the image and reports no broken files.
    let plain = fx.preview(page_id, "").await;
    assert!(plain.status.is_success());
    assert_eq!(plain.body["media_broken_count"], 0);
    let drawn = plain.body["visible_blocks"].to_string();
    assert!(
        drawn.contains(&media_id.to_string()),
        "the frame draws the live file: {drawn}"
    );

    // The simulation: the same live file, named as if it were deleted.
    let simulated = fx
        .preview(page_id, &format!("?media={media_id}"))
        .await;
    assert!(simulated.status.is_success());
    assert_eq!(
        simulated.body["simulated_media"], 1,
        "the frame must say how many of the page's files it is pretending about"
    );
    assert_eq!(
        simulated.body["media_broken_count"], 1,
        "a simulated file is drawn as broken, which is the whole point"
    );
    let simulated_tree = simulated.body["visible_blocks"].to_string();
    assert!(
        !simulated_tree.contains(&media_id.to_string()),
        "the simulated draw must not carry the id: {simulated_tree}"
    );
    assert!(
        simulated_tree.contains("A bridge at dusk"),
        "the degradation is still the caption: {simulated_tree}"
    );

    // The row is untouched, and that is the assertion that makes the endpoint safe to ship: a
    // simulation that mutated state would change the page every visitor sees.
    let deleted_at: Option<time::OffsetDateTime> =
        sqlx::query_scalar("select deleted_at from media where id = $1")
            .bind(media_id)
            .fetch_one(fx.db.pool())
            .await
            .expect("the row must still be there");
    assert!(
        deleted_at.is_none(),
        "a simulation must not trash the file — it is a viewing mode, not a write"
    );

    // And the public payload is unchanged: the simulation is a panel affordance, not a state.
    let served = fx.public_page("simulation").await;
    assert!(served.status.is_success());
    assert!(
        tree_mentions(&served.body["revision"]["blocks"], media_id),
        "the visitor's page is unchanged by an author's simulation"
    );
}

/// A filter that is not a list of ids is refused by name, and a truncated one is refused outright.
#[tokio::test]
async fn a_bad_simulation_filter_is_refused_rather_than_ignored() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let media_id = fx.media("filter-target.png").await;
    let page_id = fx
        .published_page("filter-errors", json!([image_block(media_id, "A bridge")]))
        .await;

    let bad = fx.preview(page_id, "?media=not-a-uuid").await;
    assert!(
        bad.status.is_client_error(),
        "a word that is not an id is a client bug and must be refused: {}",
        bad.status
    );
    assert!(
        error_message(&bad.body).contains("not-a-uuid"),
        "the message has to name what was wrong: {}",
        error_message(&bad.body)
    );

    // Longer than the bound: refused, not truncated. A silently shortened filter would answer
    // "this file was checked" for a gallery it skipped, which is worse than a refusal.
    let long: Vec<String> = (0..200)
        .map(|_| Uuid::new_v4().to_string())
        .collect();
    let too_long = fx
        .preview(page_id, &format!("?media={}", long.join(",")))
        .await;
    assert!(
        too_long.status.is_client_error(),
        "an over-long filter must be refused: {}",
        too_long.status
    );

    // The frame still works after both refusals, so the endpoint is not wedged.
    let fine = fx.preview(page_id, "").await;
    assert!(fine.status.is_success());
}

/// A block hidden from one screen is not that screen's problem, and the report says which.
#[tokio::test]
async fn a_hidden_blocks_file_is_reported_with_the_viewport_it_draws_on() {
    let Some(mut fx) = Fixture::new().await else {
        return;
    };
    let desktop_only = fx.media("desktop.png").await;
    let phone_only = fx.media("phone.png").await;
    let blocks = json!([
        {
            "id": Uuid::new_v4().to_string(),
            "type": "image",
            "meta": { "hide_on": "mobile" },
            "props": { "src": desktop_only.to_string(), "alt": "A wide photo" },
        },
        {
            "id": Uuid::new_v4().to_string(),
            "type": "image",
            "meta": { "hide_on": "desktop" },
            "props": { "src": phone_only.to_string(), "alt": "A narrow photo" },
        },
    ]);
    let page_id = fx.published_page("hidden-images", blocks).await;
    fx.trash(desktop_only).await;
    fx.trash(phone_only).await;

    // The frame reports BOTH, because a warning that disappears when the author resizes is a
    // story that changes with the window. Each row names the viewport it belongs to, so the
    // panel can say "phones only" instead of hiding it.
    let desktop = fx.preview(page_id, "").await;
    assert!(desktop.status.is_success());
    assert_eq!(
        desktop.body["media_broken_count"], 2,
        "both files are named on this page, so both are reported"
    );
    let viewports: Vec<String> = desktop.body["media"]["refs"]
        .as_array()
        .map(|refs| {
            refs.iter()
                .map(|row| row["visible_on"].as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        viewports.contains(&"mobile".to_owned()) && viewports.contains(&"desktop".to_owned()),
        "each ref names the viewport its block draws on: {viewports:?}"
    );

    // The phone render draws neither — both blocks are hidden from it — so it carries no dead
    // id at all. A degraded block for a screen it never shows would be markup nobody asked for.
    let phone = fx.preview(page_id, "?viewport=mobile").await;
    assert!(phone.status.is_success());
    assert!(
        !tree_mentions(&phone.body["visible_blocks"], desktop_only),
        "a block hidden from phones must not be drawn on the phone frame"
    );
    let served = call(
        &fx.state,
        visitor(
            Method::GET,
            "/api/v1/public/pages/hidden-images?viewport=mobile",
            &fx.host,
        ),
    )
    .await;
    assert!(served.status.is_success());
    assert!(
        !tree_mentions(&served.body["revision"]["blocks"], phone_only),
        "a block hidden from phones must not be served to a phone"
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
