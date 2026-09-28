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

/// One `column` wrapper holding a block or two, in the shape slice 2 made required.
///
/// A Columns block holds columns, not content: "two of these side by side" is only expressible
/// if each column is its own node. Every fixture below that builds a container has to go through
/// this helper, or it writes a payload the validator refuses and the test fails for a reason
/// that has nothing to do with what it was written to prove.
fn column(blocks: Vec<Value>) -> Value {
    json!({
        "id": Uuid::new_v4().to_string(),
        "type": "column",
        "props": { "align": "left" },
        "children": blocks
    })
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
                column(vec![block("text", json!({ "text": "Left column." }))]),
                column(vec![block("text", json!({ "text": "Right column." }))])
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
    // "2" is the version that added the `column` wrapper. The panel compares against it to tell
    // a new block type from a misspelled one, so the assertion is on the value, not on "it is
    // a string" — a version that never moves is a version nothing reads.
    assert_eq!(listed.body["version"], json!("2"));
    let categories: Vec<&str> = listed.body["categories"]
        .as_array()
        .expect("categories")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(categories.contains(&"text") && categories.contains(&"layout"));

    let blocks = listed.body["blocks"].as_array().expect("blocks");
    // Seventeen, not the sixteen §Scope named: slice 2 added `column`, the wrapper a Columns
    // block holds. The count is asserted as a number so that adding an eighteenth type without
    // updating this line is a failure rather than a silently longer registry.
    assert_eq!(
        blocks.len(),
        17,
        "sixteen block types plus the column wrapper: {}",
        blocks.len()
    );
    let column_entry = blocks
        .iter()
        .find(|entry| entry["key"] == json!("column"))
        .expect("the column wrapper is registered");
    assert_eq!(
        column_entry["structure_only"],
        json!(true),
        "a block that only exists inside another container is not offered as an insert choice"
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
            "column",
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

    // A container says so, and only a container does. `column` is one as well as `columns`:
    // a wrapper that held no blocks would not be a layout, it would be an empty box. Sorted
    // because the registry's declaration order is a presentation choice, not a contract.
    let mut containers: Vec<&str> = blocks
        .iter()
        .filter(|entry| entry["container"] == json!(true))
        .filter_map(|entry| entry["key"].as_str())
        .collect();
    containers.sort_unstable();
    assert_eq!(containers, vec!["column", "columns"]);

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
    // Six, not five: the container's two children are `column` wrappers, and the text inside
    // them is one level deeper than slice 1 had it. A count that stops including a block type
    // the platform just gained is a count the editor's bottom bar can no longer trust.
    assert_eq!(
        clean.body["block_count"],
        json!(7),
        "nested blocks are counted, wrappers included"
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
            "children": [
                column(vec![block("text", json!({ "text": "In a column." }))]),
                column(vec![block("text", json!({ "text": "In the other column." }))])
            ]
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
    // And the container keeps its two column wrappers, each with its own block inside.
    assert_eq!(stored[6]["children"].as_array().map(Vec::len), Some(2));
    assert_eq!(stored[6]["children"][0]["type"], json!("column"));
    assert_eq!(
        stored[6]["children"][0]["children"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
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
    // Seven, because the count an event carries is the same count the validator reports and the
    // tree has gained two wrappers. A count that quietly excludes a block type the platform
    // ships is a hint a downstream integration would act on incorrectly.
    assert_eq!(payload["block_count"], json!(7));
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
    assert_eq!(
        payload["block_count"],
        json!(7),
        "the publication hint counts the same tree the draft event did"
    );
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

/// REQ-063, slice 2: "blocks marked `hide_on: mobile` are absent from the mobile render
/// (server-side), not merely CSS-hidden".
///
/// The claim has three parts and each needs its own assertion, because a CSS-only
/// implementation passes none of them: the desktop render still has the block, the mobile render
/// does not have it *anywhere* in its payload, and a block hidden the other way round is still
/// there for a phone. The test reads the API rather than the HTML because the API is what decides.
#[tokio::test]
async fn a_block_hidden_on_phones_is_absent_from_the_mobile_render() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "viewport").await;

    let mut wide_only = block("text", json!({ "text": "Wide-only line" }));
    wide_only["meta"] = json!({ "hide_on": "mobile" });
    let mut phone_only = block("text", json!({ "text": "Phone-only line" }));
    phone_only["meta"] = json!({ "hide_on": "desktop" });
    let everywhere = block("text", json!({ "text": "Every line" }));

    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [wide_only, phone_only, everywhere] })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

    // The setting is stored exactly as the author wrote it, and `none` is nowhere in the payload:
    // the editor writes absence for "everywhere", so an ordinary page carries no settings at all.
    let stored = saved.body["draft"]["blocks"].clone();
    assert_eq!(stored[0]["meta"]["hide_on"], json!("mobile"));
    assert_eq!(stored[2].get("meta"), None, "an unset block stores no meta");

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

    let public = |viewport: Option<&'static str>| {
        let query = match viewport {
            Some(value) => format!("?site={}&viewport={value}", fixture.site_key),
            None => format!("?site={}", fixture.site_key),
        };
        format!("/api/v1/public/pages/viewport{query}")
    };
    let texts = |body: &Value| -> Vec<String> {
        body["revision"]["blocks"]
            .as_array()
            .expect("the blocks are an array")
            .iter()
            .map(|entry| entry["props"]["text"].as_str().unwrap_or("").to_owned())
            .collect()
    };

    // The default (and the desktop) render carries the blocks that are not hidden from it — which
    // is NOT all three: the phone-only block is `hide_on: desktop`, so a wide screen must not get
    // it either. Asserting "three" here would have tested a filter that hides nothing.
    let desktop = call(
        &fixture.state,
        request(Method::GET, &public(None), None, None),
    )
    .await;
    assert_eq!(desktop.status, StatusCode::OK, "{}", desktop.body);
    assert_eq!(
        texts(&desktop.body),
        vec!["Wide-only line", "Every line"],
        "the wide render drops the block hidden from desktops"
    );

    // The phone render is a *different payload*, not the same page with a class on one block.
    let mobile = call(
        &fixture.state,
        request(Method::GET, &public(Some("mobile")), None, None),
    )
    .await;
    assert_eq!(mobile.status, StatusCode::OK, "{}", mobile.body);
    assert_eq!(
        texts(&mobile.body),
        vec!["Phone-only line", "Every line"],
        "the phone render must not carry the block hidden from phones"
    );
    assert!(
        !mobile.body.to_string().contains("Wide-only line"),
        "the hidden block must not survive anywhere in the phone payload"
    );

    // A word the platform does not recognise is the wide render, not a 400: the query addresses a
    // display choice, and a renderer that sends a bad one must still get a working page.
    let nonsense = call(
        &fixture.state,
        request(Method::GET, &public(Some("tablet")), None, None),
    )
    .await;
    assert_eq!(nonsense.status, StatusCode::OK, "{}", nonsense.body);
    assert_eq!(
        texts(&nonsense.body),
        vec!["Wide-only line", "Every line"],
        "an unreadable viewport word is the wide render, not a filter of its own"
    );

    fixture.cleanup().await;
}

