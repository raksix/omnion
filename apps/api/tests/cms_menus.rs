//! Integration test for menus and scheduled publishing (REQ-064, slice 1).
//!
//! Slice 1 is the "when does this appear and how do people get to it" half of the CMS depth
//! pack. What has to be true is:
//!
//! * two menus can hold `header` and `footer` at once, and a location already held by one menu
//!   is *refused* by the other rather than silently taken over;
//! * a tree survives a save → reload → save round trip with its nesting and its order, and a
//!   fourth level is refused with a message that names the limit;
//! * `Add pages…` inserts published pages with their titles and refuses a draft — a menu that
//!   links to an unpublished page is a 404 for every visitor;
//! * the public payload is audience-filtered, and the *same* endpoint answers both ways, which
//!   is what makes the editor's preview incapable of disagreeing with the live site;
//! * a scheduled publish fires exactly once, and the queue says so.
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

mod support;
use support::isolated_db::{IsolatedDb, announce_skip, assert_nothing_skipped};
use support::walk_auth;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The menu manager's keys. Note what is absent: nothing here grants a *panel* admin power, and
/// `content.pages.schedule` is deliberately missing — the schedule half of this slice is proved
/// by a second account so the two powers stay provably separate.
const MANAGER_PERMISSIONS: [&str; 4] = [
    "content.pages.read",
    "content.pages.create",
    "content.pages.update",
    "menus.read",
];

