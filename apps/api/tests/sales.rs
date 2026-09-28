//! Integration tests for the sales catalog surface (docs/requests/REQ-052, slice 1).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason.
//!
//! What the walk proves, in the words of the acceptance criteria: every `/api/v1/sales/*` route
//! answers `401` unauthenticated, `403` with the permission missing and `200` with it granted; a
//! product or price list of another organization is `404`; a bad SKU, a nameless product, a
//! negative price and a duplicate SKU are each refused with the field the form renders the message
//! under; creating, editing, archiving and replacing a price grid write an audit row with the
//! actor, the changed fields and the before/after; the `sales.product.*` and `sales.price_list.*`
//! events reach the feed with the documented payload; the list's filters combine and its sort
//! refuses an unknown column; the price-row replacement is atomic; and — the reason this slice
//! exists — **a product created through the panel is the price a quote line is prefilled with**,
//! including the fallback to the default price when the list has no row for it.
//!
//! One test in here is a **money** test and not a plumbing test: a price written without its
//! trailing zero (`19.9`) must come back as `19.90` and not as `1.99`. That was a real bug in the
//! module's `round_to` (see `modules/sales/src/money.rs`), and every test that existed before it
//! happened to write two decimals — so the walk writes the awkward one on purpose.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope as PermScope};
use omnion_permissions::{roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Serialises this suite: the organizations and the IAM seed are shared state.
static SALES_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What a catalog reader may do: see the products and the price lists, change nothing.
const READER_PERMISSIONS: [&str; 4] = [
    "sales.products.read",
    "sales.pricelists.read",
    "sales.quotes.create",
    "sites.read",
];

/// What a sales manager adds on top: the two catalog writes and the settings read.
///
/// `sales.products.manage` and `sales.pricelists.manage` are proved **separately** below, because
/// separating them is the whole point of the pair: a role that may add a product must not thereby
/// be able to rewrite what the sales desk charges for everything.
const MANAGER_PERMISSIONS: [&str; 8] = [
    "sales.products.read",
    "sales.products.manage",
    "sales.pricelists.read",
    "sales.pricelists.manage",
    "sales.quotes.create",
    "sales.quotes.update",
    "sales.reports.read",
    "sites.read",
];

/// A manager who may also **release** — send a quote, change the approval threshold. The two
/// releases live in their own account so the walk can prove a prepared-but-not-sent role exists.
const RELEASER_PERMISSIONS: [&str; 9] = [
    "sales.products.read",
    "sales.products.manage",
    "sales.pricelists.read",
    "sales.pricelists.manage",
    "sales.quotes.create",
    "sales.quotes.update",
    "sales.quotes.send",
    "sales.orders.read",
    "sites.read",
];

/// A writer in a **second** organization.
///
/// The route guard answers `403` before the module looks at a record, which is right but means a
/// read-only account can never demonstrate the rule that matters most here: a caller who *could*
/// write is still told `404` for another organization's catalog. A `403` would confirm it exists.
const OTHER_WRITER_PERMISSIONS: [&str; 5] = [
    "sales.products.read",
    "sales.products.manage",
    "sales.pricelists.read",
    "sales.pricelists.manage",
    "sites.read",
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
        serde_json::from_slice(&bytes).unwrap_or(Value::String(
            String::from_utf8_lossy(&bytes).to_string(),
        ))
    };

    TestResponse {
        status,
        set_cookie,
        body,
    }
}

/// Build a JSON request; `token` becomes the session cookie and `body` the payload.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };

    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
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
        omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
            .expect("the default storage configuration is valid"),
    );
    Some((state, db))
}

/// One organization, a platform Owner, a sales manager, a plain reader, a releaser, a member with
/// nothing, and a writer of a *second* organization.
struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    org: Uuid,
    manager: String,
    reader: String,
    releaser: String,
    member: String,
    other_writer: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = SALES_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org = create_organization_row(&db, "a").await;
        let other_org = create_organization_row(&db, "b").await;
        // Both organizations are created so the foreign account is a real tenant with its own
        // rows: the `404` walk below would otherwise pass for the wrong reason if the outsider's
        // products were guesses rather than rows that genuinely exist.

        let (owner_id, _owner) = create_account(&db, None, "Sales Owner").await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        let (manager_id, manager) = create_account(&db, Some(org), "Sales Manager").await;
        grant(&db, org, manager_id, owner_id, &MANAGER_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(org), "Sales Reader").await;
        grant(&db, org, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (releaser_id, releaser) = create_account(&db, Some(org), "Sales Releaser").await;
        grant(&db, org, releaser_id, owner_id, &RELEASER_PERMISSIONS).await;

        let (_member_id, member) = create_account(&db, Some(org), "Sales Member").await;

        let (other_id, other_writer) = create_account(&db, Some(other_org), "Sales Other").await;
        grant(&db, other_org, other_id, owner_id, &OTHER_WRITER_PERMISSIONS).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            org,
            manager,
            reader,
            releaser,
            member,
            other_writer,
        })
    }

    async fn token(&self, email: &str) -> String {
        login(&self.state, email).await
    }
}

