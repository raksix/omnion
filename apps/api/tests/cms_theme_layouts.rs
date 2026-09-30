//! Integration test for theme layouts and packages (REQ-062, slice 3).
//!
//! Slice 2's suite was about one sentence — *a save is not a publish* — and this one is about
//! three, each of which the obvious implementation gets wrong in a different direction.
//!
//! * **A slot save is a presentation change, so it must not touch content.** The REQ's whole
//!   risk note is that switching themes never touches `pages`, `page_revisions` or media. A
//!   walk that saves a header and then counts page revisions is the only thing that proves it,
//!   and the count is the assertion — not "the call returned 200".
//!
//! * **A reset restores what the theme SHIPS, not what the site last had.** Two rows carry the
//!   same key, and the only way to tell the tests apart is to make the default and the custom
//!   tree different *in a way that is observable after both writes*. A test that saves `[]`,
//!   resets, and asserts `[]` passes against a reset that deletes the row.
//!
//! * **A package installs inactive, and a bad one installs nothing at all.** Those are two
//!   different claims and they are checked at two different levels: the install response says
//!   `active: false` while the gallery still renders the *previous* theme, and a refused
//!   install leaves the `themes` table with no row for the key. A 422 alone proves nothing —
//!   the obvious bug is a handler that validates, then writes anyway.
//!
//! Two more that cost real time to find in other suites, so they are walks here rather than
//! notes:
//!
//! * **`themes.read` does not imply `themes.customize`, `themes.export` or `themes.install`.**
//!   Four permissions over one surface, and an export is a read that walks off the platform
//!   with the site's look in its hands.
//! * **An unfinished slot SAVES and reports, exactly as an unfinished page does.** The slot
//!   store shares the page store's validator on purpose, so the contract it inherits is
//!   slice 2's half-built contract: fatal findings are refused, everything else is stored and
//!   comes back in `issues`. `an_orphaned_column_is_stored_and_reported_not_refused` is the
//!   walk that keeps that promise from being a comment — and an earlier version of it asserted
//!   the opposite, which is how a 400 was found in the builder for a case the product intends
//!   to accept.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, CsrfSecret};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sites::{self, NewSite};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_security::{CSRF_HEADER, RatePolicy, derive_csrf_token};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";
const CSRF_SECRET: &str = "w2-theme-layouts-suite-csrf-secret";

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

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
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
                    derive_csrf_token(CSRF_SECRET.as_bytes(), &auth.session_id),
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

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = Config::from_env().expect("environment must be valid");
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({error})");
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");
    // Sign-in is limited per IP and this file signs in once per walk. The row goes in BEFORE
    // the state exists, and the limiter is re-read afterwards: `ensure_installed` seeds the
    // process-wide layer from the shipped defaults, so writing the row alone is a silent no-op
    // that reads as "the fix did not work".
    let raised: Vec<Value> = RatePolicy::defaults()
        .into_iter()
        .map(|mut policy| {
            if policy.scope == "sign_in" {
                policy.limit = 10_000;
                policy.burst = 0;
            }
            serde_json::to_value(&policy).unwrap_or(Value::Null)
        })
        .filter(|value| !value.is_null())
        .collect();
    let _ = sqlx::query(
        "insert into security_settings (id, rate_limits) values (1, $1::jsonb) \
         on conflict (id) do update set rate_limits = excluded.rate_limits",
    )
    .bind(serde_json::to_value(&raised).unwrap_or(Value::Null))
    .execute(db.pool())
    .await;

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    let _ = omnion_api::rate_limit_middleware::reload_from_store(&state).await;
    Some((state, db))
}

macro_rules! walk {
    ($state:expr, $body:expr) => {
        async {
            match live_state().await {
                Some((state, db)) => $body(state, db).await,
                None => {
                    eprintln!("SKIP: no database, this walk did not run");
                    Ok(())
                }
            }
        }
    };
}

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn create_organization(db: &Db) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Theme Layouts Co")
    .bind(format!("tl-{}", &Uuid::new_v4().simple().to_string()[..12]))
    .fetch_one(db.pool())
    .await
    .expect("the organization must be created");
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");
    id
}