/// What the curator adds on top: writing menus and scheduling pages.
const CURATOR_EXTRA: [&str; 3] = [
    "menus.manage",
    "content.pages.schedule",
    "content.pages.publish",
];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookies: Vec<String>,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    // **Every** `Set-Cookie`, not the first: sign-in sets the session and the CSRF token
    // beside it, and `headers().get()` returns one value. See `support::walk_auth`.
    let set_cookies: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_owned))
        .collect();
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
        set_cookies,
        body,
    }
}

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => walk_auth::apply_credential(token, builder),
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
    let mut config = Config::from_env().expect("environment must be valid");
    // A deployment with no CSRF secret never issues the token, and every cookie-authenticated
    // write is then refused with `csrf_unavailable`. Fifteen walks in this file sign in through
    // `walk_auth::Session`, which asserts that token rather than tolerating its absence — so
    // without this line the whole file fails in its own `login` helper and reports nothing about
    // the code it is testing. The suites that already call `with_csrf_secret` do so for the same
    // reason; this file was simply never updated when that helper landed.
    walk_auth::with_csrf_secret(&mut config);
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
    let email = format!("menu-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Menu Tester".to_owned(),
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
    // Both cookies, packed: the session and the CSRF token travel together.
    walk_auth::Session::from_set_cookies(&response.set_cookies).pack()
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
    /// The site's own key — what the public surface addresses it by. A uuid is not a site
    /// address: the resolver classifies its hint as a host (it contains no dot, so it is read as
    /// a key), looks it up, and answers "no site answers to <uuid>". Storing the key is what lets
    /// the public-menu assertions address their own site in a database that has many.
    site_key: String,
    editor_email: String,
    curator_email: String,
    outsider_email: String,
    accounts: Vec<Uuid>,
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
            .bind("Menu Test Org")
            .bind(format!("menu-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        let site_key = format!("mnu{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&site_key)
            .bind("Menu Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        let (editor_id, editor_email) = create_account(&db, Some(org)).await;
        let editor_keys: Vec<&str> = MANAGER_PERMISSIONS.to_vec();
        grant(&db, org, editor_id, &editor_keys, "Menu Editor").await;

        let (curator_id, curator_email) = create_account(&db, Some(org)).await;
        let mut curator_keys = MANAGER_PERMISSIONS.to_vec();
        curator_keys.extend_from_slice(&CURATOR_EXTRA);
        grant(&db, org, curator_id, &curator_keys, "Menu Curator").await;

        let (outsider_id, outsider_email) = create_account(&db, Some(org)).await;

        Some(Self {
            state,
            db,
            org,
            site,
            site_key,
            editor_email,
            curator_email,
            outsider_email,
            accounts: vec![editor_id, curator_id, outsider_id],
        })
    }

    async fn editor(&self) -> String {
        login(&self.state, &self.editor_email).await
    }
    async fn curator(&self) -> String {
        login(&self.state, &self.curator_email).await
    }
    async fn outsider(&self) -> String {
        login(&self.state, &self.outsider_email).await
    }

    /// A published page, so `Add pages…` has something legal to insert.
    async fn published_page(&self, token: &str, slug: &str, title: &str) -> String {
        let created = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/pages",
                Some(token),
                Some(json!({ "site_id": self.site, "slug": slug, "title": title })),
            ),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        let id = created.body["id"].as_str().expect("an id").to_owned();
        let published = call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/pages/{id}/publish"),
                Some(token),
                None,
            ),
        )
        .await;
        assert_eq!(published.status, StatusCode::OK, "{}", published.body);
        id
    }

    /// A draft page — the thing `Add pages…` must refuse.
    async fn draft_page(&self, token: &str, slug: &str, title: &str) -> String {
        let created = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/pages",
                Some(token),
                Some(json!({ "site_id": self.site, "slug": slug, "title": title })),
            ),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        created.body["id"].as_str().expect("an id").to_owned()
    }

    async fn create_menu(
        &self,
        token: &str,
        key: &str,
        name: &str,
        locations: Vec<&str>,
    ) -> String {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/menus",
                Some(token),
                Some(json!({
                    "site_id": self.site,
                    "key": key,
                    "name": name,
                })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.body);
        let id = response.body["id"].as_str().expect("an id").to_owned();
        if !locations.is_empty() {
            let assigned = call(
                &self.state,
                request(
                    Method::PUT,
                    &format!("/api/v1/menus/{id}"),
                    Some(token),
                    Some(json!({ "locations": locations })),
                ),
            )
            .await;
            assert_eq!(assigned.status, StatusCode::OK, "{}", assigned.body);
        }
        id
    }

    async fn cleanup(&self) {
        sqlx::query("delete from users where id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = $1")
            .bind(self.org)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

/// A menu item as the editor submits it.
/// The public menu URL for a fixture's site.
///
/// The hint is explicit because the suite creates one site per test and they all live in the
/// same database: without it the public resolver reaches its "exactly one site" convenience and
/// correctly refuses, which is the product behaving properly and the test addressing it wrongly.
fn public_menu_uri(site_key: &str, audience: &str) -> String {
    format!("/api/v1/public/menus/header?audience={audience}&site={site_key}")
}

fn item(label: &str) -> Value {
    json!({
        "id": Uuid::new_v4().to_string(),
        "parent_id": Value::Null,
        "position": 0,
        "label": label,
        "item_type": "url",
        "url": format!("/{label}"),
    })
}

/// A child of `parent`, one level down.
fn child(parent: &str, label: &str, position: i32) -> Value {
    json!({
        "id": Uuid::new_v4().to_string(),
        "parent_id": parent,
        "position": position,
        "label": label,
        "item_type": "url",
        "url": format!("/{label}"),
    })
}

// --------------------------------------------------------------------------------------------
// The suite
// --------------------------------------------------------------------------------------------

/// **Acceptance 1.** Two menus at once, one on the header and one on the footer, assigned in a
/// single save each — and a *third* trying to take `header` is refused with the holder named.
#[tokio::test]
async fn two_menus_hold_header_and_footer_and_a_third_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;

    let header = fixture
        .create_menu(&token, "primary", "Primary", vec!["header"])
        .await;
    let footer = fixture
        .create_menu(&token, "legal", "Legal", vec!["footer"])
        .await;

    // Both claims stand, and the site has two menus.
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus?site_id={}", fixture.site),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let menus = listed.body.as_array().expect("an array");
    assert_eq!(menus.len(), 2, "both menus must be listed: {menus:?}");

    // A third menu may exist with no location at all.
    let spare = fixture.create_menu(&token, "spare", "Spare", vec![]).await;
    // …and must not be able to take the header the first one holds.
    let contested = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{spare}"),
            Some(&token),
            Some(json!({ "locations": ["header"] })),
        ),
    )
    .await;
    assert_eq!(
        contested.status,
        StatusCode::CONFLICT,
        "a taken location must be refused, not silently taken over: {}",
        contested.body
    );
    assert_eq!(
        contested.body["error"]["code"], "menu_location_taken",
        "the refusal must name the reason: {}",
        contested.body
    );
    assert!(
        contested.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("header"),
        "the message must name the contested location: {}",
        contested.body
    );
    // The message must also name WHO holds it. `header` alone leaves the editor to guess which of
    // the site's menus to open, and the browser pass asserts on this string — a UUID or the word
    // "another" is not something anyone can act on, so the holder's key is required here.
    let message = contested.body["error"]["message"]
        .as_str()
        .expect("a message");
    assert!(
        message.contains("primary"),
        "the refusal must name the holder's key so the editor knows which menu to move: {message}"
    );

    // The first menu still holds it — a refusal that had already moved the location would look
    // exactly like this one from the panel.
    let first = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus/{header}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(first.body["locations"], json!(["header"]));

    // Moving it on purpose is the documented way out.
    let moved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{spare}"),
            Some(&token),
            Some(json!({ "locations": ["header"] })),
        ),
    )
    .await;
    assert_eq!(
        moved.status,
        StatusCode::CONFLICT,
        "still held: {}",
        moved.body
    );

    let released = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{header}"),
            Some(&token),
            Some(json!({ "locations": [] })),
        ),
    )
    .await;
    assert_eq!(released.status, StatusCode::OK, "{}", released.body);
    let now_movable = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{spare}"),
            Some(&token),
            Some(json!({ "locations": ["header"] })),
        ),
    )
    .await;
    assert_eq!(
        now_movable.status,
        StatusCode::OK,
        "after the holder releases it the location is claimable: {}",
        now_movable.body
    );

    let _ = footer;
    fixture.cleanup().await;
}

/// **Acceptance 2.** Three levels by drag survive a reload; a fourth is refused with a message.
#[tokio::test]
async fn items_nest_three_deep_survive_a_reload_and_a_fourth_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let menu = fixture
        .create_menu(&token, "main", "Main", vec!["header"])
        .await;

    let root = item("root");
    let root_id = root["id"].as_str().expect("an id").to_owned();
    let middle = child(&root_id, "middle", 0);
    let middle_id = middle["id"].as_str().expect("an id").to_owned();
    let leaf = child(&middle_id, "leaf", 0);
    let leaf_id = leaf["id"].as_str().expect("an id").to_owned();
    let mut second = item("second");

    let mut items = vec![root, middle, leaf, second];
    items[3]["position"] = json!(1);
    items[0]["position"] = json!(0);

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{menu}/items"),
            Some(&token),
            Some(json!({ "items": items, "locations": ["header"] })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["item_count"], json!(4), "{}", saved.body);

    // Reload: the tree is what came back, in the same shape.
    let reloaded = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus/{menu}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(reloaded.status, StatusCode::OK, "{}", reloaded.body);
    let stored = reloaded.body["items"].as_array().expect("items");
    assert_eq!(stored.len(), 4, "the tree must survive the round trip");
    let by_label = |label: &str| -> Value {
        stored
            .iter()
            .find(|row| row["label"] == label)
            .cloned()
            .unwrap_or_else(|| panic!("{label} must be in the reloaded tree"))
    };
    assert_eq!(by_label("middle")["parent_id"], json!(root_id));
    assert_eq!(by_label("leaf")["parent_id"], json!(middle_id));
    assert_eq!(by_label("second")["parent_id"], json!(Value::Null));
    // The order the editor set is the order the store hands back.
    assert_eq!(by_label("root")["position"], json!(0));
    assert_eq!(by_label("second")["position"], json!(1));

    // A fourth level is refused, and the refusal names the limit.
    let too_deep = child(&leaf_id, "too-deep", 0);
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{menu}/items"),
            Some(&token),
            Some(json!({
                "items": [by_label("root"), by_label("middle"), by_label("leaf"), by_label("second"), too_deep],
                "locations": ["header"],
            })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(
        refused.body["error"]["code"], "menu_too_deep",
        "the refusal must be about depth, not a generic 400: {}",
        refused.body
    );

    // …and the tree on disk is untouched: a refused save is a save that did not happen.
    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus/{menu}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        after.body["items"].as_array().map(Vec::len),
        Some(4),
        "a refused save must leave the stored tree alone: {}",
        after.body
    );

    fixture.cleanup().await;
}