/// A `hide_on` value outside the closed list is refused at the *save*, not at the publish.
///
/// A setting the platform cannot read is worse than no setting: the author believes a block is
/// hidden from phones while it renders on every phone in the country. It is an error rather than
/// a warning because nothing in a published page should be able to contradict what it says.
#[tokio::test]
async fn a_hide_on_value_outside_the_list_cannot_be_saved() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "bad-viewport").await;

    let mut block_value = block("text", json!({ "text": "Asked for a tablet" }));
    block_value["meta"] = json!({ "hide_on": "tablet" });
    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [block_value] })),
        ),
    )
    .await;
    assert_eq!(
        saved.status, StatusCode::BAD_REQUEST,
        "an unreadable viewport must not be stored: {}",
        saved.body
    );

    // The dry run names the setting, so the inspector can put the message under the control.
    let dry = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/blocks/validate",
            Some(&editor),
            Some(json!({ "blocks": [{ "id": "9f5b0e0a-2c2f-4a3f-9a3a-0a1b2c3d4e5f", "type": "text", "props": { "text": "x" }, "meta": { "hide_on": "tablet" } }] })),
        ),
    )
    .await;
    assert_eq!(dry.status, StatusCode::OK, "{}", dry.body);
    let issue = dry.body["issues"]
        .as_array()
        .expect("an array of issues")
        .iter()
        .find(|entry| entry["code"] == json!("block_meta_invalid"))
        .expect("the setting is named");
    assert_eq!(issue["severity"], json!("error"));
    assert!(
        issue["path"].as_str().expect("a path").contains("meta.hide_on"),
        "the path has to name the setting: {issue}"
    );

    fixture.cleanup().await;
}