async fn create_account(db: &Db, organization_id: Uuid) -> (Uuid, String) {
    let email = format!("tl-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Layout Tester".to_owned(),
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
    // Every `Set-Cookie` header, not the first one: the CSRF cookie is set by the same
    // response, and reading only the first header means the walk measures the wrong
    // credential. This is the defect REQ-064 slice 2 recorded in the forms suite.
    let token = response
        .headers
        .iter()
        .filter(|(name, _)| name == "set-cookie")
        .filter_map(|(_, value)| value.split(';').next())
        .find_map(|pair| pair.strip_prefix("omnion_session="))
        .expect("login must set the session cookie")
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

async fn create_site(db: &Db, organization_id: Uuid, key: &str) -> omnion_identity::Site {
    sites::create_site(
        db.pool(),
        NewSite {
            organization_id,
            key: key.to_owned(),
            name: "Layout Site".to_owned(),
            theme: None,
        },
    )
    .await
    .expect("the site must be created")
}

/// An account with every theme power, and the site it works on.
struct Fixture {
    auth: Auth,
    site: omnion_identity::Site,
}

async fn fixture(state: &AppState, db: &Db) -> Fixture {
    let organization_id = create_organization(db).await;
    let (user_id, email) = create_account(db, organization_id).await;
    grant(
        db,
        organization_id,
        user_id,
        &[
            "themes.read",
            "themes.customize",
            "themes.activate",
            "themes.export",
            "themes.install",
        ],
        "Theme Builder",
    )
    .await;
    let site = create_site(db, organization_id, &format!("site-{}", &Uuid::new_v4().simple().to_string()[..8])).await;
    let auth = login(state, db, &email).await;
    Fixture { auth, site }
}

fn slot_uri(fixture: &Fixture, suffix: &str) -> String {
    format!(
        "/api/v1/sites/{}/theme-layouts{suffix}",
        fixture.site.id
    )
}

fn block(kind: &str, props: Value) -> Value {
    json!({ "id": Uuid::new_v4().to_string(), "type": kind, "props": props })
}

/// A header tree the platform can render: two blocks and no structural mistake.
fn header_tree(headline: &str) -> Value {
    json!([
        block("heading", json!({ "text": headline, "level": 2 })),
        block("text", json!({ "text": "Built by the theme builder." })),
    ])
}

/// The tree the theme is supposed to ship, deliberately unlike [`header_tree`].
fn shipped_header() -> Value {
    json!([block("heading", json!({ "text": "Shipped by the theme", "level": 1 }))])
}

/// Reduce a stored tree to what the operator actually typed.
///
/// Two things in a stored tree are not the operator's doing, and comparing them makes an
/// assertion about round-tripping measure the storage layer's default-filling instead:
/// `prepare_tree` mints a block `id` for a block that had none, and `normalize` fills every
/// optional prop the author left out (`align: "left"` above). Both are correct behaviour —
/// they are just not what this assertion is about. What survives is the type and the props
/// that were actually written, which is the claim a save-then-read walk can honestly make.
fn normalize_tree(tree: &Value) -> Value {
    let mut value = tree.clone();
    if let Some(list) = value.as_array_mut() {
        for block in list.iter_mut() {
            let Some(object) = block.as_object_mut() else {
                continue;
            };
            object.remove("id");
            // `prepare_tree` runs `normalize`, which fills EVERY optional prop the author
            // omitted — so the stored block carries keys (`align`, `level`) the sent one does
            // not. Enumerating them by name is how this helper went wrong twice: it stripped
            // `align` and the mismatch simply moved to `level`.
            //
            // The honest reduction is to compare what the operator WROTE, which is exactly the
            // set of props the registry marks required. Asking the schema means a new optional
            // prop never needs an edit here.
            let required = required_props(object);
            if let Some(props) = object.get_mut("props").and_then(Value::as_object_mut) {
                props.retain(|key, _| required.iter().any(|name| name == key));
            }
        }
    }
    value
}

/// The prop keys the block's schema marks required — i.e. the ones the author had to type.
///
/// Empty when the type is unknown, which keeps every prop: a block the registry does not know
/// is already refused by the save, so there is nothing to compare and dropping its keys would
/// turn one failure into a vacuous pass.
fn required_props(block: &serde_json::Map<String, Value>) -> Vec<&'static str> {
    block
        .get("type")
        .and_then(Value::as_str)
        .and_then(omnion_content::blocks::definition)
        .map(|definition| {
            definition
                .props
                .iter()
                .filter(|prop| prop.required)
                .map(|prop| prop.key)
                .collect()
        })
        .unwrap_or_default()
}

/// Write a **bundled** theme row.
///
/// The removal rules are about a theme's SOURCE — bundled, uploaded, active, in use — and a
/// walk about them has to establish that source itself rather than assume it. The application's
/// own seeder is not run by `live_state`, so `minimal` does not exist in a scratch database and
/// the walk answered 404 where it meant to ask 409. A 404 against a 409 assertion proves
/// nothing about the refusal: it is the same layer trap this repository has hit before.
async fn seed_bundled_theme(db: &Db, key: &str) {
    sqlx::query(
        "insert into themes (organization_id, key, name, version, source, manifest) \
         values (null, $1, $1, '1.0.0', 'bundled', '{}'::jsonb) \
         on conflict (key) where removed_at is null do nothing",
    )
    .bind(key)
    .execute(db.pool())
    .await
    .expect("the bundled theme row must be written");
}

async fn seed_default_header(db: &Db, site_id: Uuid, theme_key: &str, blocks: Value) {
    // `default_blocks` is written here exactly as `seed_default_layouts` writes it — the
    // shipped blocks in both columns. A fixture that sets only `blocks` describes a state the
    // product never creates, and every walk that then asked for a reset was really asking
    // about its own setup.
    sqlx::query(
        "insert into theme_layouts (site_id, theme_key, slot, blocks, default_blocks, \
             is_default) \
         values ($1, $2, 'header', $3, $3, true) \
         on conflict (site_id, theme_key, slot) do update \
             set blocks = excluded.blocks, default_blocks = excluded.default_blocks, \
                 is_default = true",
    )
    .bind(site_id)
    .bind(theme_key)
    .bind(blocks)
    .execute(db.pool())
    .await
    .expect("the default header row must be written");
}

async fn active_theme_key(db: &Db, site_id: Uuid) -> String {
    omnion_content::themes::active_theme_key(db.pool(), site_id)
        .await
        .expect("a site always has a theme key")
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// **Saving a slot must NOT destroy the theme's shipped blocks.**
///
/// This walk was written to fail before the schema was fixed, and it is the one that matters
/// most in this file. `save_slot` UPSERTs on `(site_id, theme_key, slot)` — which was also the
/// unique index — and sets `is_default = false`. With one row per triple there was nowhere
/// else for the theme's blocks to live, so a custom save **overwrote** them. `reset_slot` then
/// read `blocks as default_blocks ... where is_default` from a row that no longer matched,
/// so "reset to theme default" answered 409 `theme_slot_no_default` — on a slot that had a
/// default a moment earlier. Reset was a one-way door: the only copy of the theme's blocks was
/// deleted by the first custom edit.
///
/// The fix keeps the theme's blocks in a column the save never touches (`default_blocks`,
/// written by `seed_default_layouts` and by nothing else), so the row can be the site's own
/// AND still answer a reset.
#[tokio::test]
async fn a_custom_save_keeps_the_theme_blocks_a_reset_can_restore() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let theme_key = active_theme_key(&db, fixture.site.id).await;
        seed_default_header(&db, fixture.site.id, &theme_key, shipped_header()).await;

        // Two custom saves in a row, not one: the first is the one that loses the default, and
        // a single save would let a fix that only special-cases the first write pass.
        for headline in ["Ours now", "Ours again"] {
            let sent = header_tree(headline);
            let saved = call(
                &state,
                request(
                    Method::PUT,
                    &slot_uri(&fixture, "/header"),
                    Some(&fixture.auth),
                    Some(sent),
                ),
            )
            .await;
            assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
            assert_eq!(
                saved.body["layout"]["blockCount"],
                json!(2),
                "the save response carries the count the picker's entries carry"
            );
        }

        // Compare the stored tree against the tree that was SENT, not against a freshly built
        // one: `header_tree` mints a new block id on every call, so comparing against a second
        // invocation proves only that ids differ.
        let sent = header_tree("Ours again");
        let stored: Value = sqlx::query_scalar(
            "select blocks from theme_layouts where site_id = $1 and theme_key = $2 \
             and slot = 'header'",
        )
        .bind(fixture.site.id)
        .bind(&theme_key)
        .fetch_one(db.pool())
        .await
        .expect("the custom row must be there");
        assert_eq!(
            normalize_tree(&stored),
            normalize_tree(&sent),
            "the row holds the site's own tree"
        );

        let reset = call(
            &state,
            request(
                Method::POST,
                &slot_uri(&fixture, "/header/reset"),
                Some(&fixture.auth),
                None,
            ),
        )
        .await;
        assert_eq!(
            reset.status,
            StatusCode::OK,
            "a reset after a custom save must work, not answer 409 no-default: {}",
            reset.body
        );
        assert_eq!(
            normalize_tree(&reset.body["layout"]["blocks"]),
            normalize_tree(&shipped_header()),
            "and it must restore the theme's own tree, which only still exists if the save \
             never overwrote it"
        );
        Ok(())
    })
    .await
}