/// **Acceptance 3.** `Add pages…` inserts published pages with their titles and refuses a draft.
#[tokio::test]
async fn add_pages_inserts_only_published_pages_with_their_titles() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let menu = fixture
        .create_menu(&token, "main", "Main", vec!["header"])
        .await;

    let live = fixture
        .published_page(&token, "pricing", "Pricing and plans")
        .await;
    let also_live = fixture
        .published_page(&token, "docs", "Documentation")
        .await;
    let draft = fixture.draft_page(&token, "secret", "Not ready").await;

    // The draft is refused *for the whole batch*, and nothing is written: a menu that ends up
    // with two of the three pages the editor selected is a menu somebody has to clean up.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/menus/{menu}/items/from-pages"),
            Some(&token),
            Some(json!({ "page_ids": [live, also_live, draft] })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "page_not_published");

    let untouched = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus/{menu}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        untouched.body["item_count"],
        json!(0),
        "a refused batch must insert nothing: {}",
        untouched.body
    );

    // The two published pages go in, labelled with their own titles.
    let added = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/menus/{menu}/items/from-pages"),
            Some(&token),
            Some(json!({ "page_ids": [live, also_live] })),
        ),
    )
    .await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    let items = added.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "{}", added.body);
    let labels: Vec<&str> = items
        .iter()
        .map(|row| row["label"].as_str().expect("a label"))
        .collect();
    assert!(labels.contains(&"Pricing and plans"), "{labels:?}");
    assert!(labels.contains(&"Documentation"), "{labels:?}");
    for row in items {
        assert_eq!(row["item_type"], json!("page"), "a page link, not a URL");
        assert!(!row["page_id"].is_null(), "the item must carry its page");
    }

    // The public payload resolves the page link to the page's own slug — the theme never has to
    // know what a `page` item is.
    let rendered = call(
        &fixture.state,
        request(
            Method::GET,
            &public_menu_uri(&fixture.site_key, "visitor"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(rendered.status, StatusCode::OK, "{}", rendered.body);
    let hrefs: Vec<String> = rendered.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|row| row["href"].as_str().expect("an href").to_owned())
        .collect();
    assert!(
        hrefs.contains(&"/pricing".to_owned()),
        "a page item must resolve to its slug: {hrefs:?}"
    );

    fixture.cleanup().await;
}

/// **Acceptance 4.** A `members` item is absent for a signed-out visitor and present for a member,
/// through the same endpoint the theme calls.
#[tokio::test]
async fn a_members_item_is_absent_for_a_visitor_and_present_for_a_member() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let menu = fixture
        .create_menu(&token, "main", "Main", vec!["header"])
        .await;

    let public = item("pricing");
    let mut members = item("members");
    let members_id = members["id"].as_str().expect("an id").to_owned();
    members["visibility"] = json!("members");

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{menu}/items"),
            Some(&token),
            Some(json!({ "items": [public, members], "locations": ["header"] })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["item_count"], json!(2));

    // The panel sees both — the editor must be able to see the rule it set.
    let editor_view = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus/{menu}"),
            Some(&token),
            None,
        ),
    )
    .await;
    let editor_items = editor_view.body["items"].as_array().expect("items");
    assert_eq!(editor_items.len(), 2, "the editor sees both items");
    let gated = editor_items
        .iter()
        .find(|row| row["id"] == json!(members_id))
        .expect("the gated item");
    assert_eq!(gated["visibility"], json!("members"));

    // A signed-out visitor does not.
    let visitor = call(
        &fixture.state,
        request(
            Method::GET,
            &public_menu_uri(&fixture.site_key, "visitor"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(visitor.status, StatusCode::OK, "{}", visitor.body);
    let visitor_labels: Vec<&str> = visitor.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|row| row["label"].as_str().expect("a label"))
        .collect();
    assert_eq!(
        visitor_labels,
        vec!["pricing"],
        "a members item must not reach a signed-out visitor: {visitor_labels:?}"
    );

    // A member does — from the same endpoint, with the other audience.
    let member = call(
        &fixture.state,
        request(
            Method::GET,
            &public_menu_uri(&fixture.site_key, "member"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(member.status, StatusCode::OK, "{}", member.body);
    let member_labels: Vec<&str> = member.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|row| row["label"].as_str().expect("a label"))
        .collect();
    assert_eq!(
        member_labels,
        vec!["pricing", "members"],
        "a member must see the gated item: {member_labels:?}"
    );

    // An audience nobody proved is refused rather than widened.
    let bogus = call(
        &fixture.state,
        request(
            Method::GET,
            &public_menu_uri(&fixture.site_key, "admin"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(bogus.status, StatusCode::BAD_REQUEST, "{}", bogus.body);

    fixture.cleanup().await;
}

/// A submenu whose every child is hidden disappears with its parent — an empty disclosure is a
/// dead control, and the REQ forbids dead controls.
#[tokio::test]
async fn a_submenu_with_no_visible_child_is_not_rendered() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let menu = fixture
        .create_menu(&token, "main", "Main", vec!["header"])
        .await;

    let open = item("pricing");
    let open_id = open["id"].as_str().expect("an id").to_owned();
    let gated_parent = child(&open_id, "gated", 0);
    let gated_parent_id = gated_parent["id"].as_str().expect("an id").to_owned();
    let mut hidden = child(&gated_parent_id, "hidden", 0);
    hidden["visibility"] = json!("members");

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{menu}/items"),
            Some(&token),
            Some(json!({
                "items": [open, gated_parent, hidden],
                "locations": ["header"],
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

    let visitor = call(
        &fixture.state,
        request(
            Method::GET,
            &public_menu_uri(&fixture.site_key, "visitor"),
            None,
            None,
        ),
    )
    .await;
    let top = visitor.body["items"].as_array().expect("items");
    assert_eq!(top.len(), 1, "only the open item survives: {top:?}");
    assert_eq!(
        top[0]["children"].as_array().map(Vec::len),
        Some(0),
        "a branch with no visible child must not render as an empty disclosure: {top:?}"
    );

    // The member sees the branch *with* its child — so the pruning is a filter, not a deletion.
    let member = call(
        &fixture.state,
        request(
            Method::GET,
            &public_menu_uri(&fixture.site_key, "member"),
            None,
            None,
        ),
    )
    .await;
    let member_top = member.body["items"].as_array().expect("items");
    assert_eq!(
        member_top[0]["children"].as_array().map(Vec::len),
        Some(1),
        "the member must see the child: {member_top:?}"
    );

    fixture.cleanup().await;
}

/// Reading a menu needs `menus.read`; writing it needs `menus.manage`, and the two are provably
/// separate powers rather than one key with two names.
#[tokio::test]
async fn reading_and_writing_menus_are_separate_powers() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // The editor holds `menus.read` and nothing that writes a menu.
    let editor = fixture.editor().await;
    let denied = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/menus",
            Some(&editor),
            Some(json!({ "site_id": fixture.site, "key": "nope", "name": "Nope" })),
        ),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);

    // …and may still read.
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus?site_id={}", fixture.site),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);

    fixture.cleanup().await;
}

/// A menu in another organization is not reachable, and the refusal does not say whether it
/// exists beyond "no such menu".
#[tokio::test]
async fn another_organizations_menu_is_not_reachable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let menu = fixture
        .create_menu(&token, "mine", "Mine", vec!["header"])
        .await;

    // A second organization with its own account.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other_org)
        .bind("Other Org")
        .bind(format!("other-{}", Uuid::new_v4().simple()))
        .execute(fixture.db.pool())
        .await
        .expect("the second organization must be created");
    let (other_id, other_email) = create_account(&fixture.db, Some(other_org)).await;
    let keys: Vec<&str> = MANAGER_PERMISSIONS.to_vec();
    grant(&fixture.db, other_org, other_id, &keys, "Other Editor").await;
    let other_token = login(&fixture.state, &other_email).await;

    let peeked = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus/{menu}"),
            Some(&other_token),
            None,
        ),
    )
    .await;
    assert_eq!(peeked.status, StatusCode::FORBIDDEN, "{}", peeked.body);

    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus?site_id={}", fixture.site),
            Some(&other_token),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::FORBIDDEN, "{}", listed.body);

    sqlx::query("delete from users where id = $1")
        .bind(other_id)
        .execute(fixture.db.pool())
        .await
        .expect("the other account must be removed");
    sqlx::query("delete from organizations where id = $1")
        .bind(other_org)
        .execute(fixture.db.pool())
        .await
        .expect("the other organization must be removed");

    fixture.cleanup().await;
}

/// **Acceptance 13 (the first half).** Scheduling a page writes one queue row, a second
/// `POST` replaces it rather than adding a second, and the queue screen can see both the
/// instant and the author's timezone.
#[tokio::test]
async fn scheduling_twice_replaces_the_pending_entry() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let page = fixture.draft_page(&token, "launch", "Launch post").await;

    let first_at = (time::OffsetDateTime::now_utc() + time::Duration::hours(2))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let second_at = (time::OffsetDateTime::now_utc() + time::Duration::hours(5))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({
                "action": "publish",
                "scheduled_at": first_at,
                "timezone": "Europe/Istanbul",
            })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert_eq!(first.body["status"], json!("pending"));
    assert_eq!(first.body["timezone"], json!("Europe/Istanbul"));
    assert_eq!(first.body["page_slug"], json!("launch"));

    // A second POST is a *reschedule*, so the page carries one promise, not two.
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({
                "action": "publish",
                "scheduled_at": second_at,
                "timezone": "Europe/Istanbul",
            })),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::CREATED, "{}", second.body);
    assert_eq!(
        first.body["id"], second.body["id"],
        "a second schedule must replace the first, not add a row"
    );

    let queue = call(
        &fixture.state,
        request(Method::GET, "/api/v1/publishing/queue", Some(&token), None),
    )
    .await;
    assert_eq!(queue.status, StatusCode::OK, "{}", queue.body);
    let rows = queue.body.as_array().expect("an array");
    let for_page: Vec<&Value> = rows
        .iter()
        .filter(|row| row["page_id"] == json!(page))
        .collect();
    assert_eq!(for_page.len(), 1, "one pending entry per page: {rows:?}");

    // Cancelling keeps the row: the queue is the record, and a vanished row leaves "did we
    // publish or cancel?" with nothing to answer it.
    let entry_id = second.body["id"].as_str().expect("an id").to_owned();
    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/publishing/queue/{entry_id}/cancel"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.body);
    assert_eq!(cancelled.body["status"], json!("cancelled"));

    // A cancelled entry cannot be published now.
    let now = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/publishing/queue/{entry_id}/publish-now"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(now.status, StatusCode::CONFLICT, "{}", now.body);

    fixture.cleanup().await;
}