/// The heading-order rule the REQ names: an `h2` before the page's `h1` warns, and it is a
/// warning — the page renders and publishes. The first half of the criterion is the lint
/// existing at all; this test is the proof that it is *wired to a surface* rather than a
/// function nothing calls.
#[tokio::test]
async fn a_heading_that_skips_back_to_an_h1_is_reported_by_the_dry_run() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    let late = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/blocks/validate",
            Some(&editor),
            Some(json!({
                "blocks": [
                    block("heading", json!({ "text": "A section", "level": "h2" })),
                    block("heading", json!({ "text": "The title", "level": "h1" }))
                ]
            })),
        ),
    )
    .await;
    assert_eq!(late.status, StatusCode::OK, "{}", late.body);
    assert_eq!(
        late.body["can_publish"],
        json!(true),
        "a late h1 renders fine: {}",
        late.body
    );
    assert!(
        late.body["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .any(|entry| entry["code"] == json!("block_heading_order")),
        "the dry run must name the late h1: {}",
        late.body
    );

    // The same two blocks, reordered: the warning is gone. "The warning disappears after
    // reordering" is half of the criterion, and it is only true if the rule looks at order.
    let fixed = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/blocks/validate",
            Some(&editor),
            Some(json!({
                "blocks": [
                    block("heading", json!({ "text": "The title", "level": "h1" })),
                    block("heading", json!({ "text": "A section", "level": "h2" }))
                ]
            })),
        ),
    )
    .await;
    assert_eq!(fixed.status, StatusCode::OK, "{}", fixed.body);
    assert_eq!(
        fixed.body["issues"],
        json!([]),
        "the reordered pair must be silent: {}",
        fixed.body
    );

    fixture.cleanup().await;
}