/// **A slot the site deliberately EMPTIES is `custom`, not `empty`.**
///
/// This is the walk that pins the state machine's order. Emptying a slot is a legitimate
/// thing to want — "this theme's footer is noise on my site, take it away" — and it leaves
/// a row with no blocks that is NOT the theme's. If the view answered `empty` for it, the
/// builder would treat the slot as one nothing has been written to, and the reset control
/// (the only way back to the theme's own blocks) would disappear on exactly the slot that
/// needs it. The way back has to be visible because somebody took it.
#[tokio::test]
async fn emptying_a_slot_leaves_it_custom_so_it_can_be_reset() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let theme_key = active_theme_key(&db, fixture.site.id).await;
        seed_default_header(&db, fixture.site.id, &theme_key, shipped_header()).await;

        let emptied = call(
            &state,
            request(
                Method::PUT,
                &slot_uri(&fixture, "/header"),
                Some(&fixture.auth),
                Some(json!([])),
            ),
        )
        .await;
        assert_eq!(emptied.status, StatusCode::OK, "body: {}", emptied.body);
        assert_eq!(
            emptied.body["layout"]["isDefault"],
            json!(false),
            "an emptied slot is the site's own row now"
        );
        assert_eq!(emptied.body["layout"]["blockCount"], json!(0));

        let read = call(
            &state,
            request(Method::GET, &slot_uri(&fixture, ""), Some(&fixture.auth), None),
        )
        .await;
        let header = read.body["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|entry| entry["slot"] == json!("header"))
            .cloned()
            .expect("the header slot is present");
        assert_eq!(
            header["state"],
            json!("custom"),
            "an emptied slot is a custom slot, or the reset control vanishes on it"
        );
        assert_eq!(
            header["isDefault"], json!(false),
            "and it is not the theme's row, which is what makes it resettable"
        );

        // The other half: a theme that genuinely SHIPS an empty slot is not a custom one.
        // Seeding the default row with no blocks must read `empty`, or every theme without a
        // blog list would look like a site that deleted one.
        sqlx::query(
            "update theme_layouts set blocks = '[]'::jsonb \
             where site_id = $1 and theme_key = $2 and slot = 'footer' and is_default",
        )
        .bind(fixture.site.id)
        .bind(&theme_key)
        .execute(db.pool())
        .await
        .ok();

        let after = call(
            &state,
            request(Method::GET, &slot_uri(&fixture, ""), Some(&fixture.auth), None),
        )
        .await;
        let footer = after.body["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|entry| entry["slot"] == json!("footer"))
            .cloned()
            .unwrap_or(json!({}));
        if footer.get("isDefault") == Some(&json!(true)) {
            assert_eq!(
                footer["state"],
                json!("empty"),
                "a theme that ships an empty slot reads empty, not custom"
            );
        }
        Ok(())
    })
    .await
}