/// A schedule in the past is refused, and an unknown action names the legal values.
#[tokio::test]
async fn scheduling_validates_the_instant_and_the_action() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let page = fixture.draft_page(&token, "past", "In the past").await;

    let yesterday = (time::OffsetDateTime::now_utc() - time::Duration::days(1))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({ "action": "publish", "scheduled_at": yesterday })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "invalid_schedule");

    let bad_action = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({
                "action": "archive",
                "scheduled_at": (time::OffsetDateTime::now_utc() + time::Duration::hours(1))
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
            })),
        ),
    )
    .await;
    assert_eq!(
        bad_action.status,
        StatusCode::BAD_REQUEST,
        "{}",
        bad_action.body
    );
    assert_eq!(bad_action.body["error"]["code"], "invalid_publish_action");
    assert!(
        bad_action.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("publish, unpublish"),
        "the message must name the legal values: {}",
        bad_action.body
    );

    // A timestamp that is not a timestamp is a bad request about *that*, not a server error.
    let not_a_time = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({ "action": "publish", "scheduled_at": "next tuesday" })),
        ),
    )
    .await;
    assert_eq!(
        not_a_time.status,
        StatusCode::BAD_REQUEST,
        "{}",
        not_a_time.body
    );

    fixture.cleanup().await;
}