/// REQ-063, slice 2: the revision compare. "The revision diff shows added/removed/changed
/// blocks with prop-level detail, not a raw JSON diff."
///
/// The test drives the compare the way an author meets it — save a page, change it, open the
/// second revision — so the assertions are about what the response says, not about the shape of
/// a struct the handler happens to return.
#[tokio::test]
async fn two_revisions_compare_block_by_block() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: the development PostgreSQL is not reachable");
        return;
    };
    let editor = login(&fixture.state, &fixture.editor_email).await;
    let page_id = fixture.page(&editor, "diffed").await;

    // Revision 2: a heading, an image and a paragraph. The ids are held so the second save can
    // reuse them — that is what makes the compare report "changed" instead of "removed and
    // added", and it is what the editor does on every keystroke-batched save.
    let heading = block("heading", json!({ "text": "Welcome", "level": "h1" }));
    let image = block("image", json!({ "url": "/hero.png", "alt": "A cat" }));
    let paragraph = block("text", json!({ "text": "The first wording." }));
    let first = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [heading.clone(), image.clone(), paragraph.clone()] })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let first_revision = first.body["draft"]["id"]
        .as_str()
        .expect("a revision id")
        .to_owned();

    // Revision 3: the image's alt text is rewritten, the paragraph is deleted, a button is
    // added. Three of the four change kinds, from one save.
    let mut edited_image = image.clone();
    edited_image["props"]["alt"] = json!("A cat asleep on a keyboard");
    let cta = block("cta", json!({ "text": "Start now", "url": "/signup" }));
    let second = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [heading, edited_image, cta] })),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    let second_revision = second.body["draft"]["id"]
        .as_str()
        .expect("a revision id")
        .to_owned();

    let diff = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions/{second_revision}/diff"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(diff.status, StatusCode::OK, "{}", diff.body);

    // With no `?against=`, the base is the revision before the one being read. An author opening
    // a revision has not chosen a base; the screen asking them to would be a screen that says
    // "pick something to compare" the first time anyone opens it.
    assert_eq!(diff.body["base"]["id"], json!(first_revision));
    assert_eq!(diff.body["compared"]["id"], json!(second_revision));

    let blocks = &diff.body["blocks"];
    assert_eq!(blocks["changed"], json!(1), "the image: {}", diff.body);
    assert_eq!(blocks["removed"], json!(1), "the paragraph: {}", diff.body);
    assert_eq!(blocks["added"], json!(1), "the call to action: {}", diff.body);
    assert!(blocks["has_removals"].as_bool().expect("a bool"));

    // The changed row names the prop and shows both values. This is the whole criterion: a row
    // saying "image changed" is a summary, a row saying "Alternative text: A cat → A cat asleep
    // on a keyboard" is the diff.
    let changed = blocks["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["change"] == json!("changed"))
        .expect("a changed row");
    assert_eq!(changed["block_id"], json!(image["id"]));
    assert_eq!(changed["block_type"], json!("image"));
    assert_eq!(changed["label"], json!("A cat asleep on a keyboard"));
    let prop = &changed["props"][0];
    assert_eq!(prop["path"], json!("alt"));
    assert_eq!(prop["label"], json!("Alternative text"));
    assert_eq!(prop["before"], json!("A cat"));
    assert_eq!(prop["after"], json!("A cat asleep on a keyboard"));

    // The removed row carries the paragraph's text, so a reader can tell *which* paragraph
    // went without opening the old revision.
    let removed = blocks["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["change"] == json!("removed"))
        .expect("a removed row");
    assert_eq!(removed["label"], json!("The first wording."));
    assert_eq!(removed["to_path"], json!(""));

    // The body never changed, and the compare says so rather than inventing a body diff.
    assert_eq!(diff.body["body"]["changed"], json!(false));

    // Naming the base explicitly is the same compare, which is what the revisions screen's
    // "compare with…" picker does.
    let explicit = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/pages/{page_id}/revisions/{second_revision}/diff?against={first_revision}"
            ),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(explicit.status, StatusCode::OK, "{}", explicit.body);
    assert_eq!(explicit.body["blocks"], diff.body["blocks"]);

    // A revision cannot be compared with itself: the query was built wrong, and an empty diff
    // would look like a page nobody ever edited.
    let with_itself = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/pages/{page_id}/revisions/{second_revision}/diff?against={second_revision}"
            ),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(with_itself.status, StatusCode::BAD_REQUEST, "{}", with_itself.body);
    assert_eq!(with_itself.body["error"]["code"], json!("diff_same_revision"));

    // The FIRST revision on a page has nothing before it. It has to be looked up rather than
    // assumed: creating the page already wrote revision 1, so the revision this test saved first
    // is revision 2 and it does have a base.
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
    assert_eq!(history.status, StatusCode::OK, "{}", history.body);
    let first_on_page = history.body["revisions"]
        .as_array()
        .expect("revisions")
        .iter()
        .find(|entry| entry["revision_no"] == json!(1))
        .expect("revision 1")
        .clone();
    let first_diff = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/pages/{page_id}/revisions/{}/diff",
                first_on_page["id"].as_str().expect("an id")
            ),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(first_diff.status, StatusCode::BAD_REQUEST, "{}", first_diff.body);
    assert_eq!(
        first_diff.body["error"]["code"],
        json!("no_earlier_revision")
    );

    fixture.cleanup().await;
}

/// A page that renders from its body has no blocks, and the compare must still answer the
/// question — otherwise "no blocks changed" would be reported for a page whose paragraphs were
/// rewritten wholesale.
#[tokio::test]
async fn a_body_only_page_compares_its_text() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: the development PostgreSQL is not reachable");
        return;
    };
    let editor = login(&fixture.state, &fixture.editor_email).await;
    let page_id = fixture.page(&editor, "body-diffed").await;

    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "body": "A completely rewritten paragraph." })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let revision = saved.body["draft"]["id"].as_str().expect("a revision id");

    let diff = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions/{revision}/diff"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(diff.status, StatusCode::OK, "{}", diff.body);
    assert_eq!(diff.body["body"]["changed"], json!(true));
    assert_eq!(
        diff.body["body"]["after"],
        json!("A completely rewritten paragraph.")
    );
    assert_eq!(
        diff.body["blocks"]["entries"],
        json!([]),
        "a page with no blocks has no block rows, and that is not the same as no change"
    );

    fixture.cleanup().await;
}