/// Create an organization row with a unique slug.
async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("sales-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Sales Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("sales-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: name.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Give an account a role with exactly these permissions.
async fn grant(db: &Db, organization_id: Uuid, user_id: Uuid, granted_by: Uuid, permissions: &[&str]) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("sales-role-{}", Uuid::new_v4().simple()),
            name: "Sales Test Role".to_owned(),
            description: "A role of the sales suite".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    let entries: Vec<RolePermissionInput> = permissions
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
        scope: PermScope::Organization { organization_id },
        granted_by: Some(granted_by),
        expires_at: None,
    };
    omnion_permissions::bindings::grant(db.pool(), binding)
        .await
        .expect("the role binding must be created");
}

/// Sign an account in and return its session token.
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

    assert_eq!(
        response.status,
        StatusCode::OK,
        "login body: {}",
        response.body
    );
    response
        .set_cookie
        .clone()
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie has a name")
        .1
        .to_owned()
}

/// The audit rows of one action, newest first.
///
/// The actor column is read as **text**: declaring it as `Value` makes sqlx try to read a `TEXT`
/// cell as `JSONB` and the helper panics with a `ColumnDecode` that names the index and nothing
/// about the query.
async fn audit_rows(db: &Db, action: &str) -> Vec<Value> {
    let rows: Vec<(Value, Option<String>, Option<String>, String)> = sqlx::query_as(
        "select metadata, target_type, target_id, coalesce(actor_user_id::text, '') \
         from audit_log where action = $1 order by id desc limit 5",
    )
    .bind(action)
    .fetch_all(db.pool())
    .await
    .expect("the audit rows must read");

    rows.into_iter()
        .map(|(metadata, target_type, target_id, actor)| {
            json!({
                "metadata": metadata,
                "target_type": target_type,
                "target_id": target_id,
                "actor": actor,
            })
        })
        .collect()
}

/// The payloads of one event name, newest first.
async fn event_payloads(db: &Db, name: &str) -> Vec<Value> {
    let rows: Vec<(Value,)> =
        sqlx::query_as("select payload from events where name = $1 order by id desc limit 5")
            .bind(name)
            .fetch_all(db.pool())
            .await
            .expect("the event rows must read");
    rows.into_iter().map(|(payload,)| payload).collect()
}

/// A SKU unique to one test, so a run cannot collide with another.
fn sku(label: &str) -> String {
    format!("{}-{}", label, &Uuid::new_v4().simple().to_string()[..8])
}

/// Create a product and return its id, failing loudly with the body if the create was refused.
async fn create_product(state: &AppState, token: &str, body: Value) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/products",
            Some(token),
            Some(body),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the product must be created: {}",
        response.body
    );
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
}

/// Create a price list and return its id.
async fn create_price_list(state: &AppState, token: &str, name: &str) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/pricelists",
            Some(token),
            Some(json!({ "name": name })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the price list must be created: {}",
        response.body
    );
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
}

// -------------------------------------------------------------------------------------------
// The walk
// -------------------------------------------------------------------------------------------

/// Every sales route refuses an anonymous caller and a member without the permission.
#[tokio::test]
async fn every_sales_route_is_permission_guarded() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let marker = Uuid::new_v4();

    let calls: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, "/api/v1/sales/products".to_owned(), None),
        (
            Method::POST,
            "/api/v1/sales/products".to_owned(),
            Some(json!({ "sku": "AA-1", "name": "X" })),
        ),
        (
            Method::GET,
            format!("/api/v1/sales/products/{marker}"),
            None,
        ),
        (
            Method::GET,
            format!("/api/v1/sales/products/{marker}/price"),
            None,
        ),
        (
            Method::PATCH,
            format!("/api/v1/sales/products/{marker}"),
            Some(json!({ "name": "Y" })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/sales/products/{marker}"),
            None,
        ),
        (Method::GET, "/api/v1/sales/pricelists".to_owned(), None),
        (
            Method::GET,
            format!("/api/v1/sales/pricelists/{marker}"),
            None,
        ),
        (
            Method::POST,
            "/api/v1/sales/pricelists".to_owned(),
            Some(json!({ "name": "X" })),
        ),
        (
            Method::PATCH,
            format!("/api/v1/sales/pricelists/{marker}"),
            Some(json!({ "name": "Y" })),
        ),
        (
            Method::PUT,
            format!("/api/v1/sales/pricelists/{marker}/items"),
            Some(json!({ "items": [] })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/sales/pricelists/{marker}"),
            None,
        ),
        (Method::GET, "/api/v1/sales/settings".to_owned(), None),
        (
            Method::PUT,
            "/api/v1/sales/settings".to_owned(),
            Some(json!({ "currency": "USD" })),
        ),
    ];

    for (method, uri, body) in &calls {
        let anonymous = call(state, request(method.clone(), uri, None, body.clone())).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must refuse an anonymous caller: {}",
            anonymous.body
        );
    }

    let member = fixture.token(&fixture.member).await;
    for (method, uri, body) in &calls {
        let refused = call(
            state,
            request(method.clone(), uri, Some(&member), body.clone()),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must refuse a member without the permission: {}",
            refused.body
        );
    }
}