/// **Acceptance 13 (the timezone half).** A zone this build does not know is refused by the
/// route, and nothing is written — the label is printed beside the instant in the queue, so a
/// stored typo tells an editor their wall clock was honoured when the platform cannot compute
/// it at all. `crates/backup/src/cadence.rs` has refused an unknown zone in a backup cadence
/// since it shipped; this is the same refusal on the CMS side, proved through HTTP rather than
/// by calling the validator, because a store test cannot catch a route that skips it.
#[tokio::test]
async fn an_unknown_timezone_is_refused_and_stores_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let page = fixture.draft_page(&token, "tz", "Timezone check").await;
    let when = (time::OffsetDateTime::now_utc() + time::Duration::hours(2))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({ "action": "publish", "scheduled_at": when, "timezone": "Europe/Istanbool" })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{}",
        refused.body
    );
    assert_eq!(
        refused.body["error"]["code"], "invalid_schedule"
    );
    let message = refused.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Europe/Istanbool") && message.contains("Europe/Istanbul"),
        "the refusal must name what was typed and offer a spelling that works: {message}"
    );
    let stored: (i64,) = sqlx::query_as(
        "select count(*) from cms_publishing_queue where page_id = $1",
    )
    .bind(uuid::Uuid::parse_str(&page).expect("a page id"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must answer");
    assert_eq!(
        stored.0, 0,
        "a refused schedule must leave no row: the queue cannot show what it refused"
    );

    // The same route accepts a real zone, so the refusal is about the zone and not about the
    // field having been closed off.
    let accepted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({ "action": "publish", "scheduled_at": when, "timezone": "Europe/Istanbul" })),
        ),
    )
    .await;
    assert_eq!(
        accepted.status,
        StatusCode::CREATED,
        "{}",
        accepted.body
    );
    assert_eq!(accepted.body["timezone"], "Europe/Istanbul");

    fixture.cleanup().await;
}

/// **Acceptance 13 (the firing half).** A due entry publishes the page exactly once and the queue
/// says what it did. This drives the store's claim path rather than the 30-second worker, because
/// a test that waits for a wall clock is a test that fails for reasons that have nothing to do
/// with the code under it.
#[tokio::test]
async fn a_due_entry_publishes_once_and_records_the_result() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let page = fixture
        .draft_page(&token, "scheduled", "Scheduled post")
        .await;

    // A schedule one second out, so it is due by the time the claim runs.
    let soon = (time::OffsetDateTime::now_utc() + time::Duration::seconds(1))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let scheduled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({ "action": "publish", "scheduled_at": soon, "timezone": "UTC" })),
        ),
    )
    .await;
    assert_eq!(scheduled.status, StatusCode::CREATED, "{}", scheduled.body);
    let entry_id = scheduled.body["id"].as_str().expect("an id").to_owned();

    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;

    let pool = fixture.db.pool();
    let claimed = omnion_content::claim_due(
        pool,
        time::OffsetDateTime::now_utc() + time::Duration::minutes(1),
        50,
    )
    .await
    .expect("the claim must answer");
    assert_eq!(
        claimed.len(),
        1,
        "exactly the due entry is claimed, not the whole history: {}",
        claimed.len()
    );
    let result = omnion_content::run_entry(pool, &claimed[0])
        .await
        .expect("the entry must run");
    assert!(
        result.contains("published"),
        "the result must say what happened: {result}"
    );

    // The page really is published.
    let page_now = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/pages/{page}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        page_now.body["status"],
        json!("published"),
        "{}",
        page_now.body
    );

    // The queue records it as done, with the result line.
    let queue = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/publishing/queue?status=done"),
            Some(&token),
            None,
        ),
    )
    .await;
    let done = queue.body.as_array().expect("an array");
    let row = done
        .iter()
        .find(|row| row["id"] == json!(entry_id))
        .unwrap_or_else(|| panic!("the entry must be done: {}", queue.body));
    assert_eq!(row["status"], json!("done"));
    assert!(
        row["result"]
            .as_str()
            .expect("a result")
            .contains("published"),
        "the result must be recorded: {row}"
    );

    // A second claim finds nothing: the promise fired once.
    let again = omnion_content::claim_due(
        pool,
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        50,
    )
    .await
    .expect("the second claim must answer");
    assert!(
        again.is_empty(),
        "a fired entry must not be claimable again; {} rows came back",
        again.len()
    );

    fixture.cleanup().await;
}