/// A reorder must read as a move. This is the property id-keyed comparison buys and a
/// positional diff cannot have: after a reorder every position differs, so a positional compare
/// reports the whole page as rewritten.
#[tokio::test]
async fn a_reorder_reads_as_a_move_not_as_a_rewrite() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: the development PostgreSQL is not reachable");
        return;
    };
    let editor = login(&fixture.state, &fixture.editor_email).await;
    let page_id = fixture.page(&editor, "moved").await;

    let first_block = block("text", json!({ "text": "First." }));
    let second_block = block("text", json!({ "text": "Second." }));
    let first = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [first_block.clone(), second_block.clone()] })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let base_revision = first.body["draft"]["id"].as_str().expect("a revision id");

    let moved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [second_block, first_block] })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    let revision = moved.body["draft"]["id"].as_str().expect("a revision id");

    let diff = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions/{revision}/diff"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(diff.status, StatusCode::OK, "{}", diff.body);
    let blocks = &diff.body["blocks"];
    assert_eq!(blocks["moved"], json!(2), "{}", diff.body);
    assert_eq!(blocks["added"], json!(0), "a move is not an addition");
    assert_eq!(blocks["removed"], json!(0), "a move is not a removal");
    assert_eq!(blocks["changed"], json!(0), "a move is not an edit");

    let _ = base_revision;
    fixture.cleanup().await;
}

/// Comparing a revision is reading the history, so it carries the same key — and a member with
/// only `content.blocks.read` cannot use a page's history through the compare route.
#[tokio::test]
async fn comparing_revisions_needs_the_pages_read_key() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: the development PostgreSQL is not reachable");
        return;
    };
    let editor = login(&fixture.state, &fixture.editor_email).await;
    let page_id = fixture.page(&editor, "guarded-diff").await;
    let member = login(&fixture.state, &fixture.member_email).await;

    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions/{page_id}/diff"),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);

    let allowed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/revisions/{page_id}/diff"),
            Some(&editor),
            None,
        ),
    )
    .await;
    // The editor passes the guard; the revision id is a page id, so the request fails later —
    // which is exactly the point being asserted (the guard, not the lookup).
    assert_ne!(allowed.status, StatusCode::FORBIDDEN, "{}", allowed.body);

    fixture.cleanup().await;
}

/// The preview frame reads the DRAFT and hands back a tree the server filtered (REQ-063
/// acceptance 14: "inline editing saves one draft revision per save, shows the revision number
/// in the toast, and never publishes").
///
/// The frame is the one screen where the REQ's rule has to hold by construction rather than by
/// convention: an author typing in a rendered page is one click away from putting a
/// half-finished sentence in front of visitors. So the claims asserted here are the three that
/// make that impossible — it reads the draft, not the published revision; a save is an ordinary
/// `PATCH` that appends a draft revision; and there is no publish verb anywhere on the route.
#[tokio::test]
async fn the_preview_frame_reads_the_draft_and_filters_it_server_side() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: the development PostgreSQL is not reachable");
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "frame").await;

    let mut wide_only = block("text", json!({ "text": "Wide-only line" }));
    wide_only["meta"] = json!({ "hide_on": "mobile" });
    let everywhere = block("text", json!({ "text": "Every line" }));

    let saved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [wide_only, everywhere] })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

    // Publish, so the frame has BOTH revisions and the difference between them is observable.
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
    let live_no = published.body["published"]["revision_no"].as_i64().expect("a number");

    // An edit that is only a draft: the frame must show it, the site must not.
    let draft_edit = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({
                "blocks": [wide_only.clone(), everywhere.clone(), block("text", json!({ "text": "Draft only" }))]
            })),
        ),
    )
    .await;
    assert_eq!(draft_edit.status, StatusCode::OK, "{}", draft_edit.body);
    let draft_no = draft_edit.body["draft"]["revision_no"]
        .as_i64()
        .expect("a number");

    let frame = |query: &str| {
        call(
            &fixture.state,
            request(
                Method::GET,
                &format!("/api/v1/pages/{page_id}/preview{query}"),
                Some(&editor),
                None,
            ),
        )
    };
    let texts = |body: &Value| -> Vec<String> {
        body["visible_blocks"]
            .as_array()
            .expect("the visible blocks are an array")
            .iter()
            .map(|entry| entry["props"]["text"].as_str().unwrap_or("").to_owned())
            .collect()
    };

    // 1. It reads the DRAFT. The published revision still has two blocks, so a frame that
    //    answered from it would be missing the line the author just wrote.
    let desktop = frame("").await;
    assert_eq!(desktop.status, StatusCode::OK, "{}", desktop.body);
    assert_eq!(desktop.body["revision_no"].as_i64(), Some(draft_no));
    assert_eq!(
        desktop.body["published_revision_no"].as_i64(),
        Some(live_no),
        "the frame names the live revision so the author sees their edit is not public"
    );
    assert!(
        texts(&desktop.body).contains(&"Draft only".to_owned()),
        "the frame must draw the draft: {:?}",
        texts(&desktop.body)
    );
    assert_eq!(desktop.body["block_count"].as_i64(), Some(3));

    // 2. The server filters, and it keeps BOTH trees. A frame that only received the filtered
    //    tree could not tell a hidden block from a deleted one.
    let phone = frame("?viewport=mobile").await;
    assert_eq!(phone.status, StatusCode::OK, "{}", phone.body);
    assert_eq!(phone.body["viewport"], json!("mobile"));
    assert_eq!(
        texts(&phone.body),
        vec!["Every line", "Draft only"],
        "the phone frame must not carry the block hidden from phones"
    );
    assert_eq!(phone.body["block_count"].as_i64(), Some(3));
    assert_eq!(
        phone.body["visible_count"].as_i64(),
        Some(2),
        "the stored tree and the phone render are different sizes, and the frame says so"
    );
    assert!(
        phone.body["blocks"]
            .to_string()
            .contains("Wide-only line"),
        "the unfiltered tree travels beside the filtered one"
    );

    // 3. An unreadable viewport word is the wide render, not a 400 and not a filter of its own:
    //    the query addresses a display choice, so a typo must still produce a working frame.
    let nonsense = frame("?viewport=tablet").await;
    assert_eq!(nonsense.status, StatusCode::OK, "{}", nonsense.body);
    assert_eq!(nonsense.body["viewport"], json!("desktop"));

    fixture.cleanup().await;
}