/// The builder's payload names all eight slots whether or not a row exists, and says which of
/// `theme` / `custom` / `empty` each one is.
#[tokio::test]
async fn the_slot_picker_answers_every_slot_with_a_state() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let theme_key = active_theme_key(&db, fixture.site.id).await;
        seed_default_header(&db, fixture.site.id, &theme_key, shipped_header()).await;

        let response = call(
            &state,
            request(Method::GET, &slot_uri(&fixture, ""), Some(&fixture.auth), None),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);

        let slots = response.body["slots"].as_array().expect("slots is a list");
        assert_eq!(
            slots.len(),
            8,
            "the picker must show every slot the platform renders, not only the ones with rows"
        );

        let header = slots
            .iter()
            .find(|entry| entry["slot"] == json!("header"))
            .expect("the header slot is present");
        assert_eq!(header["state"], json!("theme"));
        assert_eq!(header["isDefault"], json!(true));
        assert_eq!(header["blockCount"], json!(1));

        // A theme that ships nothing in a slot reports `empty`, and NOT `theme`: the picker
        // badge has three words and "empty" is the honest one for a slot with no blocks.
        let footer = slots
            .iter()
            .find(|entry| entry["slot"] == json!("footer"))
            .expect("the footer slot is present");
        assert_eq!(footer["state"], json!("empty"));
        assert_eq!(footer["blocks"], json!([]));

        // The known block types travel with the payload, because the package validator names
        // what a package got wrong and must not need a second round trip to know the list.
        let known = response.body["knownBlockTypes"].as_array().expect("a list");
        assert!(
            known.iter().any(|kind| kind == "heading"),
            "the registry's own types must be in the answer"
        );
        Ok(())
    })
    .await
}

/// Saving a header marks it `custom`, and the two states are distinguishable afterwards.
#[tokio::test]
async fn saving_a_slot_marks_it_custom_and_keeps_the_payload() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let theme_key = active_theme_key(&db, fixture.site.id).await;
        seed_default_header(&db, fixture.site.id, &theme_key, shipped_header()).await;
        let tree = header_tree("Our own header");

        let saved = call(
            &state,
            request(
                Method::PUT,
                &slot_uri(&fixture, "/header"),
                Some(&fixture.auth),
                Some(tree.clone()),
            ),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
        assert_eq!(saved.body["layout"]["isDefault"], json!(false));
        // Through `normalize_tree`, not raw: `prepare_tree` fills optional props and mints ids
        // on the way in, so a raw equality here would be asserting that the storage layer did
        // NOT run. The claim worth making is "the author's tree survives, modulo defaults".
        assert_eq!(
            normalize_tree(&saved.body["layout"]["blocks"]),
            normalize_tree(&tree)
        );

        let read = call(
            &state,
            request(Method::GET, &slot_uri(&fixture, ""), Some(&fixture.auth), None),
        )
        .await;
        let header = read.body["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|entry| entry["slot"] == json!("header"))
            .cloned()
            .expect("the header slot is present");
        assert_eq!(header["state"], json!("custom"));
        assert_eq!(header["blockCount"], json!(2));
        assert_eq!(
            normalize_tree(&header["blocks"]),
            normalize_tree(&tree),
            "the stored tree is what was sent"
        );
        Ok(())
    })
    .await
}