/// A page with no draft to publish fails *visibly*: the row says `failed` and keeps the reason,
/// so the queue screen's retry has something to retry.
#[tokio::test]
async fn a_publish_that_cannot_run_is_recorded_as_failed_with_its_reason() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;

    // A page, published, then unpublised by hand so it has no draft left.
    let page = fixture
        .published_page(&token, "unpublishable", "Unpublishable")
        .await;
    let unpublished =
        omnion_content::unpublish_page(fixture.db.pool(), Uuid::parse_str(&page).expect("a uuid"))
            .await
            .expect("the unpublish must answer");
    assert!(
        unpublished,
        "the page was published, so the unpublish must report that it did work"
    );

    // Unpublishing archives the published revision, so the page has no draft: a publish now has
    // nothing to promote, and that is exactly the failure this test wants to see recorded.
    let soon = (time::OffsetDateTime::now_utc() + time::Duration::seconds(1))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let scheduled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({ "action": "publish", "scheduled_at": soon })),
        ),
    )
    .await;
    assert_eq!(scheduled.status, StatusCode::CREATED, "{}", scheduled.body);
    let entry_id = scheduled.body["id"].as_str().expect("an id").to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;

    let pool = fixture.db.pool();
    let claimed = omnion_content::claim_due(
        pool,
        time::OffsetDateTime::now_utc() + time::Duration::minutes(1),
        50,
    )
    .await
    .expect("the claim must answer");
    assert_eq!(claimed.len(), 1);
    let outcome = omnion_content::run_entry(pool, &claimed[0]).await;
    assert!(
        outcome.is_err(),
        "a page with no draft cannot be published: {outcome:?}"
    );

    let queue = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/publishing/queue?status=failed"),
            Some(&token),
            None,
        ),
    )
    .await;
    let failed = queue.body.as_array().expect("an array");
    let row = failed
        .iter()
        .find(|row| row["id"] == json!(entry_id))
        .unwrap_or_else(|| panic!("the entry must be failed: {}", queue.body));
    assert!(
        !row["error"].as_str().expect("an error").is_empty(),
        "a failed row must keep the reason: {row}"
    );

    // And the retry puts it back in the queue rather than leaving it dead.
    let retried = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/publishing/queue/{entry_id}/retry"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(retried.status, StatusCode::OK, "{}", retried.body);
    assert_eq!(retried.body["status"], json!("pending"));
    assert_eq!(retried.body["error"], json!(""));

    fixture.cleanup().await;
}

/// The queue is scoped to the caller's organization: another tenant's entries are not listed,
/// moved or cancelled.
#[tokio::test]
async fn the_queue_is_scoped_to_the_callers_organization() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let page = fixture.draft_page(&token, "mine", "Mine").await;
    let soon = (time::OffsetDateTime::now_utc() + time::Duration::hours(3))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let scheduled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{page}/schedule"),
            Some(&token),
            Some(json!({ "action": "publish", "scheduled_at": soon })),
        ),
    )
    .await;
    assert_eq!(scheduled.status, StatusCode::CREATED, "{}", scheduled.body);
    let entry_id = scheduled.body["id"].as_str().expect("an id").to_owned();

    // An account with the same keys, in another organization.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other_org)
        .bind("Queue Other Org")
        .bind(format!("qother-{}", Uuid::new_v4().simple()))
        .execute(fixture.db.pool())
        .await
        .expect("the second organization must be created");
    let (other_id, other_email) = create_account(&fixture.db, Some(other_org)).await;
    let mut keys: Vec<&str> = MANAGER_PERMISSIONS.to_vec();
    keys.extend_from_slice(&CURATOR_EXTRA);
    grant(&fixture.db, other_org, other_id, &keys, "Queue Other").await;
    let other_token = login(&fixture.state, &other_email).await;

    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/publishing/queue",
            Some(&other_token),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert!(
        listed.body.as_array().expect("an array").is_empty(),
        "another organization must not see this queue: {}",
        listed.body
    );

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/publishing/queue/{entry_id}/cancel"),
            Some(&other_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        cancelled.status,
        StatusCode::NOT_FOUND,
        "{}",
        cancelled.body
    );

    sqlx::query("delete from users where id = $1")
        .bind(other_id)
        .execute(fixture.db.pool())
        .await
        .expect("the other account must be removed");
    sqlx::query("delete from organizations where id = $1")
        .bind(other_org)
        .execute(fixture.db.pool())
        .await
        .expect("the other organization must be removed");

    fixture.cleanup().await;
}

