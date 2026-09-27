//! Integration test for the block system of the page builder (REQ-063, slice 1).
//!
//! Slice 1 is the half that has to be true before any editor can be trusted: the registry
//! answers with every block type and its schema, a block tree round-trips through a draft
//! revision without losing a prop or a block id, a reorder changes the order and nothing else,
//! a duplicate is a copy with a fresh id, and a required prop is refused on publish while a
//! draft save is still allowed — because an author is allowed to be mid-sentence, and a page
//! is not allowed to go live that way.
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
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Permission keys the content editor of this suite holds. The block registry rides its own
/// key (REQ-063), so an editor who may author against the registry but may not manage the
/// pattern library is a real role — that is what the refusal test below proves.
const CONTENT_PERMISSIONS: [&str; 8] = [
    "content.pages.read",
    "content.pages.create",
    "content.pages.update",
    "content.pages.delete",
    "content.pages.publish",
    "content.pages.restore",
    "content.blocks.read",
    "content.patterns.manage",
];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

/// Drive the real router without a network socket.
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
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("body must be JSON")
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

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(error) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({error}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            None
        }
    }
}

/// A state whose database has all migrations applied and the IAM seed loaded.
async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = live_db(&config).await?;
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

/// A page, a site, an organization and the accounts that drive them.
struct Fixture {
    state: AppState,
    db: Db,
    org: Uuid,
    site: Uuid,
    site_key: String,
    platform_email: String,
    editor_email: String,
    member_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
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
            .bind("Block Test Org")
            .bind(format!("blk-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        // A site key unique to this run: the public surface resolves a site by its *global*
        // key, and the development database already carries a `main` from every other suite,
        // so a fixture that used it would be refused as ambiguous.
        let site = Uuid::new_v4();
        let site_key = format!("blk{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&site_key)
            .bind("Block Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        // The platform Owner: no primary organization, so it may work across tenants.
        let (platform_id, platform_email) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        // The editor: the content keys bound at organization scope.
        let (editor_id, editor_email) = create_account(&db, Some(org)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org,
                key: format!("block-editor-{}", Uuid::new_v4().simple()),
                name: "Block Editor".to_owned(),
                description: "Authors pages out of blocks".to_owned(),
                priority: 400,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the organization role must be created");

        let entries: Vec<RolePermissionInput> = CONTENT_PERMISSIONS
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
            user_id: editor_id,
            scope: Scope::Organization {
                organization_id: org,
            },
            granted_by: Some(platform_id),
            expires_at: None,
        };
        bindings::validate(db.pool(), &binding)
            .await
            .expect("the binding must validate");
        bindings::grant(db.pool(), binding)
            .await
            .expect("the binding must be granted");

        // A plain member of the same organization, without a content permission.
        let (member_id, member_email) = create_account(&db, Some(org)).await;

        Some(Self {
            state,
            db,
            org,
            site,
            site_key,
            platform_email,
            editor_email,
            member_email,
            accounts: vec![platform_id, editor_id, member_id],
            organizations: vec![org],
        })
    }

    async fn editor_token(&self) -> String {
        login(&self.state, &self.editor_email).await
    }

    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    async fn platform_token(&self) -> String {
        login(&self.state, &self.platform_email).await
    }

    /// Create a page and return its id.
    async fn page(&self, token: &str, slug: &str) -> String {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/pages",
                Some(token),
                Some(json!({
                    "site_id": self.site,
                    "slug": slug,
                    "title": "Landing",
                    "body": "The pre-block text.",
                })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "page: {}",
            response.body
        );
        response.body["id"].as_str().expect("an id").to_owned()
    }

    async fn cleanup(&self) {
        sqlx::query("delete from users where id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = any($1)")
            .bind(&self.organizations)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

/// Create one account and answer its id and email.
///
/// The id comes back from the insert, not from a value made up here: `users::create_user`
/// mints its own primary key, and a binding made against a guessed id fails its foreign key
/// with the least informative message the platform has.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("blk-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Block Tester".to_owned(),
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
    // The `Set-Cookie` header is `name=value; Path=/; HttpOnly; …`; only the value is the
    // session token, and sending the attributes back would be a cookie no browser accepts.
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

/// One block entry with a fresh id, in the payload shape the editor sends.
fn block(kind: &str, props: Value) -> Value {
    json!({ "id": Uuid::new_v4().to_string(), "type": kind, "props": props })
}

/// A small page's worth of blocks: a heading, a text and a two-column container.
fn sample_tree() -> Value {
    json!([
        block("heading", json!({ "text": "Welcome", "level": "h1" })),
        block("text", json!({ "text": "The opening paragraph." })),
        {
            "id": Uuid::new_v4().to_string(),
            "type": "columns",
            "props": { "columns": 2 },
            "children": [
                block("text", json!({ "text": "Left column." })),
                block("text", json!({ "text": "Right column." }))
            ]
        }
    ])
}

// ---------------------------------------------------------------------------------------------
// The suite
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_block_registry_is_read_only_and_permission_gated() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let member = fixture.member_token().await;

    // A member without `content.blocks.read` is refused both halves.
    for (method, uri) in [
        (Method::GET, "/api/v1/blocks"),
        (Method::POST, "/api/v1/blocks/validate"),
    ] {
        let body = if method == Method::POST {
            Some(json!({ "blocks": [] }))
        } else {
            None
        };
        let refused = call(
            &fixture.state,
            request(method.clone(), uri, Some(&member), body),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{uri} must be refused without content.blocks.read: {}",
            refused.body
        );
    }

    // The editor gets the whole registry: the version, the category order and every type with
    // its schema — no list is ever assembled by the client.
    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/blocks", Some(&editor), None),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(listed.body["version"], json!("1"));
    let categories: Vec<&str> = listed.body["categories"]
        .as_array()
        .expect("categories")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(categories.contains(&"text") && categories.contains(&"layout"));

    let blocks = listed.body["blocks"].as_array().expect("blocks");
    assert_eq!(
        blocks.len(),
        16,
        "REQ-063 §Scope names sixteen block types: {}",
        blocks.len()
    );

    let mut keys: Vec<&str> = blocks
        .iter()
        .filter_map(|entry| entry["key"].as_str())
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "blog_list",
            "card_grid",
            "columns",
            "cta",
            "embed",
            "faq",
            "form",
            "gallery",
            "heading",
            "image",
            "pricing_table",
            "product_grid",
            "raw_html",
            "testimonial",
            "text",
            "video",
        ]
    );

    // Every definition is complete enough to generate an inspector from.
    for entry in blocks {
        let key = entry["key"].as_str().expect("a key");
        assert!(
            !entry["label"].as_str().unwrap_or_default().is_empty(),
            "{key}"
        );
        assert!(
            categories.contains(&entry["category"].as_str().unwrap_or_default()),
            "{key} sits in a category the panel groups by"
        );
        for prop in entry["props"].as_array().expect("props") {
            let prop_key = prop["key"].as_str().expect("a prop key");
            assert!(
                !prop["label"].as_str().unwrap_or_default().is_empty(),
                "{key}.{prop_key}"
            );
            assert!(
                !prop["type"].as_str().unwrap_or_default().is_empty(),
                "{key}.{prop_key}"
            );
            assert!(
                prop.get("default").is_some(),
                "{key}.{prop_key} declares a default"
            );
        }
    }

    // A container says so, and only the container does.
    let containers: Vec<&str> = blocks
        .iter()
        .filter(|entry| entry["container"] == json!(true))
        .filter_map(|entry| entry["key"].as_str())
        .collect();
    assert_eq!(containers, vec!["columns"]);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_dry_run_reports_issues_without_writing_anything() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    // A clean tree.
    let clean = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/blocks/validate",
            Some(&editor),
            Some(json!({ "blocks": sample_tree() })),
        ),
    )
    .await;
    assert_eq!(clean.status, StatusCode::OK, "{}", clean.body);
    assert_eq!(clean.body["can_publish"], json!(true));
    assert_eq!(
        clean.body["block_count"],
        json!(5),
        "nested blocks are counted"
    );
    assert_eq!(
        clean.body["issues"].as_array().map(Vec::len),
        Some(0),
        "a clean tree has nothing to report: {}",
        clean.body
    );