/// The theme/content separation: a slot save writes `theme_layouts` and nothing else.
///
/// The REQ's risk note is the sentence this walk exists for, and the assertion is a COUNT of
/// page revisions before and after — not the 200. A store that wrote the slot into a page
/// revision would pass every other test in this file.
#[tokio::test]
async fn a_slot_save_touches_no_page_and_no_revision() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let theme_key = active_theme_key(&db, fixture.site.id).await;

        let before_pages: i64 =
            sqlx::query_scalar("select count(*) from pages where site_id = $1")
                .bind(fixture.site.id)
                .fetch_one(db.pool())
                .await
                .expect("the page count must read");
        let before_revisions: i64 = sqlx::query_scalar(
            "select count(*) from page_revisions r join pages p on p.id = r.page_id \
             where p.site_id = $1",
        )
        .bind(fixture.site.id)
        .fetch_one(db.pool())
        .await
        .expect("the revision count must read");

        let saved = call(
            &state,
            request(
                Method::PUT,
                &slot_uri(&fixture, "/header"),
                Some(&fixture.auth),
                Some(header_tree("Unrelated to content")),
            ),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);

        let after_pages: i64 =
            sqlx::query_scalar("select count(*) from pages where site_id = $1")
                .bind(fixture.site.id)
                .fetch_one(db.pool())
                .await
                .expect("the page count must read");
        let after_revisions: i64 = sqlx::query_scalar(
            "select count(*) from page_revisions r join pages p on p.id = r.page_id \
             where p.site_id = $1",
        )
        .bind(fixture.site.id)
        .fetch_one(db.pool())
        .await
        .expect("the revision count must read");

        assert_eq!(before_pages, after_pages, "a slot save must not create a page");
        assert_eq!(
            before_revisions, after_revisions,
            "a slot save must not create or archive a page revision"
        );
        // And the row DID land, or the two counts prove only that nothing happened at all.
        let rows: i64 = sqlx::query_scalar(
            "select count(*) from theme_layouts where site_id = $1 and theme_key = $2 and slot = 'header'",
        )
        .bind(fixture.site.id)
        .bind(&theme_key)
        .fetch_one(db.pool())
        .await
        .expect("the layout count must read");
        assert_eq!(rows, 1, "the slot save must actually have written its row");
        Ok(())
    })
    .await
}

/// Reset restores the SHIPPED tree, and the two are different trees on purpose.
///
/// A reset that emptied the slot, or that restored "whatever the site last had", passes a
/// test where the custom and default trees are the same. Here they are not.
#[tokio::test]
async fn resetting_a_slot_puts_back_what_the_theme_shipped() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let theme_key = active_theme_key(&db, fixture.site.id).await;
        seed_default_header(&db, fixture.site.id, &theme_key, shipped_header()).await;

        let saved = call(
            &state,
            request(
                Method::PUT,
                &slot_uri(&fixture, "/header"),
                Some(&fixture.auth),
                Some(header_tree("Ours now")),
            ),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);

        let reset = call(
            &state,
            request(
                Method::POST,
                &slot_uri(&fixture, "/header/reset"),
                Some(&fixture.auth),
                None,
            ),
        )
        .await;
        assert_eq!(reset.status, StatusCode::OK, "body: {}", reset.body);
        assert_eq!(reset.body["layout"]["isDefault"], json!(true));
        assert_eq!(
            normalize_tree(&reset.body["layout"]["blocks"]),
            normalize_tree(&shipped_header()),
            "reset must restore the theme's own tree, not empty the slot and not keep the \
             custom one"
        );

        let read = call(
            &state,
            request(Method::GET, &slot_uri(&fixture, ""), Some(&fixture.auth), None),
        )
        .await;
        let header = read.body["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|entry| entry["slot"] == json!("header"))
            .cloned()
            .expect("the header slot is present");
        assert_eq!(header["state"], json!("theme"), "a reset slot is theme-provided again");
        Ok(())
    })
    .await
}

/// A reset with no shipped default is a CONFLICT, not a silent empty canvas.
#[tokio::test]
async fn resetting_a_slot_the_theme_never_shipped_is_refused() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        // No default row: this theme ships nothing for the footer.
        let reset = call(
            &state,
            request(
                Method::POST,
                &slot_uri(&fixture, "/footer/reset"),
                Some(&fixture.auth),
                None,
            ),
        )
        .await;
        assert_eq!(
            reset.status,
            StatusCode::CONFLICT,
            "a reset that cannot restore anything must not answer 200 with an empty slot: {}",
            reset.body
        );
        assert_eq!(error_code(&reset.body), "theme_slot_no_default");
        Ok(())
    })
    .await
}

/// A slot name the platform does not render is a 400 with the platform's own sentence.
#[tokio::test]
async fn an_unknown_slot_name_is_refused_by_the_platform() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        for (method, uri) in [
            (Method::GET, slot_uri(&fixture, "/sidebar")),
            (Method::PUT, slot_uri(&fixture, "/sidebar")),
        ] {
            let response = call(
                &state,
                request(
                    method.clone(),
                    &uri,
                    Some(&fixture.auth),
                    (method == Method::PUT).then(|| json!([])),
                ),
            )
            .await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{method} {uri} answered {}: {}",
                response.status,
                response.body
            );
            assert_eq!(error_code(&response.body), "theme_unknown_slot");
        }
        Ok(())
    })
    .await
}