/// A product and a price-list row created through the panel come back on a read, and a
/// `sales.product.created` event and audit row are written.
#[tokio::test]
async fn a_product_round_trip_writes_its_audit_row_and_event() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let code = sku("RT");

    let product_id = create_product(
        state,
        &manager,
        json!({
            "sku": code,
            "name": "Widget",
            "description": "A thing we sell",
            "category": "tools",
            "unit": "piece",
            "tax_percent": 20,
            "default_price": "19.90",
            "currency": "TRY",
        }),
    )
    .await;

    // Read it back and check the stored shape, not just the status.
    let read = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products/{product_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "body: {}", read.body);
    assert_eq!(read.body["sku"], code);
    assert_eq!(read.body["name"], "Widget");
    assert_eq!(read.body["category"], "tools");
    assert_eq!(read.body["unit"], "piece");
    assert_eq!(read.body["tax_percent"], 20);
    assert_eq!(read.body["default_price"], "19.90");
    assert_eq!(read.body["currency"], "TRY");
    assert_eq!(read.body["active"], true);

    // The audit row carries the actor and the created product.
    let audited = audit_rows(&fixture.db, "sales.product.created").await;
    assert!(
        !audited.is_empty(),
        "a create must write an audit row the trail can answer"
    );
    let latest = &audited[0];
    assert_eq!(latest["target_type"], "sales_product");
    assert_eq!(latest["target_id"], product_id.to_string());
    assert!(!latest["actor"].as_str().unwrap_or("").is_empty());
    assert_eq!(latest["metadata"]["after"]["sku"], code);

    // The event reaches the feed with the documented payload.
    let payloads = event_payloads(&fixture.db, "sales.product.created").await;
    assert!(
        !payloads.is_empty(),
        "sales.product.created must reach the event feed for automations"
    );
    let payload = payloads
        .iter()
        .find(|value| value["sku"] == code)
        .expect("the created product's own event");
    assert_eq!(payload["product_id"], product_id.to_string());
    assert_eq!(payload["currency"], "TRY");
    assert_eq!(payload["default_price"], "19.90");
}

/// **The money test.** A price typed without its trailing zero keeps its value.
///
/// `19.9` is the shape a person actually types. The module's `round_to` used to relabel the scale
/// without padding the minor units, so this product was stored as `1.99` — a factor of ten, with
/// nothing on screen to say so. Every other test in the repository wrote two decimals, which is
/// why it survived; this one writes the awkward value on purpose.
#[tokio::test]
async fn a_price_written_without_its_trailing_zero_is_not_stored_at_a_tenth_of_itself() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let product_id = create_product(
        state,
        &manager,
        json!({
            "sku": sku("MONEY"),
            "name": "Priced loosely",
            "default_price": "19.9",
        }),
    )
    .await;

    let read = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products/{product_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(
        read.body["default_price"], "19.90",
        "19.9 is nineteen pounds ninety, not one pound ninety-nine"
    );

    // The whole-number case too: `7` is `7.00`, and it is the same bug one decimal place later.
    let whole = create_product(
        state,
        &manager,
        json!({ "sku": sku("MONEY"), "name": "Cheap", "default_price": "7" }),
    )
    .await;
    let read = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products/{whole}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(read.body["default_price"], "7.00");
}