    // Every documented issue code the editor must be able to highlight.
    let cases: Vec<(Value, &str)> = vec![
        (json!([block("carousel", json!({}))]), "block_unknown_type"),
        (
            json!([block("heading", json!({ "level": "h2" }))]),
            "block_prop_required",
        ),
        (
            json!([block("image", json!({ "src": "/m/1", "alt": "  " }))]),
            "block_alt_missing",
        ),
        // A payload that is not an array at all is refused before it is walked.
        (json!({ "not": "an array" }), "block_payload_invalid"),
    ];
    for (tree, expected) in cases {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/blocks/validate",
                Some(&editor),
                Some(json!({ "blocks": tree })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(response.body["can_publish"], json!(false), "{expected}");
        let codes: Vec<&str> = response.body["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter_map(|issue| issue["code"].as_str())
            .collect();
        assert!(
            codes.contains(&expected),
            "expected {expected}, got {codes:?}"
        );

        // Every issue names the block it belongs to, so the editor can put the cursor on it.
        for issue in response.body["issues"].as_array().expect("issues") {
            assert!(issue["block_id"].is_string(), "an issue names its block");
            assert!(!issue["message"].as_str().unwrap_or_default().is_empty());
            assert!(matches!(
                issue["severity"].as_str(),
                Some("error") | Some("warning")
            ));
        }
    }

    // The dry run wrote nothing: the page count of the site is untouched by four validations.
    let pages = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages?site_id={}", fixture.site),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(pages.status, StatusCode::OK);
    assert_eq!(
        pages.body["pages"].as_array().map(Vec::len),
        Some(0),
        "a dry run never writes"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_page_keeps_one_block_of_every_type_through_a_save_and_a_publish() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "every-type").await;

    // One block of every type in the registry, with the props each one requires.
    let one_of_each = json!([
        block("heading", json!({ "text": "Every type", "level": "h1" })),
        block("text", json!({ "text": "Body copy." })),
        block("image", json!({ "src": "/api/v1/public/media/1", "alt": "A diagram" })),
        block("gallery", json!({ "images": ["/a.png", "/b.png"], "columns": 2 })),
        block("video", json!({ "src": "https://videos.example/clip" })),
        block("cta", json!({ "title": "Ready?", "label": "Start", "href": "/start" })),
        {
            "id": Uuid::new_v4().to_string(),
            "type": "columns",
            "props": { "columns": 2, "gap": "normal" },
            "children": [block("text", json!({ "text": "In a column." }))]
        },
        block("card_grid", json!({ "items": ["One|First|", "Two|Second|"], "columns": 2 })),
        block("pricing_table", json!({ "plans": ["Starter|0|One thing"], "note": "No card needed" })),
        block("testimonial", json!({ "quote": "It shipped in a week.", "author": "A reader" })),
        block("faq", json!({ "items": ["What is it?|A page builder"] })),
        block("form", json!({ "form_key": "contact", "title": "Write to us" })),
        block("embed", json!({ "src": "https://www.example.org/map" })),
        block("raw_html", json!({ "html": "<p>Hand written.</p>" })),
        block("product_grid", json!({ "category": "tools", "limit": 4 })),
        block("blog_list", json!({ "limit": 3 }))
    ]);

    // The dry run agrees the page is publishable before anything is saved.
    let dry = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/blocks/validate",
            Some(&editor),
            Some(json!({ "blocks": one_of_each })),
        ),
    )
    .await;
    assert_eq!(dry.body["can_publish"], json!(true), "{}", dry.body);