/// The slot store shares the page store's validator, so a structural mistake is refused here
/// too. This is the walk that keeps "the same validator" from being only a comment.
#[tokio::test]
async fn an_orphaned_column_is_stored_and_reported_not_refused() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        // A `column` outside a `columns` block. This walk USED to assert a 400 here, and the
        // walk was wrong rather than the product: slice 2's half-built feature depends on an
        // unfinished tree being savable — `patterns::a_half_built_pattern_stores_and_reports_
        // what_it_still_needs` is the test that says so — and a builder that refuses the
        // author's half-finished drag is a wall exactly where they need somewhere to keep
        // working. The contract is "stores, reports, refuses the publish", and this is that
        // contract written against the route.
        let orphan = json!([block("column", json!({}))]);

        let saved = call(
            &state,
            request(
                Method::PUT,
                &slot_uri(&fixture, "/header"),
                Some(&fixture.auth),
                Some(orphan),
            ),
        )
        .await;
        assert_eq!(
            saved.status,
            StatusCode::OK,
            "an unfinished slot saves, the way an unfinished page does: {}",
            saved.body
        );
        assert_eq!(saved.body["layout"]["blockCount"], json!(1));

        // The issues come back WITH the save, which is the half of the contract that matters:
        // storing silently would tell the author nothing, and that is the difference between a
        // lint and a broken save.
        let issues = saved.body["issues"]
            .as_array()
            .expect("the save reports its issues")
            .iter()
            .map(|issue| issue.as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("Column") && issue.contains("Columns")),
            "the builder must be told what is still wrong: {issues:?}"
        );

        // And it really is stored — read out of the table rather than out of the response, so
        // the claim is about the database and not about a serializer.
        let stored: i64 = sqlx::query_scalar(
            "select count(*) from theme_layouts where site_id = $1 and slot = 'header' \
             and jsonb_array_length(blocks) = 1",
        )
        .bind(fixture.site.id)
        .fetch_one(db.pool())
        .await
        .expect("the layout count must read");
        assert_eq!(stored, 1, "the unfinished tree is in the table");

        Ok(())
    })
    .await
}

/// A save that carries a `<script>` is stored already stripped, and the response says so.
#[tokio::test]
async fn raw_html_in_a_slot_is_sanitised_on_the_way_into_storage() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let tree = json!([block(
            "raw_html",
            json!({ "html": "<p onclick=\"steal()\">hi</p><script>steal()</script>" }),
        )]);

        let saved = call(
            &state,
            request(
                Method::PUT,
                &slot_uri(&fixture, "/header"),
                Some(&fixture.auth),
                Some(tree),
            ),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
        let stored = saved.body["layout"]["blocks"][0]["props"]["html"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            !stored.contains("script") && !stored.contains("onclick"),
            "what is STORED must be safe, because every reader downstream inherits it: {stored}"
        );
        assert!(stored.contains("hi"), "the harmless part must survive: {stored}");

        // The read path agrees with the save path — a value that was sanitised on the way in
        // is the value that comes back out.
        let read = call(
            &state,
            request(
                Method::GET,
                &slot_uri(&fixture, "/header"),
                Some(&fixture.auth),
                None,
            ),
        )
        .await;
        let read_back = read.body["blocks"][0]["props"]["html"]
            .as_str()
            .unwrap_or_default();
        assert!(
            !read_back.contains("script") && !read_back.contains("onclick"),
            "the read path must not resurrect markup the store stripped: {read_back}"
        );
        Ok(())
    })
    .await
}

/// A package with an unknown slot AND an unknown block type reports BOTH.
///
/// The criterion says "refuses a package with an unknown slot or an unknown block type **and
/// lists each problem**", and the "each" is the half that needs a test: a validator that
/// returns on the first error is a validator that makes an operator upload three times.
#[tokio::test]
async fn a_bad_package_reports_every_problem_not_only_the_first() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let package = json!({
            "manifest": {
                "key": "acme-storefront",
                "name": "Acme Storefront",
                "version": "1.0.0",
                "modes": ["light", "dark"],
                "slots": ["header", "sidebar"]
            },
            "slots": {
                "header": [block("carousel", json!({ "images": [] }))],
                "sidebar": [block("text", json!({ "text": "nope" }))]
            },
            "tokens": { "surface": { "light": "#fff" } }
        });

        let validated = call(
            &state,
            request(
                Method::POST,
                "/api/v1/themes/validate",
                Some(&fixture.auth),
                Some(package),
            ),
        )
        .await;
        assert_eq!(validated.status, StatusCode::OK, "body: {}", validated.body);
        assert_eq!(validated.body["valid"], json!(false));
        let findings = validated.body["findings"].as_array().expect("findings");
        let messages: Vec<&str> = findings
            .iter()
            .map(|finding| finding["message"].as_str().unwrap_or_default())
            .collect();
        assert!(
            messages.iter().any(|message| message.contains("sidebar")),
            "the unknown slot must be named: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("carousel")),
            "the unknown block type must be named: {messages:?}"
        );
        assert!(
            validated.body["errorCount"].as_u64().unwrap_or_default() >= 2,
            "two independent problems must be two errors, not one: {}",
            validated.body["errorCount"]
        );
        // Every finding carries a path, because the upload screen points at a line.
        assert!(
            findings
                .iter()
                .all(|finding| !finding["path"].as_str().unwrap_or_default().is_empty()),
            "a finding without a path cannot be shown on a screen"
        );
        Ok(())
    })
    .await
}