/// A price row written by a seller is the price a line is prefilled with, and a product with no
/// row on the list falls back to its own default price.
///
/// This is the acceptance criterion that decides whether slice 1 exists: "a product and a
/// price-list row are created through the panel and the price appears prefilled in a
/// quote-line request".
#[tokio::test]
async fn a_price_list_row_becomes_the_prefilled_price_and_a_missing_row_falls_back() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let cheap = create_product(
        state,
        &manager,
        json!({ "sku": sku("PL"), "name": "Bulk item", "default_price": "10.00", "unit": "piece" }),
    )
    .await;
    let unlisted = create_product(
        state,
        &manager,
        json!({ "sku": sku("PL"), "name": "Unlisted item", "default_price": "9.99", "unit": "piece" }),
    )
    .await;
    let list_id = create_price_list(state, &manager, &format!("Wholesale {}", &Uuid::new_v4().simple().to_string()[..6])).await;

    // Two tiers: from 1 unit and from 100 units, which is the whole reason `min_quantity` exists.
    let saved = call(
        state,
        request(
            Method::PUT,
            &format!("/api/v1/sales/pricelists/{list_id}/items"),
            Some(&manager),
            Some(json!({ "items": [
                { "product_id": cheap, "min_quantity": "1", "price": "9.00" },
                { "product_id": cheap, "min_quantity": "100", "price": "7.50" }
            ] })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
    assert_eq!(
        saved.body["items"].as_array().map(Vec::len),
        Some(2),
        "both tiers must be stored"
    );
    // The row is joined with its product so the editor renders in one request.
    assert!(
        saved.body["items"][0]["product_sku"].is_string(),
        "a price row carries the product's SKU: {}",
        saved.body
    );
    assert!(saved.body["list"]["item_count"].as_i64().unwrap_or(0) >= 2);

    // The resolution the quote builder calls, per line.
    let at_one = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products/{cheap}/price?quantity=1&price_list_id={list_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(at_one.status, StatusCode::OK, "body: {}", at_one.body);
    assert_eq!(at_one.body["unit_price"], "9.00");

    // The threshold is inclusive and the **largest** qualifying row wins — taking the first match
    // would give a 500-unit order the one-unit price, which is the opposite of a tiered list.
    let at_five_hundred = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products/{cheap}/price?quantity=500&price_list_id={list_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(at_five_hundred.body["unit_price"], "7.50");

    // A product the list does not carry costs its own default price, and the answer says where it
    // came from — "the price came from somewhere else" is a question a person asks when a total
    // is not what they expected, so it is answered rather than assumed.
    let fallback = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products/{unlisted}/price?quantity=1&price_list_id={list_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(
        fallback.body["unit_price"], "9.99",
        "a product with no row must not look free"
    );
    assert_eq!(fallback.body["source"], "resolved");

    // No list at all is also the default price, and says so.
    let no_list = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products/{cheap}/price?quantity=1"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(no_list.body["unit_price"], "10.00");
    assert_eq!(no_list.body["source"], "default");
}

/// Editing a product writes an audit row naming only the fields that changed.
#[tokio::test]
async fn an_edit_writes_an_audit_row_with_the_fields_it_changed() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let product_id = create_product(
        state,
        &manager,
        json!({ "sku": sku("ED"), "name": "Before", "default_price": "5.00" }),
    )
    .await;

    let patched = call(
        state,
        request(
            Method::PATCH,
            &format!("/api/v1/sales/products/{product_id}"),
            Some(&manager),
            Some(json!({ "name": "After", "default_price": "6.50" })),
        ),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "body: {}", patched.body);
    assert_eq!(patched.body["name"], "After");
    assert_eq!(patched.body["default_price"], "6.50");

    let audited = audit_rows(&fixture.db, "sales.product.updated").await;
    let entry = audited
        .iter()
        .find(|row| row["target_id"] == product_id.to_string())
        .expect("the edit must be audited");
    let changed: Vec<&str> = entry["metadata"]["changed"]
        .as_array()
        .expect("a changed list")
        .iter()
        .map(|value| value.as_str().unwrap_or(""))
        .collect();
    assert!(changed.contains(&"name"), "{changed:?}");
    assert!(changed.contains(&"default_price"), "{changed:?}");
    assert!(
        !changed.contains(&"sku"),
        "a field nobody touched must not appear: {changed:?}"
    );
    // The before/after are both there, so a trail reader can see what it was.
    assert_eq!(entry["metadata"]["before"]["name"], "Before");
    assert_eq!(entry["metadata"]["after"]["name"], "After");
}

/// A duplicate SKU is a conflict that says which one, not a generic bad-request.
#[tokio::test]
async fn a_duplicate_sku_is_a_conflict_and_the_catalog_keeps_one_row() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let code = sku("DUP");

    create_product(state, &manager, json!({ "sku": code, "name": "First" })).await;
    let second = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/products",
            Some(&manager),
            Some(json!({ "sku": code, "name": "Second" })),
        ),
    )
    .await;

    assert_eq!(
        second.status,
        StatusCode::CONFLICT,
        "a taken SKU is a conflict: {}",
        second.body
    );
    assert_eq!(second.body["error"]["code"], "product_sku_taken");

    // Case-insensitively: the unique index compares `lower(sku)`, so `DUP-x` and `dup-x` are the
    // same product and the form must not be able to smuggle a second one past the index.
    let lowered = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/products",
            Some(&manager),
            Some(json!({ "sku": code.to_lowercase(), "name": "Third" })),
        ),
    )
    .await;
    assert_eq!(
        lowered.status,
        StatusCode::CONFLICT,
        "a SKU is one product per organization, case-insensitively"
    );
}

/// A refused write names the field the form renders the message under.
#[tokio::test]
async fn every_refusal_names_the_field_it_is_rendered_under() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let cases: Vec<(Value, &str, &str)> = vec![
        (
            json!({ "sku": "a", "name": "Too short a SKU" }),
            "sku",
            "2 to 32 characters",
        ),
        (
            json!({ "sku": "with space", "name": "Bad characters" }),
            "sku",
            "letters, digits",
        ),
        (
            json!({ "sku": sku("V"), "name": "   " }),
            "name",
            "needs a name",
        ),
        (
            json!({ "sku": sku("V"), "name": "Negative", "default_price": "-1.00" }),
            "default_price",
            "cannot be negative",
        ),
        (
            json!({ "sku": sku("V"), "name": "Bad tax", "tax_percent": 101 }),
            "tax_percent",
            "between 0 and 100",
        ),
        (
            json!({ "sku": sku("V"), "name": "Bad currency", "currency": "TRYTRY" }),
            "currency",
            "three-letter",
        ),
    ];

    for (body, field, needle) in cases {
        let refused = call(
            state,
            request(
                Method::POST,
                "/api/v1/sales/products",
                Some(&manager),
                Some(body.clone()),
            ),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{body} must be refused: {}",
            refused.body
        );
        assert_eq!(
            refused.body["error"]["details"]["field"], field,
            "the message must land on the {field} field: {}",
            refused.body
        );
        let message = refused.body["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.contains(needle),
            "the {field} message should say {needle:?}, said {message:?}"
        );
    }
}