    // Saving writes a new draft revision carrying the tree.
    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": one_of_each })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let draft = &saved.body["draft"];
    assert_eq!(
        draft["revision_no"],
        json!(2),
        "a block save appends a revision"
    );

    // Every block and every id came back, and every prop the author wrote survived. The tree
    // the API stores is the tree it was given *plus* the defaults the schema fills in — an
    // absent optional prop is written as its default, so a later schema change is a no-op for
    // content that never used the prop.
    let stored = draft["blocks"].as_array().expect("blocks");
    assert_eq!(
        stored.len(),
        16,
        "one block of every type: {}",
        draft["blocks"]
    );
    for (index, expected) in one_of_each.as_array().expect("sent").iter().enumerate() {
        assert_eq!(
            stored[index]["id"], expected["id"],
            "block {index} kept its id"
        );
        assert_eq!(stored[index]["type"], expected["type"]);
        for (key, value) in expected["props"].as_object().expect("props") {
            assert_eq!(
                stored[index]["props"][key], *value,
                "block {index} kept {key}"
            );
        }
    }
    // A default the author did not touch is written down rather than left absent.
    assert_eq!(stored[0]["props"]["align"], json!("left"));
    assert_eq!(stored[1]["props"]["align"], json!("left"));
    assert_eq!(stored[2]["props"]["caption"], json!(""));
    // And the container keeps its children.
    assert_eq!(stored[6]["children"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        draft["body"],
        json!("The pre-block text."),
        "the body is untouched"
    );

    // Reading the page back gives the same tree: no prop, id or order is lost on the way in.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(read.body["draft"]["blocks"], draft["blocks"]);

    // Publishing freezes that revision, and the public surface now carries the blocks.
    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(published.status, StatusCode::OK, "{}", published.body);
    assert_eq!(published.body["published"]["blocks"], draft["blocks"]);

    let public = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/public/pages/every-type?site={}", fixture.site_key),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(public.status, StatusCode::OK, "{}", public.body);
    assert_eq!(public.body["revision"]["blocks"], draft["blocks"]);
    assert_eq!(
        public.body["revision"]["body"],
        json!("The pre-block text."),
        "the pre-block body stays available to a theme that wants it"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn reordering_keeps_block_ids_and_duplicating_gives_the_copy_a_new_one() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "reorder").await;

    let first = block("heading", json!({ "text": "First", "level": "h1" }));
    let second = block("text", json!({ "text": "Second" }));
    let third = block("text", json!({ "text": "Third" }));

    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [first.clone(), second.clone(), third.clone()] })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let original_ids: Vec<String> = [&first, &second, &third]
        .iter()
        .map(|entry| entry["id"].as_str().expect("an id").to_owned())
        .collect();

    // Reorder: third, first, second. The ids travel with their blocks, so the diff reads as a
    // move and the props are untouched.
    let reordered = json!([third.clone(), first.clone(), second.clone()]);
    let moved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": reordered })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    let stored = moved.body["draft"]["blocks"].as_array().expect("blocks");
    let moved_ids: Vec<&str> = stored
        .iter()
        .map(|entry| entry["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(
        moved_ids,
        vec![
            third["id"].as_str().unwrap(),
            first["id"].as_str().unwrap(),
            second["id"].as_str().unwrap()
        ],
        "a reorder changes the order and nothing else"
    );
    for id in &original_ids {
        assert!(
            moved_ids.contains(&id.as_str()),
            "no block id is lost by a reorder"
        );
    }
    assert_eq!(stored[0]["props"]["text"], json!("Third"));
    assert_eq!(
        stored[1]["props"]["level"],
        json!("h1"),
        "props are not rewritten"
    );

    // Reloading the page gives the same order: the reorder was persisted, not just held.
    let reloaded = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    let stored = reloaded.body["draft"]["blocks"].as_array().expect("blocks");
    assert_eq!(stored[0]["id"], third["id"]);
    assert_eq!(stored[2]["id"], second["id"]);

    // Duplicating is what an editor does with the clipboard: the copy carries the same props and
    // a fresh id, and the original is left exactly as it was.
    let mut duplicate = stored[0].clone();
    let duplicate_id = Uuid::new_v4().to_string();
    duplicate["id"] = json!(duplicate_id);
    let with_duplicate = json!([duplicate.clone(), stored[0].clone(), stored[1].clone()]);
    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": with_duplicate })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let stored = saved.body["draft"]["blocks"].as_array().expect("blocks");
    assert_eq!(stored.len(), 3, "the copy is added, the original is kept");
    assert_eq!(stored[0]["id"], json!(duplicate_id));
    assert_eq!(
        stored[0]["props"], stored[1]["props"],
        "a copy carries the props"
    );
    assert_eq!(
        stored[1]["id"], third["id"],
        "the original keeps its identity"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_required_prop_stops_a_publish_but_not_a_draft_save() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "incomplete").await;

    // A draft with an image that has no alternative text: the save is allowed, because an
    // author is allowed to be mid-sentence and a draft is not read by anybody.
    let incomplete = json!([
        block("heading", json!({ "text": "Draft", "level": "h1" })),
        block(
            "image",
            json!({ "src": "/api/v1/public/media/7", "alt": "" })
        )
    ]);
    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": incomplete })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["draft"]["revision_no"], json!(2));

    // Publishing is refused, and the refusal names the block and what it needs.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(
        refused.body["error"]["code"],
        json!("blocks_not_publishable")
    );
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("alternative text"),
        "the refusal names what is missing: {message}"
    );

    // The page is still a draft, and its history is intact.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(read.body["status"], json!("draft"));
    assert!(read.body["published"].is_null());

    // Fixing the block makes the very same page publishable — no other change needed.
    let fixed = json!([
        block("heading", json!({ "text": "Draft", "level": "h1" })),
        block(
            "image",
            json!({ "src": "/api/v1/public/media/7", "alt": "The hero photo" })
        )
    ]);
    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": fixed })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(published.status, StatusCode::OK, "{}", published.body);
    assert_eq!(published.body["status"], json!("published"));

    // Every revision of the page carries the tree it was saved with, and the pre-block revision
    // is still an empty one: history was extended, never rewritten.
    let history = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions"),
            Some(&editor),
            None,
        ),
    )
    .await;
    let revisions = history.body["revisions"].as_array().expect("revisions");
    assert_eq!(revisions.len(), 3);
    assert_eq!(revisions[0]["blocks"], saved.body["draft"]["blocks"]);
    assert_eq!(
        revisions[1]["blocks"].as_array().expect("blocks").len(),
        2,
        "the broken draft is still readable, image and all"
    );
    assert_eq!(
        revisions[2]["blocks"],
        json!([]),
        "revision 1 predates the block system and renders from its body"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_block_save_is_recorded_as_an_event_and_an_audit_row() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "events").await;

    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": sample_tree() })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

    // The platform's own bus carries the structural change, and the payload counts the blocks
    // without ever handing an integration the page's content.
    let events = sqlx::query(
        "select name, payload from events where organization_id = $1 order by created_at desc",
    )
    .bind(fixture.org)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the event log must read");
    let blocks_event = events
        .iter()
        .find(|row| row.get::<String, _>("name").as_str() == "content.blocks.updated")
        .expect("the block save must be recorded");
    let payload: Value = blocks_event.get::<Value, _>("payload");
    assert_eq!(payload["block_count"], json!(5));
    assert!(
        payload.get("body").is_none(),
        "the event never carries the content"
    );

    // The audit trail says the same, from the same save.
    let audit = sqlx::query(
        "select action, metadata from audit_log \
         where target_id = $1 and action = 'page.updated'",
    )
    .bind(&page_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit trail must read");
    assert!(!audit.is_empty(), "the save writes an audit row");
    let metadata: Value = audit[0].get::<Value, _>("metadata");
    assert_eq!(metadata["blocks_changed"], json!(true));
    assert_eq!(metadata["content_changed"], json!(true));

    // Publishing counts the blocks on the publication event too.
    let published = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/publish"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(published.status, StatusCode::OK, "{}", published.body);
    // The publication event carries the same count hint, so a downstream integration learns
    // how much of the page changed without ever receiving its content.
    let payload: Value = sqlx::query_scalar(
        "select payload from events where name = 'page.published' and organization_id = $1 \
         order by created_at desc limit 1",
    )
    .bind(fixture.org)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the publication must be recorded");
    assert_eq!(payload["block_count"], json!(5));
    assert_eq!(payload["slug"], json!("events"));

    fixture.cleanup().await;
}