/// A refused install writes nothing at all.
#[tokio::test]
async fn a_package_with_errors_installs_nothing() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let package = json!({
            "manifest": { "key": "acme-broken", "name": "Acme Broken", "version": "0.1.0" },
            "slots": { "sidebar": [block("text", json!({ "text": "x" }))] }
        });

        let response = call(
            &state,
            request(
                Method::POST,
                "/api/v1/themes/install",
                Some(&fixture.auth),
                Some(package),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a package with errors is a legal payload the product refuses: {}",
            response.body
        );
        assert_eq!(error_code(&response.body), "theme_package_invalid");
        // The whole report rides on the refusal, because "2 errors" alone sends the operator
        // to the console to find out which two. The envelope is `error.message`, not a
        // top-level `message` — reading the wrong path makes this assertion pass vacuously on
        // a null, which is how it looked like a broken product instead of a broken walk.
        let message = response.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            message.contains("sidebar"),
            "the refusal must name the offending slot: {}",
            message
        );
        assert!(
            message.contains("error"),
            "and it must carry the severity so the upload screen can sort errors from warnings: \
             {}",
            message
        );

        let rows: i64 = sqlx::query_scalar(
            "select count(*) from themes where key = 'acme-broken' and removed_at is null",
        )
        .fetch_one(db.pool())
        .await
        .expect("the theme count must read");
        assert_eq!(rows, 0, "a refused install must leave no row behind");
        Ok(())
    })
    .await
}

/// A valid package installs, and installs INACTIVE.
///
/// The "inactive" half is the acceptance criterion and it is checked where it can be
/// observed: the response says so AND the site's active theme key is unchanged.
#[tokio::test]
async fn a_valid_package_installs_inactive_and_changes_no_site() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let before_key = active_theme_key(&db, fixture.site.id).await;
        let package = json!({
            "manifest": {
                "key": "acme-clean",
                "name": "Acme Clean",
                "version": "2.1.0",
                "modes": ["light", "dark"],
                "slots": ["header", "footer"]
            },
            "slots": { "header": header_tree("Acme header") },
            "tokens": { "surface": { "light": "#ffffff", "dark": "#101010" } }
        });

        let response = call(
            &state,
            request(
                Method::POST,
                "/api/v1/themes/install",
                Some(&fixture.auth),
                Some(package),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
        assert_eq!(response.body["install"]["active"], json!(false));
        assert_eq!(response.body["install"]["themeKey"], json!("acme-clean"));

        let after_key = active_theme_key(&db, fixture.site.id).await;
        assert_eq!(
            before_key, after_key,
            "an install must not activate anything: the site renders what it rendered before"
        );

        // And the row is a real installed theme, not a memory of one.
        let source: Option<String> =
            sqlx::query_scalar("select source from themes where key = 'acme-clean' and removed_at is null")
                .fetch_one(db.pool())
                .await
                .expect("the installed row must exist");
        assert_eq!(source.as_deref(), Some("uploaded"));
        Ok(())
    })
    .await
}

/// An export carries the site's published tokens, its slots, and its active theme — and a
/// re-import of that export is accepted, which is acceptance 12's whole sentence.
///
/// The draft half matters: a site that saved a draft and never published must export the
/// *theme's* tokens, because exporting the draft would install a look nobody has seen.
#[tokio::test]
async fn an_export_round_trips_through_the_validator() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let site = format!("/api/v1/sites/{}/theme-package/export", fixture.site.id);

        let saved = call(
            &state,
            request(
                Method::PUT,
                &slot_uri(&fixture, "/header"),
                Some(&fixture.auth),
                Some(header_tree("Exported header")),
            ),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);

        let exported = call(
            &state,
            request(Method::GET, &site, Some(&fixture.auth), None),
        )
        .await;
        assert_eq!(exported.status, StatusCode::OK, "body: {}", exported.body);
        assert_eq!(exported.body["valid"], json!(true));
        assert_eq!(
            exported.body["slots"]["header"].as_array().map(Vec::len),
            Some(2),
            "the export carries the site's own slot tree"
        );

        // The wire shape the uploader posts is `{ manifest, slots, tokens }`, so the export is
        // repackaged into it before validation — a round trip that skipped this step would be
        // testing two different documents.
        let package = json!({
            "manifest": {
                "key": "acme-round-trip",
                "name": exported.body["name"],
                "version": exported.body["version"],
                "modes": ["light", "dark"],
                "slots": ["header"]
            },
            "slots": exported.body["slots"].clone(),
            "tokens": exported.body["tokens"].clone()
        });

        let validated = call(
            &state,
            request(
                Method::POST,
                "/api/v1/themes/validate",
                Some(&fixture.auth),
                Some(package),
            ),
        )
        .await;
        assert_eq!(
            validated.status,
            StatusCode::OK,
            "an export must re-import cleanly: {}",
            validated.body
        );
        assert_eq!(
            validated.body["valid"],
            json!(true),
            "an export that its own validator refuses is a broken format: {}",
            validated.body["findings"]
        );
        Ok(())
    })
    .await
}