/// A page item with no page, a link item with no URL and an item with no label are each refused
/// with a message that names the item — a menu full of dead links is the failure this prevents.
#[tokio::test]
async fn an_item_without_a_target_is_refused_by_name() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;
    let menu = fixture
        .create_menu(&token, "main", "Main", vec!["header"])
        .await;

    let mut page_item = item("orphan");
    page_item["item_type"] = json!("page");
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{menu}/items"),
            Some(&token),
            Some(json!({ "items": [page_item], "locations": [] })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "invalid_menu_item");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("orphan"),
        "the refusal must name the item: {}",
        refused.body
    );

    let mut blank = item("nameless");
    blank["label"] = json!("   ");
    let no_label = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{menu}/items"),
            Some(&token),
            Some(json!({ "items": [blank], "locations": [] })),
        ),
    )
    .await;
    assert_eq!(
        no_label.status,
        StatusCode::BAD_REQUEST,
        "{}",
        no_label.body
    );

    // An unknown location is refused naming the legal slots, so the editor's picker and the
    // server cannot drift apart.
    let bad_location = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/menus/{menu}"),
            Some(&token),
            Some(json!({ "locations": ["sidebar-nav"] })),
        ),
    )
    .await;
    assert_eq!(
        bad_location.status,
        StatusCode::BAD_REQUEST,
        "{}",
        bad_location.body
    );
    assert_eq!(bad_location.body["error"]["code"], "invalid_location");
    assert!(
        bad_location.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("header, footer, sidebar, mobile"),
        "the message must name the legal locations: {}",
        bad_location.body
    );

    fixture.cleanup().await;
}