/// An inline save appends exactly one draft revision and never moves the published one.
///
/// This is the criterion's real content: "saves one draft revision per save … and never
/// publishes". A test that only checked the draft moved would pass against an implementation
/// that also published, which is the failure that matters here.
#[tokio::test]
async fn an_inline_save_writes_one_draft_revision_and_leaves_the_page_alone() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: the development PostgreSQL is not reachable");
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "inline-save").await;

    let first = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [block("text", json!({ "text": "First draft" }))] })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let before = first.body["draft"]["revision_no"].as_i64().expect("a number");

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
    let live = published.body["published"]["revision_no"].as_i64().expect("a number");

    // The frame's save is the page's own PATCH — there is no other verb on the route, and the
    // method list says so: a preview that could publish would need one.
    let inline_save = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/pages/{page_id}"),
            Some(&editor),
            Some(json!({ "blocks": [block("text", json!({ "text": "Typed in the frame" }))] })),
        ),
    )
    .await;
    assert_eq!(inline_save.status, StatusCode::OK, "{}", inline_save.body);
    assert_eq!(
        inline_save.body["draft"]["revision_no"].as_i64(),
        Some(before + 1),
        "one save appends exactly one revision"
    );
    assert_eq!(
        inline_save.body["published"]["revision_no"].as_i64(),
        Some(live),
        "an inline save must not move the published revision"
    );
    assert!(
        inline_save.body["published"]["blocks"]
            .to_string()
            .contains("First draft"),
        "the live revision still carries its own text"
    );

    // The page as visitors get it is untouched, which is the assertion the draft/live split
    // exists for.
    let public = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/public/pages/inline-save?site={}", fixture.site_key),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(public.status, StatusCode::OK, "{}", public.body);
    assert!(
        public.body.to_string().contains("First draft")
            && !public.body.to_string().contains("Typed in the frame"),
        "an inline save is invisible to the public render"
    );

    // Only GET and PATCH belong to the frame. A POST here would be a publish with extra steps.
    let published_again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page_id}/preview"),
            Some(&editor),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        published_again.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "the preview route must carry no verb that could publish"
    );

    fixture.cleanup().await;
}

/// The frame carries the pages read key, like the screen it draws.
#[tokio::test]
async fn the_preview_frame_needs_the_pages_read_key() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: the development PostgreSQL is not reachable");
        return;
    };
    let editor = fixture.editor_token().await;
    let page_id = fixture.page(&editor, "frame-guarded").await;
    let member = fixture.member_token().await;

    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/preview"),
            Some(&member),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);

    let allowed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page_id}/preview"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);

    fixture.cleanup().await;
}