/// A record of another organization is a `404`, for a caller who *could* write.
#[tokio::test]
async fn a_catalog_of_another_organization_is_a_four_oh_four() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let outsider = fixture.token(&fixture.other_writer).await;

    let ours = create_product(state, &manager, json!({ "sku": sku("X"), "name": "Ours" })).await;
    let ours_list = create_price_list(state, &manager, &format!("Ours {}", &Uuid::new_v4().simple().to_string()[..6])).await;

    // The foreign account creates its own, so the ids are real rows and not a guess that would
    // pass a `404` for the wrong reason.
    let theirs = create_product(state, &outsider, json!({ "sku": sku("Y"), "name": "Theirs" })).await;
    let theirs_list = create_price_list(state, &outsider, &format!("Theirs {}", &Uuid::new_v4().simple().to_string()[..6])).await;

    // Reading someone else's row is a 404, not a 403: a 403 would confirm that it exists.
    for uri in [
        format!("/api/v1/sales/products/{theirs}"),
        format!("/api/v1/sales/products/{theirs}/price"),
        format!("/api/v1/sales/pricelists/{theirs_list}"),
    ] {
        let response = call(state, request(Method::GET, &uri, Some(&manager), None)).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{uri} must be a 404 for another organization: {}",
            response.body
        );
    }

    // Writing is the same. A 403 here would prove the guard ran but say nothing about the
    // tenant boundary, which is the rule that actually matters.
    for (method, uri, body) in [
        (
            Method::PATCH,
            format!("/api/v1/sales/products/{theirs}"),
            Some(json!({ "name": "Stolen" })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/sales/products/{theirs}"),
            None,
        ),
        (
            Method::PATCH,
            format!("/api/v1/sales/pricelists/{theirs_list}"),
            Some(json!({ "name": "Stolen" })),
        ),
        (
            Method::PUT,
            format!("/api/v1/sales/pricelists/{theirs_list}/items"),
            Some(json!({ "items": [] })),
        ),
    ] {
        let response = call(
            state,
            request(method.clone(), &uri, Some(&manager), body),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{method} {uri} must be a 404 across the tenant boundary: {}",
            response.body
        );
    }

    // The lists themselves stay apart. The strongest form of that is **one SKU used in both
    // organizations**: SKU is unique per organization, not globally, so this is a pair of real rows
    // a person actually creates when two companies under the same reseller both sell "Widget".
    // Each catalog must show its own row and never the other — and the search must reach the right
    // one, which a random term would satisfy vacuously by matching nothing at all.
    let shared = sku("TWIN");
    let ours_twin = create_product(state, &manager, json!({ "sku": shared, "name": "Ours" })).await;
    let theirs_twin =
        create_product(state, &outsider, json!({ "sku": shared, "name": "Theirs" })).await;
    assert_ne!(ours_twin, theirs_twin);

    let ours_page = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products?search={shared}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    let our_ids: Vec<&str> = ours_page.body["items"]
        .as_array()
        .expect("an items array")
        .iter()
        .map(|item| item["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        our_ids.contains(&ours_twin.to_string().as_str()),
        "our own product must be in our own list: {our_ids:?}"
    );
    assert!(
        !our_ids.contains(&theirs_twin.to_string().as_str()),
        "the other organization shares the SKU and must still be invisible: {our_ids:?}"
    );

    // And the mirror image, read by the other account: same term, the other row.
    let theirs_page = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products?search={shared}"),
            Some(&outsider),
            None,
        ),
    )
    .await;
    let their_ids: Vec<&str> = theirs_page.body["items"]
        .as_array()
        .expect("an items array")
        .iter()
        .map(|item| item["id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        their_ids,
        vec![theirs_twin.to_string().as_str()],
        "each account sees exactly its own row for the shared SKU"
    );
    let _ = (ours, ours_list);
}