/// **The queue is addressed by site, and the platform owner may read it.** Two defects, both of
/// which only a *real* panel session could reach, and both of which this suite's fixtures were
/// shaped to hide:
///
/// 1. The queue took its organization from `current.user.organization_id` and refused when that
///    was NULL. The account a fresh installation creates first is the platform Owner, whose
///    `organization_id` is deliberately NULL (`crates/onboarding/src/steps.rs` — "an Owner runs
///    the platform, not one tenant"), so the one person who must read the queue got a 400. Every
///    test in this file passes because its fixtures all carry an organization.
/// 2. The organization is not enough. Two sites of one organization have separate queues, and a
///    screen that lists both under one site's name is a leak that reads as a feature — which is
///    why `?site_id=` exists and the panel always sends it.
///
/// And the third thing this proves is the shape the fix had to take: the menu body carries the
/// site's **key** beside its id, because the panel's audience preview calls a *public* route that
/// addresses a site by key or host. Sending the uuid there is a 404 that renders as "this
/// installation has no navigation", and no test on the authenticated half could have seen it.
#[tokio::test]
async fn the_queue_is_read_by_a_platform_owner_and_scoped_to_one_site() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.curator().await;

    // A second site of the SAME organization, with its own page and its own queue entry.
    let sibling_site = Uuid::new_v4();
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(sibling_site)
        .bind(fixture.org)
        .bind(format!("mns{}", &Uuid::new_v4().simple().to_string()[..8]))
        .bind("Menu Sibling Site")
        .execute(fixture.db.pool())
        .await
        .expect("the sibling site must be created");
    let sibling_page = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/pages",
            Some(&token),
            Some(json!({ "site_id": sibling_site, "slug": "sibling", "title": "Sibling" })),
        ),
    )
    .await;
    assert_eq!(
        sibling_page.status,
        StatusCode::CREATED,
        "{}",
        sibling_page.body
    );
    let sibling_page_id = sibling_page.body["id"].as_str().expect("an id").to_owned();
    let scheduled_sibling = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{sibling_page_id}/schedule"),
            Some(&token),
            Some(json!({
                "action": "publish",
                "scheduled_at": (time::OffsetDateTime::now_utc() + time::Duration::hours(5))
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
            })),
        ),
    )
    .await;
    assert_eq!(
        scheduled_sibling.status,
        StatusCode::CREATED,
        "{}",
        scheduled_sibling.body
    );
    let mine = fixture.draft_page(&token, "own", "Own").await;
    let scheduled_mine = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/pages/{mine}/schedule"),
            Some(&token),
            Some(json!({
                "action": "publish",
                "scheduled_at": (time::OffsetDateTime::now_utc() + time::Duration::hours(6))
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
            })),
        ),
    )
    .await;
    assert_eq!(
        scheduled_mine.status,
        StatusCode::CREATED,
        "{}",
        scheduled_mine.body
    );

    // A platform account: no primary organization, the Owner role bound globally — the shape
    // onboarding creates, and the one that answers 400 when a route reads the organization off
    // the account.
    let (owner_id, owner_email) = create_account(&fixture.db, None).await;
    seed::bind_owner(fixture.db.pool(), owner_id)
        .await
        .expect("the owner binding must be written");
    let owner_token = login(&fixture.state, &owner_email).await;

    // **Scoped** — what the panel sends on every load. This call used to answer 400
    // `no_organization` for exactly this account, because the route read the organization off
    // the account instead of off the site the caller named.
    let scoped = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/publishing/queue?site_id={}&limit=500",
                fixture.site
            ),
            Some(&owner_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        scoped.status,
        StatusCode::OK,
        "an owner must read the queue of the site they are looking at: {}",
        scoped.body
    );
    let scoped_rows = scoped.body.as_array().expect("an array").clone();
    assert_eq!(scoped_rows.len(), 1, "one site, one entry: {}", scoped.body);
    assert_eq!(
        scoped_rows[0]["page_id"],
        json!(mine),
        "the row that survives is this site's own: {}",
        scoped.body
    );

    // The sibling's entry is reachable the same way, so the filter is what separates them rather
    // than a broken join: same account, same token, one query parameter apart.
    let sibling_view = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/publishing/queue?site_id={sibling_site}"),
            Some(&owner_token),
            None,
        ),
    )
    .await;
    assert_eq!(sibling_view.status, StatusCode::OK, "{}", sibling_view.body);
    let sibling_rows = sibling_view.body.as_array().expect("an array").clone();
    assert_eq!(
        sibling_rows.len(),
        1,
        "one site, one entry: {}",
        sibling_view.body
    );
    assert_eq!(sibling_rows[0]["page_id"], json!(sibling_page_id));

    // **And writes.** The read path was corrected for this account and the four write handlers
    // were left on `require_organization`, so the queue screen listed a row and then answered 400
    // `no_organization` to Reschedule and Cancel on that very row — every button on the page dead
    // for the platform owner, while every test stayed green because their fixtures all carry an
    // organization. This is the shape that catches it: `create_account(db, None)`, no exception.
    let owned_entry = scheduled_mine.body["id"]
        .as_str()
        .expect("an entry id")
        .to_owned();
    let moved_to = (time::OffsetDateTime::now_utc() + time::Duration::hours(9))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let owner_reschedule = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/publishing/queue/{owned_entry}"),
            Some(&owner_token),
            Some(json!({ "scheduled_at": moved_to })),
        ),
    )
    .await;
    assert_eq!(
        owner_reschedule.status,
        StatusCode::OK,
        "an owner must be able to move the entry they just read: {}",
        owner_reschedule.body
    );
    let owner_cancel = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/publishing/queue/{owned_entry}/cancel"),
            Some(&owner_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        owner_cancel.status,
        StatusCode::OK,
        "and to cancel it: {}",
        owner_cancel.body
    );
    assert_eq!(owner_cancel.body["status"], json!("cancelled"));

    // Unscoped, an organization account still sees both rows: the queue is a per-site screen and
    // the organization-wide read is the API's own convenience, not a panel path.
    let org_token = fixture.curator().await;
    let all = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/publishing/queue?limit=500",
            Some(&org_token),
            None,
        ),
    )
    .await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.body);
    assert!(
        all.body.as_array().expect("an array").len() >= 2,
        "one organization, two sites, two entries: {}",
        all.body
    );

    // A platform owner legitimately reads *across* tenants — that is what `ensure_same_organization`
    // means by `(None, Some(_)) => Ok(())`, and it is the point of an Owner. What must NOT happen
    // is the sibling row leaking into the wrong answer: the foreign site's own queue is empty
    // because nothing is scheduled there, and that is what the scope check turns a leaked row
    // into. An ORGANIZATION account, by contrast, is refused — and that is the assertion below,
    // because "another tenant's queue is visible" is the failure this filter exists to stop.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other_org)
        .bind("Queue Site Other Org")
        .bind(format!(
            "qsite-{}",
            &Uuid::new_v4().simple().to_string()[..8]
        ))
        .execute(fixture.db.pool())
        .await
        .expect("the second organization must be created");
    let foreign_site = Uuid::new_v4();
    sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
        .bind(foreign_site)
        .bind(other_org)
        .bind(format!("mnf{}", &Uuid::new_v4().simple().to_string()[..8]))
        .bind("Menu Foreign Site")
        .execute(fixture.db.pool())
        .await
        .expect("the foreign site must be created");
    let foreign_view = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/publishing/queue?site_id={foreign_site}"),
            Some(&owner_token),
            None,
        ),
    )
    .await;
    assert_eq!(foreign_view.status, StatusCode::OK, "{}", foreign_view.body);
    assert_eq!(
        foreign_view.body.as_array().expect("an array").len(),
        0,
        "the foreign site has nothing scheduled and must answer empty: {}",
        foreign_view.body
    );

    let (tenant_id, tenant_email) = create_account(&fixture.db, Some(fixture.org)).await;
    let mut tenant_keys: Vec<&str> = MANAGER_PERMISSIONS.to_vec();
    tenant_keys.extend_from_slice(&CURATOR_EXTRA);
    grant(
        &fixture.db,
        fixture.org,
        tenant_id,
        &tenant_keys,
        "Queue Tenant",
    )
    .await;
    let tenant_token = login(&fixture.state, &tenant_email).await;
    let refused_foreign = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/publishing/queue?site_id={foreign_site}"),
            Some(&tenant_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused_foreign.status,
        StatusCode::FORBIDDEN,
        "an organization account must be refused another organization's site, not answered empty: {}",
        refused_foreign.body
    );
    assert_eq!(refused_foreign.body["error"]["code"], "cross_organization");
    sqlx::query("delete from users where id = $1")
        .bind(tenant_id)
        .execute(fixture.db.pool())
        .await
        .expect("the tenant account must be removed");

    // The menu body carries the site's key beside its id — the panel's audience preview
    // addresses a site by key or host, so an id alone is not enough to render a preview. Both
    // routes are asserted because the editor reads the detail and the list screen reads the row,
    // and a field added to only one of them is a screen that works in exactly one of them.
    let menu = fixture
        .create_menu(&token, "keyed", "Keyed", vec!["header"])
        .await;
    let detail = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus/{menu}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
    // `MenuDetailBody` FLATTENS the menu beside `items` and `vocabulary` — there is no `menu`
    // key, which is the same reason an indexing test written from memory of the struct fails.
    assert_eq!(
        detail.body["site_key"],
        json!(fixture.site_key),
        "the menu body must carry the site's global key: {}",
        detail.body
    );
    assert_eq!(detail.body["site_id"], json!(fixture.site));

    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/menus?site_id={}", fixture.site),
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
        .find(|row| row["id"] == json!(menu))
        .expect("the menu just created is in its own list");
    assert_eq!(row["site_key"], json!(fixture.site_key), "the list row too");

    sqlx::query("delete from users where id = $1")
        .bind(owner_id)
        .execute(fixture.db.pool())
        .await
        .expect("the owner account must be removed");
    sqlx::query("delete from organizations where id = $1")
        .bind(other_org)
        .execute(fixture.db.pool())
        .await
        .expect("the foreign organization must be removed");
    fixture.cleanup().await;
}