/// The pattern and template galleries answer the platform Owner.
///
/// The Owner is the account with **no** primary organization — that absence is what makes it an
/// Owner — and the gallery routes used to fall back to `user.organization_id` and nothing else, so
/// the account the panel creates on first run was answered `400 organization_required` on the two
/// screens that are supposed to be its first content work. The browser pass caught it as several
/// hundred 400s on `/api/v1/patterns` and `/api/v1/page-templates`; a route-level test is where it
/// should have been caught, because the failure is a property of the account, not of the browser.
///
/// Two halves, and the second is the one that makes the first safe: naming a tenant must be a
/// selector, never a door. An account that does not hold the organization still gets a refusal,
/// so adding a query parameter did not turn the gallery into a cross-tenant read.
#[tokio::test]
async fn the_galleries_answer_the_owner_and_still_refuse_a_foreign_tenant() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let owner = fixture.platform_token().await;
    let editor = fixture.editor_token().await;

    let call = |request: Request<Body>| {
        let state = fixture.state.clone();
        async move {
            let response = routes::router(state)
                .oneshot(request)
                .await
                .expect("router must answer");
            let status = response.status();
            let bytes = response.into_body().collect().await.expect("body reads").to_bytes();
            (
                status,
                if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
                },
            )
        }
    };

    // Without a selector the Owner has nothing to fall back on, and this is exactly the request
    // the panel used to send.
    let refused = call(request(Method::GET, "/api/v1/patterns", Some(&owner), None)).await;
    assert_eq!(
        refused.0,
        StatusCode::BAD_REQUEST,
        "a tenant-addressed read still has to name a tenant: {}",
        refused.1
    );
    assert_eq!(refused.1["error"]["code"], json!("organization_required"));

    // Naming its tenant answers, on both galleries.
    for uri in ["/api/v1/patterns", "/api/v1/page-templates"] {
        let ok = call(request(
            Method::GET,
            &format!("{uri}?organization_id={}", fixture.org),
            Some(&owner),
            None,
        ))
        .await;
        assert_eq!(ok.0, StatusCode::OK, "{uri} must answer the owner: {}", ok.1);
    }

    // The templates read seeds the system set, so it has something to answer with.
    let seeded = call(request(
        Method::GET,
        &format!("/api/v1/page-templates?organization_id={}", fixture.org),
        Some(&owner),
        None,
    ))
    .await;
    let templates = seeded.1["templates"].as_array().expect("templates");
    assert!(
        !templates.is_empty(),
        "the owner's first read seeds the system templates: {}",
        seeded.1
    );

    // A pattern the owner saves is readable by the same selector, and by a member of the tenant
    // without naming anything — the two callers that already worked must keep working.
    let saved = call(request(
        Method::POST,
        "/api/v1/patterns",
        Some(&owner),
        Some(json!({
            "organization_id": fixture.org,
            "key": format!("owner-{}", Uuid::new_v4().simple()),
            "name": "Owner pattern",
            "blocks": [{ "type": "text", "props": { "text": "Saved by the owner" } }],
        })),
    ))
    .await;
    assert_eq!(saved.0, StatusCode::CREATED, "{}", saved.1);

    let member_read = call(request(
        Method::GET,
        "/api/v1/patterns",
        Some(&editor),
        None,
    ))
    .await;
    assert_eq!(
        member_read.0,
        StatusCode::OK,
        "an account with a primary tenant still needs no selector: {}",
        member_read.1
    );
    assert!(
        member_read.1["patterns"]
            .as_array()
            .expect("patterns")
            .iter()
            .any(|p| p["id"] == saved.1["id"]),
        "and it sees what the owner saved into its tenant"
    );

    // The refusal that matters: another tenant, named explicitly, is still refused.
    let foreign_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(foreign_org)
        .bind("Not Yours")
        .bind(format!("other-{}", Uuid::new_v4().simple()))
        .execute(fixture.db.pool())
        .await
        .expect("the other organization must be created");

    let editor_refused = call(request(
        Method::GET,
        &format!("/api/v1/patterns?organization_id={foreign_org}"),
        Some(&editor),
        None,
    ))
    .await;
    assert!(
        matches!(
            editor_refused.0,
            StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST
        ),
        "naming a tenant the account does not hold is refused, not served: {} {}",
        editor_refused.0,
        editor_refused.1
    );
    assert!(
        editor_refused.1["patterns"].is_null(),
        "and it returns no patterns: {}",
        editor_refused.1
    );
}