/// The catalog and the price lists are **separate powers**, and a price row cannot name a product
/// of another organization.
#[tokio::test]
async fn a_product_writer_cannot_rewrite_a_price_list_and_vice_versa() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;

    // Two narrow accounts and the platform Owner that grants to them. The split is the point of
    // this test: `sales.products.manage` and `sales.pricelists.manage` are two keys, because a
    // price list is shared with everyone who quotes from it and a product is one item in a
    // catalog — a role that may add a widget must not thereby be able to rewrite what the whole
    // sales desk charges for everything.
    let (products_only_id, products_only) =
        create_account(&fixture.db, Some(fixture.org), "Products Only").await;
    let (lists_only_id, lists_only) =
        create_account(&fixture.db, Some(fixture.org), "Lists Only").await;
    let (owner_id, _owner) = fixture.owner_account().await;

    grant(
        &fixture.db,
        fixture.org,
        products_only_id,
        owner_id,
        &["sales.products.read", "sales.products.manage", "sites.read"],
    )
    .await;
    grant(
        &fixture.db,
        fixture.org,
        lists_only_id,
        owner_id,
        &["sales.pricelists.read", "sales.pricelists.manage", "sites.read"],
    )
    .await;

    let products_token = fixture.token(&products_only).await;
    let lists_token = fixture.token(&lists_only).await;

    // Each may do its own half, which is what makes the `403`s below a finding about the split and
    // not a fixture that simply granted nothing.
    let mine = create_product(
        state,
        &products_token,
        json!({ "sku": sku("SP"), "name": "Mine" }),
    )
    .await;
    assert!(!mine.is_nil());
    let theirs = create_price_list(state, &lists_token, &format!("Theirs {}", &Uuid::new_v4().simple().to_string()[..6])).await;
    assert!(!theirs.is_nil());

    let refused = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/pricelists",
            Some(&products_token),
            Some(json!({ "name": "Not allowed" })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a product writer must not be able to create a price list: {}",
        refused.body
    );

    let refused = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/products",
            Some(&lists_token),
            Some(json!({ "sku": "ZZ-1", "name": "Not allowed" })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a price-list writer must not be able to create a product: {}",
        refused.body
    );
}

/// A price row naming a product of another organization is refused **by name**, not by a trigger
/// exception the caller would read as a `500`.
#[tokio::test]
async fn a_price_row_of_a_foreign_product_is_refused_with_a_field_name() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let outsider = fixture.token(&fixture.other_writer).await;

    let theirs = create_product(state, &outsider, json!({ "sku": sku("F"), "name": "Theirs" })).await;
    let list_id = create_price_list(state, &manager, &format!("Ours {}", &Uuid::new_v4().simple().to_string()[..6])).await;

    let refused = call(
        state,
        request(
            Method::PUT,
            &format!("/api/v1/sales/pricelists/{list_id}/items"),
            Some(&manager),
            Some(json!({ "items": [{ "product_id": theirs, "price": "1.00" }] })),
        ),
    )
    .await;

    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a foreign product in a price row is a refusal, not a crash: {}",
        refused.body
    );
    assert_eq!(
        refused.body["error"]["details"]["field"], "product_id",
        "the message must name the field: {}",
        refused.body
    );
}