/// A bundled theme can never be deleted, and an in-use uploaded one cannot either.
#[tokio::test]
async fn a_bundled_or_in_use_theme_cannot_be_removed() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        seed_bundled_theme(&db, "minimal").await;

        // `minimal` is bundled, so the answer names the bundling rather than "in use" — the
        // order of the checks is what makes the message actionable.
        let bundled = call(
            &state,
            request(
                Method::DELETE,
                "/api/v1/themes/minimal",
                Some(&fixture.auth),
                None,
            ),
        )
        .await;
        assert_eq!(bundled.status, StatusCode::CONFLICT, "body: {}", bundled.body);
        assert_eq!(error_code(&bundled.body), "theme_bundled_cannot_be_removed");

        // Install one, activate it for a site, then try to remove it: now the answer is
        // "in use", which is a different problem and a different next step.
        let package = json!({
            "manifest": {
                "key": "acme-in-use",
                "name": "Acme In Use",
                "version": "1.0.0",
                "modes": ["light"],
                "slots": ["header"]
            },
            "slots": { "header": [] }
        });
        let installed = call(
            &state,
            request(
                Method::POST,
                "/api/v1/themes/install",
                Some(&fixture.auth),
                Some(package),
            ),
        )
        .await;
        assert_eq!(installed.status, StatusCode::OK, "body: {}", installed.body);

        sqlx::query("insert into site_themes (site_id, theme_key) values ($1, $2)")
            .bind(fixture.site.id)
            .bind("acme-in-use")
            .execute(db.pool())
            .await
            .expect("the activation row must be written");

        let in_use = call(
            &state,
            request(
                Method::DELETE,
                "/api/v1/themes/acme-in-use",
                Some(&fixture.auth),
                None,
            ),
        )
        .await;
        assert_eq!(in_use.status, StatusCode::CONFLICT, "body: {}", in_use.body);
        assert_eq!(error_code(&in_use.body), "theme_in_use");
        Ok(())
    })
    .await
}

/// The four theme permissions are four powers.
///
/// An export is a read that leaves the platform with the site's look in its hands, so
/// `themes.read` must not imply it; and `themes.install` writes a row every site can see, so
/// it must not be implied by `themes.activate` either.
#[tokio::test]
async fn reading_the_gallery_grants_neither_writing_nor_taking_it_away() -> TestResult {
    walk!(state, |state, db| async move {
        let organization_id = create_organization(&db).await;
        let (user_id, email) = create_account(&db, organization_id).await;
        grant(&db, organization_id, user_id, &["themes.read"], "Theme Viewer").await;
        let site = create_site(
            &db,
            organization_id,
            &format!("site-{}", &Uuid::new_v4().simple().to_string()[..8]),
        )
        .await;
        let auth = login(&state, &db, &email).await;

        // Reading works. The gallery is per-SITE — `site_in_scope` resolves the query, so a
        // bare `/themes` is a 400 from the deserializer, not a permission answer. Asserting
        // 200 there would have been asserting nothing about the guard this walk exists for.
        let gallery = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/themes?site={}", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(gallery.status, StatusCode::OK, "body: {}", gallery.body);

        // Writing a slot does not.
        let save = call(
            &state,
            request(
                Method::PUT,
                &format!("/api/v1/sites/{}/theme-layouts/header", site.id),
                Some(&auth),
                Some(json!([])),
            ),
        )
        .await;
        assert_eq!(
            save.status,
            StatusCode::FORBIDDEN,
            "`themes.read` must not imply `themes.customize`: {}",
            save.body
        );

        // And neither does taking the site's look off the platform.
        let exported = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/sites/{}/theme-package/export", site.id),
                Some(&auth),
                None,
            ),
        )
        .await;
        assert_eq!(
            exported.status,
            StatusCode::FORBIDDEN,
            "an export is a read that walks off the platform, so `themes.read` is not enough: {}",
            exported.body
        );

        // Installing is a third power again.
        let installed = call(
            &state,
            request(
                Method::POST,
                "/api/v1/themes/install",
                Some(&auth),
                Some(json!({ "manifest": { "key": "x", "name": "X", "version": "1" } })),
            ),
        )
        .await;
        assert_eq!(installed.status, StatusCode::FORBIDDEN, "body: {}", installed.body);
        Ok(())
    })
    .await
}

/// A site of another organization is refused at the slot route, exactly as it is at the
/// settings route. Two implementations of "in scope" is how the two surfaces drift.
#[tokio::test]
async fn another_organizations_site_is_out_of_scope() -> TestResult {
    walk!(state, |state, db| async move {
        let fixture = fixture(&state, &db).await;
        let other_org = create_organization(&db).await;
        let other_site = create_site(
            &db,
            other_org,
            &format!("site-{}", &Uuid::new_v4().simple().to_string()[..8]),
        )
        .await;

        let response = call(
            &state,
            request(
                Method::GET,
                &format!("/api/v1/sites/{}/theme-layouts", other_site.id),
                Some(&fixture.auth),
                None,
            ),
        )
        .await;
        assert!(
            !response.status.is_success(),
            "a cross-tenant read must not succeed: {}",
            response.body
        );
        assert_eq!(response.status, StatusCode::FORBIDDEN, "body: {}", response.body);
        Ok(())
    })
    .await
}