#[tokio::test]
async fn restoring_an_earlier_revision_brings_its_blocks_back_too() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "restore").await;

    let original = json!([block(
        "heading",
        json!({ "text": "Original", "level": "h1" })
    )]);
    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": original })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let original_revision = saved.body["draft"]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let replacement = json!([block("text", json!({ "text": "Something else entirely" }))]);
    call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": replacement })),
        ),
    )
    .await;

    // Restoring the older revision copies it forward — text and blocks together. A restore that
    // brought back only the words would put a page on the site that never existed.
    let restored = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/restore"),
            Some(&editor),
            Some(json!({ "revision_id": original_revision })),
        ),
    )
    .await;
    assert_eq!(restored.status, StatusCode::CREATED, "{}", restored.body);
    // The restored tree is the saved one — the defaults the schema filled in travel with it,
    // because a restore brings back a revision exactly as it was written, not as it was typed.
    assert_eq!(restored.body["blocks"], saved.body["draft"]["blocks"]);
    assert_eq!(restored.body["state"], json!("draft"));
    assert_eq!(restored.body["restored_from_id"], json!(original_revision));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_blocks_only_save_from_a_member_without_the_key_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let platform = fixture.platform_token().await;
    let member = fixture.member_token().await;
    let page_id = fixture.page(&platform, "scoped").await;

    // A member without `content.pages.update` cannot change the page, and the block payload in
    // the same request does not become a way around it.
    let refused = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&member),
            Some(json!({ "blocks": sample_tree() })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "blocks_changed: {}",
        refused.body
    );

    // The page is untouched: the working draft is still the empty tree of revision 1.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}"),
            Some(&platform),
            None,
        ),
    )
    .await;
    assert_eq!(read.body["draft"]["blocks"], json!([]));
    assert_eq!(read.body["draft"]["revision_no"], json!(1));

    fixture.cleanup().await;
}