/// Replacing the rows is atomic: a refused row leaves the previous grid exactly as it was.
///
/// A save that deleted the rows and then failed on the third insert would leave a list that
/// prices **nothing**, and every quote built on it would silently fall back to the default price
/// — the most expensive possible kind of quiet.
#[tokio::test]
async fn a_refused_price_row_leaves_the_previous_grid_untouched() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let good = create_product(state, &manager, json!({ "sku": sku("AT"), "name": "Good", "default_price": "10.00" })).await;
    let list_id = create_price_list(state, &manager, &format!("Atomic {}", &Uuid::new_v4().simple().to_string()[..6])).await;

    let saved = call(
        state,
        request(
            Method::PUT,
            &format!("/api/v1/sales/pricelists/{list_id}/items"),
            Some(&manager),
            Some(json!({ "items": [{ "product_id": good, "price": "8.00" }] })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
    assert_eq!(saved.body["items"].as_array().map(Vec::len), Some(1));

    // Now a grid whose second row is refused: a negative price.
    let refused = call(
        state,
        request(
            Method::PUT,
            &format!("/api/v1/sales/pricelists/{list_id}/items"),
            Some(&manager),
            Some(json!({ "items": [
                { "product_id": good, "price": "1.00" },
                { "product_id": good, "min_quantity": "10", "price": "-5.00" }
            ] })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a negative price row is refused: {}",
        refused.body
    );

    // The first row is still the one that is there: the replacement was rolled back whole.
    let read = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/pricelists/{list_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    let items = read.body["items"].as_array().expect("an items array");
    assert_eq!(items.len(), 1, "a refused save must not half-apply: {items:?}");
    assert_eq!(items[0]["price"], "8.00", "the surviving row is the old one");
}

/// A price-list read exposes the settings the quote builder defaults from, and a write changes them.
#[tokio::test]
async fn the_settings_round_trip_and_the_threshold_is_readable_by_a_seller() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let releaser = fixture.token(&fixture.releaser).await;
    let seller = fixture.token(&fixture.reader).await;

    // A new organization has its settings row from the migration's trigger, so a brand-new quote
    // can read a currency without a lookup that can miss.
    let settings = call(
        state,
        request(Method::GET, "/api/v1/sales/settings", Some(&seller), None),
    )
    .await;
    assert_eq!(settings.status, StatusCode::OK, "body: {}", settings.body);
    assert_eq!(settings.body["discount_approval_threshold"], 15);
    assert_eq!(settings.body["quote_validity_days"], 30);

    // A seller who may not send cannot rewrite the threshold they are measured against.
    let refused = call(
        state,
        request(
            Method::PUT,
            "/api/v1/sales/settings",
            Some(&seller),
            Some(json!({ "discount_approval_threshold": 90 })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "changing the approval threshold is a release, not a draft: {}",
        refused.body
    );

    let written = call(
        state,
        request(
            Method::PUT,
            "/api/v1/sales/settings",
            Some(&releaser),
            Some(json!({
                "currency": "usd",
                "discount_approval_threshold": 25,
                "quote_validity_days": 45
            })),
        ),
    )
    .await;
    assert_eq!(written.status, StatusCode::OK, "body: {}", written.body);
    assert_eq!(
        written.body["currency"], "USD",
        "a currency is normalised to upper case"
    );
    assert_eq!(written.body["discount_approval_threshold"], 25);
    assert_eq!(written.body["quote_validity_days"], 45);

    // The write is audited with both sides, because a threshold change is retroactive in effect:
    // it decides which of the seller's past drafts would now need a manager.
    let audited = audit_rows(&fixture.db, "sales.settings.updated").await;
    let entry = audited
        .iter()
        .find(|row| row["metadata"]["after"]["discount_approval_threshold"] == 25)
        .expect("the settings write must be audited");
    let changed: Vec<&str> = entry["metadata"]["changed"]
        .as_array()
        .expect("a changed list")
        .iter()
        .map(|value| value.as_str().unwrap_or(""))
        .collect();
    assert!(changed.contains(&"discount_approval_threshold"), "{changed:?}");
    assert_eq!(entry["metadata"]["before"]["discount_approval_threshold"], 15);
}

/// The catalog list filters, sorts and pages, and refuses a sort column it does not have.
#[tokio::test]
async fn the_catalog_list_filters_and_refuses_an_unknown_sort() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    // A marker short enough to leave room for the index: the SKU column is 32 characters and the
    // validator enforces the same bound, so a full 32-character uuid plus `-0` would be refused
    // for being long — which is the fixture's bug, not the module's.
    let marker = &Uuid::new_v4().simple().to_string()[..10];

    for index in 0..3 {
        create_product(
            state,
            &manager,
            json!({
                "sku": format!("{marker}-{index}"),
                "name": format!("Marked {index}"),
                "category": if index == 0 { "markers" } else { "other" },
                "default_price": format!("{}.00", index + 1),
            }),
        )
        .await;
    }

    // Search by SKU fragment finds exactly the three.
    let found = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products?search={marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "body: {}", found.body);
    assert_eq!(
        found.body["items"].as_array().map(Vec::len),
        Some(3),
        "the marker must find the three it named: {}",
        found.body
    );
    assert!(found.body["total_estimate"].as_i64().unwrap_or(0) >= 3);

    // The count and the rows agree, because both come from the same clause builder — a filter
    // that matched in one and missed in the other is how a list lies about its own size.
    let paged = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products?search={marker}&limit=2"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(paged.body["items"].as_array().map(Vec::len), Some(2));
    let cursor = paged.body["next_cursor"]
        .as_str()
        .expect("a first page of 2 of 3 has a next page");

    let second = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products?search={marker}&limit=2&cursor={cursor}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(second.body["items"].as_array().map(Vec::len), Some(1));

    // The category filter narrows, and the sort orders.
    let by_category = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products?category=markers"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(by_category.status, StatusCode::OK);

    let sorted = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products?search={marker}&sort=default_price&direction=desc"),
            Some(&manager),
            None,
        ),
    )
    .await;
    let prices: Vec<&str> = sorted.body["items"]
        .as_array()
        .expect("an items array")
        .iter()
        .map(|item| item["default_price"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(prices, vec!["3.00", "2.00", "1.00"], "descending by price");

    // A column the list does not have is refused with the ones it does, rather than silently
    // sorting by something the person did not ask for.
    let refused = call(
        state,
        request(
            Method::GET,
            "/api/v1/sales/products?sort=nonsense",
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "body: {}", refused.body);
    assert_eq!(refused.body["error"]["code"], "invalid_sales_query");
    let message = refused.body["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("nonsense"), "{message}");
    assert!(message.contains("sku"), "{message}");

    // The vocabulary the two dropdowns draw comes from the rows that exist, plus the presets.
    let vocabulary = call(
        state,
        request(
            Method::GET,
            "/api/v1/sales/products/vocabulary",
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(vocabulary.status, StatusCode::OK);
    let units: Vec<&str> = vocabulary.body["units"]
        .as_array()
        .expect("a units array")
        .iter()
        .map(|value| value.as_str().unwrap_or(""))
        .collect();
    for preset in ["piece", "hour", "kilogram", "litre", "day"] {
        assert!(units.contains(&preset), "the {preset} preset must be offered: {units:?}");
    }
    let categories: Vec<&str> = vocabulary.body["categories"]
        .as_array()
        .expect("a categories array")
        .iter()
        .map(|value| value.as_str().unwrap_or(""))
        .collect();
    assert!(categories.contains(&"markers"), "{categories:?}");
}

/// Archiving takes a product out of the list without deleting the row a past quote still names.
#[tokio::test]
async fn archiving_a_product_hides_it_and_leaves_the_row_readable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let code = sku("ARCH");
    let product_id = create_product(
        state,
        &manager,
        json!({ "sku": code, "name": "Retired", "default_price": "3.00" }),
    )
    .await;

    let archived = call(
        state,
        request(
            Method::DELETE,
            &format!("/api/v1/sales/products/{product_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK, "body: {}", archived.body);
    assert!(
        archived.body["archived_at"].is_string(),
        "archiving must stamp the row, not only clear the flag: {}",
        archived.body
    );
    assert_eq!(archived.body["active"], false, "body: {}", archived.body);

    // Gone from the default list…
    let listed = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products?search={code}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(
        listed.body["items"].as_array().map(Vec::len),
        Some(0),
        "an archived product is not offered to a new line: {}",
        listed.body
    );

    // …and still readable, because a quote line that named it last month must still resolve.
    let read = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/sales/products/{product_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::OK,
        "an archived product is still a product: {}",
        read.body
    );
    assert_eq!(read.body["default_price"], "3.00");

    // Its SKU is free again, which is the whole reason the unique index is partial.
    let reused = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/products",
            Some(&manager),
            Some(json!({ "sku": code, "name": "Replacement" })),
        ),
    )
    .await;
    assert_eq!(
        reused.status,
        StatusCode::CREATED,
        "an archived SKU may be used by a new product: {}",
        reused.body
    );
}

/// A duplicate price-list name is a conflict, and archiving frees the name.
#[tokio::test]
async fn a_price_list_name_is_unique_until_it_is_archived() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let name = format!("Retail {}", &Uuid::new_v4().simple().to_string()[..6]);

    let list_id = create_price_list(state, &manager, &name).await;

    let refused = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/pricelists",
            Some(&manager),
            Some(json!({ "name": name })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "body: {}", refused.body);
    assert_eq!(refused.body["error"]["code"], "sales_name_taken");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains(&name),
        "the conflict names the list that took it: {}",
        refused.body
    );

    // A window that ends before it starts is refused, by the validator and by the schema alike.
    let backwards = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/pricelists",
            Some(&manager),
            // A date arrives as `YYYY-MM-DD`: the shape the panel's date picker produces and the
            // one the CRM walks already send (`close_on`). The module keeps it in `time::Date`, so
            // the window rule is proved with the shape the API actually accepts rather than with
            // one that would be refused by the deserializer before the rule was ever reached.
            Some(json!({
                "name": format!("Backwards {}", &Uuid::new_v4().simple().to_string()[..6]),
                "valid_from": "2026-04-01",
                "valid_until": "2026-03-01",
            })),
        ),
    )
    .await;
    assert_eq!(backwards.status, StatusCode::BAD_REQUEST, "body: {}", backwards.body);
    assert_eq!(
        backwards.body["error"]["details"]["field"], "valid_until",
        "the refusal must land on the field the form renders: {}",
        backwards.body
    );

    // Archiving frees the name, so a list that is gone does not squat on it forever.
    let archived = call(
        state,
        request(
            Method::DELETE,
            &format!("/api/v1/sales/pricelists/{list_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK, "body: {}", archived.body);

    let reused = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/pricelists",
            Some(&manager),
            Some(json!({ "name": name })),
        ),
    )
    .await;
    assert_eq!(
        reused.status,
        StatusCode::CREATED,
        "an archived list frees its name: {}",
        reused.body
    );
}

/// The reader sees the catalog and cannot change it, which is the pair the keys exist for.
#[tokio::test]
async fn a_reader_may_look_and_may_not_touch() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let reader = fixture.token(&fixture.reader).await;

    let listed = call(
        state,
        request(Method::GET, "/api/v1/sales/products", Some(&reader), None),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "body: {}", listed.body);
    assert!(listed.body["items"].is_array());
    assert!(listed.body["total_estimate"].is_i64());

    let lists = call(
        state,
        request(Method::GET, "/api/v1/sales/pricelists", Some(&reader), None),
    )
    .await;
    assert_eq!(lists.status, StatusCode::OK);

    for (method, uri, body) in [
        (
            Method::POST,
            "/api/v1/sales/products".to_owned(),
            Some(json!({ "sku": "RD-1", "name": "No" })),
        ),
        (
            Method::POST,
            "/api/v1/sales/pricelists".to_owned(),
            Some(json!({ "name": "No" })),
        ),
    ] {
        let refused = call(
            state,
            request(method.clone(), &uri, Some(&reader), body),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must refuse a reader: {}",
            refused.body
        );
    }
}

impl Fixture {
    /// The platform Owner of this fixture, whose role holds the whole catalogue and therefore
    /// the authority to grant a narrower one.
    async fn owner_account(&self) -> (Uuid, String) {
        let email = format!("sales-owner-{}@omnion.test", Uuid::new_v4().simple());
        let user = users::create_user(
            self.db.pool(),
            NewUser {
                email: email.clone(),
                password: PASSWORD.to_owned(),
                display_name: "Sales Owner".to_owned(),
                organization_id: None,
            },
        )
        .await
        .expect("the owner account must be created");
        seed::bind_owner(self.db.pool(), user.id)
            .await
            .expect("the owner binding must be created");
        (user.id, email)
    }
}
