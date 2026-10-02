//! Integration tests for the sales-order reservation (docs/requests/REQ-053, slice 5).
//!
//! The criterion is one sentence — *"`sales.order.confirmed` reserves stock and the reservation is
//! visible in both the order and the stock list; cancel releases it"* — and it was false in a way
//! that no screen could show. `reserve` and `release` were movement kinds with correct arithmetic,
//! `sales_order_reservations` was the sales module's own table, and confirming an order wrote the
//! sales-side rows without touching `inventory_stock` at all.
//!
//! So every walk here is written to fail for the reason the criterion names, and the first of them
//! is the one that matters:
//!
//! * **`a_confirmed_order_holds_stock_and_the_stock_list_shows_it`** — the criterion's whole
//!   content. The walk confirms an order and then reads the **stock list**, the screen a warehouse
//!   actually works from, rather than the order. A bridge that wrote the sales table perfectly
//!   would pass an assertion on the order and fail this one, which is the point: the order was
//!   never where the gap was visible.
//! * **`a_cancelled_order_gives_the_stock_back_and_the_balance_returns`** — "cancel releases
//!   it". The assertion is that `available` returns to its starting figure, so a release that
//!   took too much or too little cannot pass.
//! * **`the_stock_that_cannot_be_promised_is_named_rather_than_silently_held`** — the honesty
//!   half. A line asking for more than any shelf has must report as unheld **with a sentence**,
//!   and the order must not claim a total hold. A bridge that clamped the quantity to what was
//!   available would hold real stock, print a clean `total`, and promise a number nobody can
//!   deliver.
//! * **`a_line_with_no_inventory_item_is_held_nowhere_and_says_so`** — the spec's "a visible note
//!   rather than a silent failure". This is the case the sales module's own migration `0057` was
//!   designed for, and the one that hides the gap hardest: a services order has nothing to put on
//!   a shelf, so the hold is recorded and the state stays `partial`.
//! * **`a_second_confirm_does_not_hold_the_stock_twice`** — the double-click. The unique index
//!   on the sales table makes the *sales* row idempotent, and the assertion here is on the
//!   **ledger** and the balance, because a bridge that re-ran the spread would hold it twice while
//!   every other table looked perfect.
//! * **`the_ledger_still_replays_after_a_reservation`** — the module's own invariant. A hold is
//!   the one write that touches `reserved` without touching `on_hand`, and `replay` is what
//!   proves the rollup still matches the ledger. A bridge that wrote the rollup directly would
//!   pass every other walk in this file and fail this one.
//! * **`another_organizations_order_is_not_a_stock_source`** — the tenancy clause. A sales order
//!   of another tenant is a `404`, the same answer a foreign item gets, because one
//!   organization's stock is the thing this module keeps apart.


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
static RESERVATION_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An operator: the warehouse's keys, **including the sales desk's**, because a reservation is
/// only reachable through a confirm and a confirm is `sales.orders.confirm`.
///
/// Both families are listed explicitly rather than assumed. A suite that granted "everything"
/// would keep passing if `inventory.movements.record` or `sales.orders.confirm` were never
/// registered — and a permission that guards nothing is a comment.
///
/// The list is the **real** vocabulary, read out of `crates/permissions/src/catalogue.rs`, and it
/// cost two rounds to get right: `inventory.stock.read` and `sales.products.create` are both the
/// obvious guesses and neither exists. The stock list is behind `inventory.items.read` — which is
/// why the reader below can read the same rows this file asserts on, since the balance is visible
/// to anybody who may look at the shelf — and the product write key is `sales.products.manage`.
/// A permission suite that names a key the platform does not have is not a weaker test, it is a
/// test that cannot run at all, and the failure names a typo rather than a missing guard.
const OPERATOR_PERMISSIONS: [&str; 10] = [
    "inventory.items.read",
    "inventory.items.manage",
    "inventory.movements.record",
    "inventory.locations.manage",
    "sales.products.read",
    "sales.products.manage",
    "sales.orders.read",
    "sales.orders.create",
    "sales.orders.confirm",
    "sites.read",
];

/// A reader. Note what is **absent**: every write key. Reading the stock list and reading an
/// order are different powers from promising one, and the criterion is proved with exactly this
/// difference.
const READER_PERMISSIONS: [&str; 3] = ["inventory.items.read", "sales.orders.read", "sites.read"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    /// **Every** `Set-Cookie` the response carried, in order.
    set_cookies: Vec<String>,
    body: Value,
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let set_cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect::<Vec<_>>();
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

    TestResponse { status, set_cookies, body }
}

/// The value of one cookie from a response, or `None`.
fn cookie_value(response: &TestResponse, name: &str) -> Option<String> {
    response.set_cookies.iter().find_map(|raw| {
        let pair = raw.split(';').next()?.trim();
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.to_owned())
    })
}

/// Build a JSON request; `token` becomes the session cookie and `body` the payload.
fn request(
    method: Method,
    uri: &str,
    session: Option<&Session>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match session {
        Some(jar) => builder.header(header::COOKIE, jar.to_string()),
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
            // This suite's subject is a **balance**, so a quiet SKIP is a green tick that proved
            // nothing. `OMNION_REQUIRE_DB=1` turns the skip into a failure.
            if std::env::var("OMNION_REQUIRE_DB").as_deref() == Ok("1") {
                panic!("OMNION_REQUIRE_DB=1 and PostgreSQL is not reachable: {err}");
            }
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

/// A state whose database has all migrations applied and the IAM seed loaded.
///
/// **The CSRF secret gets a test-only default here**, and the reason is worth stating: the CSRF
/// layer (merged from main) refuses every cookie-authenticated mutation when
/// `OMNION_CSRF_SECRET` is unset, which would turn every write walk in this file into an
/// assertion about a missing deployment key. Setting it in the harness means `cargo test` proves
/// what it is supposed to prove on a bare checkout.
async fn live_state() -> Option<(AppState, Db)> {
    if std::env::var("OMNION_CSRF_SECRET").is_err() {
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OMNION_CSRF_SECRET", "inventory-reservation-suite-csrf-secret");
        }
    }
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

/// One organization, an operator and a reader.
struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    org: Uuid,
    operator: String,
    reader: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = RESERVATION_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org = create_organization_row(&db).await;
        let (owner_id, _owner) = create_account(&db, None, "Reservation Owner").await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        let (operator_id, operator) = create_account(&db, Some(org), "Reservation Operator").await;
        grant(&db, org, operator_id, owner_id, &OPERATOR_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(org), "Reservation Reader").await;
        grant(&db, org, reader_id, owner_id, &READER_PERMISSIONS).await;

        Some(Self { _walk: walk, state, db, org, operator, reader })
    }

    async fn token(&self, email: &str) -> Session {
        login(&self.state, email).await
    }

    /// A location's id, by its code in the seeded `MAIN` warehouse.
    async fn location(&self, code: &str) -> Uuid {
        sqlx::query_scalar(
            "select l.id from inventory_locations l join inventory_warehouses w on w.id = l.warehouse_id \
             where l.organization_id = $1 and l.code = $2",
        )
        .bind(self.org)
        .bind(code)
        .fetch_one(self.db.pool())
        .await
        .unwrap_or_else(|err| panic!("the {code} location must exist: {err}"))
    }
}

/// Create an organization row with a unique slug.
async fn create_organization_row(db: &Db) -> Uuid {
    let slug = format!("reservation-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind("Reservation Test Co")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("reservation-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("reservation-role-{}", Uuid::new_v4().simple()),
            name: "Reservation Test Role".to_owned(),
            description: "A role of the reservation suite".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    let entries: Vec<RolePermissionInput> = permissions
        .iter()
        .map(|key| RolePermissionInput { key: (*key).to_owned(), effect: Effect::Allow })
        .collect();
    role_store::set_role_permissions(db.pool(), role.id, &entries)
        .await
        .expect("the role permissions must be stored");

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

/// A session cookie jar: the session token and the CSRF token, as a browser would hold them.
#[derive(Clone)]
struct Session {
    /// The value of `omnion_session`.
    session: String,
    /// The value of `omnion_csrf`.
    csrf: String,
}

impl std::fmt::Display for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "omnion_session={}; omnion_csrf={}", self.session, self.csrf)
    }
}

/// Exchange credentials for a session jar.
///
/// **Both cookies, or the writes are refused** — the CSRF layer is doing its job, not the test.
///
/// The token is derived here rather than read from a cookie because of a gap on `main` worth
/// naming: `POST /auth/login` issues only the session cookie, so a real browser cannot write
/// anything either. The assertion below is the note that says when to delete this workaround.
async fn login(state: &AppState, email: &str) -> Session {
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
    assert_eq!(response.status, StatusCode::OK, "login body: {}", response.body);
    let session = cookie_value(&response, "omnion_session")
        .expect("login must set the session cookie");
    let csrf = csrf_token_for(state, &session).await;
    assert!(
        cookie_value(&response, "omnion_csrf").is_none(),
        "login now issues a CSRF cookie — replace the derivation in `login` with \
         `cookie_value(&response, \"omnion_csrf\")` and delete `csrf_token_for`"
    );
    Session { session, csrf }
}

/// The token the CSRF middleware expects for a session, computed the way it computes it.
///
/// The session's **id**, not its token: `derive_token` is an HMAC over the id, and a harness
/// that hashed the cookie value would fail every write and the failure would read like a
/// permission problem.
async fn csrf_token_for(state: &AppState, session_token: &str) -> String {
    let session = omnion_identity::sessions::resolve_session(state.db().pool(), session_token)
        .await
        .expect("the session must resolve right after a login")
        .expect("the session row must exist right after a login");

    let secret = state
        .config()
        .csrf
        .as_bytes()
        .expect("live_state sets OMNION_CSRF_SECRET before the config is read")
        .to_vec();
    omnion_security::derive_csrf_token(&secret, &session.session.id.to_string())
}

/// A SKU unique to one test, capped at the schema's 32 characters.
fn sku(label: &str) -> String {
    format!("{}-{}", label, &Uuid::new_v4().simple().to_string()[..8])
}

/// Create an item and return its id.
async fn create_item(state: &AppState, token: &Session, mut body: Value) -> Uuid {
    let object = body.as_object_mut().expect("an object");
    if !object.contains_key("sku") {
        object.insert("sku".to_owned(), json!(sku("ST")));
    }
    let response = call(
        state,
        request(Method::POST, "/api/v1/inventory/items", Some(token), Some(body)),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "the item must be created: {}", response.body);
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
}

/// An item that does not interfere with what is being counted.
fn item_body() -> Value {
    json!({
        "name": "Washer M8",
        "unit": "piece",
        "min_threshold": "0",
        "reorder_point": "0",
        "reorder_qty": "10",
    })
}

/// Put stock on a shelf, failing loudly with the body.
async fn receive(
    state: &AppState,
    token: &Session,
    item_id: Uuid,
    location_id: Uuid,
    quantity: &str,
) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/movements",
            Some(token),
            Some(json!({
                "item_id": item_id,
                "location_id": location_id,
                "quantity": quantity,
                "reason": "purchase_receipt",
            })),
        ),
    )
    .await
}


// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// A catalog product, which is what an order line points at.
async fn create_product(state: &AppState, token: &Session, item_sku: &str) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/products",
            Some(token),
            Some(json!({
                "sku": item_sku,
                "name": "Steel bracket",
                "unit": "piece",
                "default_price": "12.50",
            })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the catalog product must be created so the order has something to name: {}",
        response.body
    );
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
}

/// A draft order with the given lines, and its id.
async fn create_order(state: &AppState, token: &Session, lines: Value) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/orders",
            Some(token),
            Some(json!({
                "customer_name": "Yıldız Makina A.Ş.",
                "lines": lines,
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "the order must be created: {}", response.body);
    Uuid::parse_str(response.body["order"]["id"].as_str().expect("an order id")).expect("an id")
}

/// One order line naming a product.
fn product_line(product_id: Uuid, quantity: &str) -> Value {
    json!({
        "product_id": product_id,
        "quantity": quantity,
        "unit_price": "12.50",
    })
}

async fn confirm(state: &AppState, token: &Session, order_id: Uuid) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/confirm"),
            Some(token),
            None,
        ),
    )
    .await
}

async fn cancel(state: &AppState, token: &Session, order_id: Uuid, reason: &str) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/cancel"),
            Some(token),
            Some(json!({ "reason": reason })),
        ),
    )
    .await
}

/// The stock list's own answer for one item, as `(on_hand, reserved, available)` in text.
///
/// Read through the **API the screen reads**, not through SQL, for the walks whose whole claim is
/// "visible in the stock list". Reading the rollup directly would let a bridge pass while the
/// screen it feeds draws something else — which is the same mistake as asserting on the order.
async fn stock_row(state: &AppState, token: &Session, item_id: Uuid) -> (String, String, String) {
    let response = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/stock?item_id={item_id}&limit=50"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the stock list must answer: {}", response.body);
    let row = response.body["items"]
        .as_array()
        .expect("the stock list has items")
        .iter()
        .find(|row| row["item_id"] == json!(item_id))
        .unwrap_or_else(|| panic!("item {item_id} must be on the stock list: {}", response.body));
    (
        row["on_hand"].as_str().expect("on_hand text").to_owned(),
        row["reserved"].as_str().expect("reserved text").to_owned(),
        row["available"].as_str().expect("available text").to_owned(),
    )
}

/// Every ledger row this order wrote, in order.
async fn order_movements(db: &Db, organization_id: Uuid, order_id: Uuid) -> Vec<(String, String)> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select kind, quantity::text from inventory_movements \
         where organization_id = $1 and source_kind = 'order' and source_id = $2 order by id",
    )
    .bind(organization_id)
    .bind(order_id)
    .fetch_all(db.pool())
    .await
    .expect("the ledger must read");
    rows
}

/// The order detail, as the order screen would render it.
async fn order_detail(state: &AppState, token: &Session, order_id: Uuid) -> Value {
    let response = call(
        state,
        request(Method::GET, &format!("/api/v1/sales/orders/{order_id}"), Some(token), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the order must read: {}", response.body);
    response.body
}

#[tokio::test]
#[ignore = "needs PostgreSQL; run with --ignored --test-threads=1"]
async fn a_confirmed_order_holds_stock_and_the_stock_list_shows_it() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.operator).await;
    let shelf = fixture.location("STOCK").await;

    let item_sku = sku("RSV");
    let item_id = create_item(&fixture.state, &token, {
        let mut body = item_body();
        body["sku"] = json!(item_sku.clone());
        body
    })
    .await;
    let product_id = create_product(&fixture.state, &token, &item_sku).await;
    receive(&fixture.state, &token, item_id, shelf, "10.000").await;

    // Before: ten on the shelf, none of it promised.
    let (on_hand, reserved, available) = stock_row(&fixture.state, &token, item_id).await;
    assert_eq!((on_hand.as_str(), reserved.as_str(), available.as_str()), ("10.000", "0.000", "10.000"));

    let order_id = create_order(&fixture.state, &token, json!([product_line(product_id, "4.000")])).await;
    let confirmed = confirm(&fixture.state, &token, order_id).await;
    assert_eq!(confirmed.status, StatusCode::OK, "the confirm must succeed: {}", confirmed.body);

    // The assertion that the whole slice exists for: read the **stock list**, not the order.
    // Before the bridge, this row still said `reserved 0.000` while the order detail showed a
    // hold — the two screens the criterion names, disagreeing, with nothing red on either.
    let (on_hand, reserved, available) = stock_row(&fixture.state, &token, item_id).await;
    assert_eq!(
        (on_hand.as_str(), reserved.as_str(), available.as_str()),
        ("10.000", "4.000", "6.000"),
        "confirming an order must take 4 off the shelf's available balance, in the stock list itself"
    );

    // And it must be on the ledger, or the warehouse reading the movement screen sees nothing.
    let movements = order_movements(&fixture.db, fixture.org, order_id).await;
    assert_eq!(movements, vec![("reserve".to_owned(), "4.000".to_owned())]);

    // The order detail shows the same hold, so the two screens agree.
    let detail = order_detail(&fixture.state, &token, order_id).await;
    let line = &detail["lines"][0];
    assert!(
        line["reservation"].is_object(),
        "the order line must show its hold, the criterion's first half: {detail}"
    );
    assert_eq!(line["reservation"]["quantity"], "4.000");
    assert_eq!(line["reservation"]["state"], "held");
}

#[tokio::test]
#[ignore = "needs PostgreSQL; run with --ignored --test-threads=1"]
async fn a_cancelled_order_gives_the_stock_back_and_the_balance_returns() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.operator).await;
    let shelf = fixture.location("STOCK").await;

    let item_sku = sku("RSVC");
    let mut body = item_body();
    body["sku"] = json!(item_sku.clone());
    let item_id = create_item(&fixture.state, &token, body).await;
    let product_id = create_product(&fixture.state, &token, &item_sku).await;
    receive(&fixture.state, &token, item_id, shelf, "10.000").await;

    let order_id = create_order(&fixture.state, &token, json!([product_line(product_id, "4.000")])).await;
    confirm(&fixture.state, &token, order_id).await;
    let (_, reserved, available) = stock_row(&fixture.state, &token, item_id).await;
    assert_eq!((reserved.as_str(), available.as_str()), ("4.000", "6.000"));

    let cancelled = cancel(&fixture.state, &token, order_id, "customer went with another supplier").await;
    assert_eq!(cancelled.status, StatusCode::OK, "the cancel must succeed: {}", cancelled.body);

    // "Cancel releases it" means the *balance* returns, not that some row was written. A release
    // of the wrong amount passes a "did a release happen" check and fails here.
    let (on_hand, reserved, available) = stock_row(&fixture.state, &token, item_id).await;
    assert_eq!(
        (on_hand.as_str(), reserved.as_str(), available.as_str()),
        ("10.000", "0.000", "10.000"),
        "a cancelled order must put the shelf back exactly where it was"
    );

    // The ledger tells the story in both directions, which is what makes a release auditable.
    let movements = order_movements(&fixture.db, fixture.org, order_id).await;
    assert_eq!(
        movements,
        vec![
            ("reserve".to_owned(), "4.000".to_owned()),
            ("release".to_owned(), "4.000".to_owned()),
        ]
    );
}

#[tokio::test]
#[ignore = "needs PostgreSQL; run with --ignored --test-threads=1"]
async fn the_stock_that_cannot_be_promised_is_named_rather_than_silently_held() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.operator).await;
    let shelf = fixture.location("STOCK").await;

    let item_sku = sku("RSVO");
    let mut body = item_body();
    body["sku"] = json!(item_sku.clone());
    let item_id = create_item(&fixture.state, &token, body).await;
    let product_id = create_product(&fixture.state, &token, &item_sku).await;
    // Three on the shelf, and the order asks for ten.
    receive(&fixture.state, &token, item_id, shelf, "3.000").await;

    let order_id = create_order(&fixture.state, &token, json!([product_line(product_id, "10.000")])).await;
    let confirmed = confirm(&fixture.state, &token, order_id).await;
    assert_eq!(confirmed.status, StatusCode::OK, "confirming must not be refused for a shortage: {}", confirmed.body);

    // The honest outcome: hold what exists, and say what could not be held. A bridge that clamped
    // the quantity to three would pass every balance assertion in this file and leave a customer
    // promised ten.
    let (on_hand, reserved, available) = stock_row(&fixture.state, &token, item_id).await;
    assert_eq!((on_hand.as_str(), reserved.as_str(), available.as_str()), ("3.000", "3.000", "0.000"));

    // The audit row names the line, because "2 of 5 lines are short" is a number and only a
    // sentence is something a person acts on before the next confirmation.
    let audit: Vec<Value> = sqlx::query_scalar(
        "select metadata from audit_log where organization_id = $1 and target_id = $2 \
         order by created_at desc limit 1",
    )
    .bind(fixture.org)
    .bind(order_id.to_string())
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit log must read");
    let metadata = &audit
        .first()
        .expect("confirming must write an audit row")["unheld_lines"];
    let unheld = metadata.as_array().expect("an array of lines");
    assert_eq!(unheld.len(), 1, "the short line must be named: {metadata}");
    let reason = unheld[0]["reason"].as_str().expect("a sentence");
    assert!(
        reason.contains("only 3.000 of 10.000"),
        "the sentence must carry both numbers the seller needs: {reason}"
    );
}

#[tokio::test]
#[ignore = "needs PostgreSQL; run with --ignored --test-threads=1"]
async fn a_line_with_no_inventory_item_is_held_nowhere_and_says_so() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.operator).await;

    // A product in the catalog that was never made an inventory item — the exact case migration
    // 0057 was designed around, and the one that hides this gap hardest, because a free-text or
    // unlinked line has no shelf to contradict the order on.
    let product_id = create_product(&fixture.state, &token, &sku("NOSHELF")).await;
    let order_id = create_order(&fixture.state, &token, json!([product_line(product_id, "2.000")])).await;

    let confirmed = confirm(&fixture.state, &token, order_id).await;
    assert_eq!(confirmed.status, StatusCode::OK, "confirming must still succeed: {}", confirmed.body);

    let audit: Vec<Value> = sqlx::query_scalar(
        "select metadata from audit_log where organization_id = $1 and target_id = $2 \
         order by created_at desc limit 1",
    )
    .bind(fixture.org)
    .bind(order_id.to_string())
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit log must read");
    let metadata = &audit.first().expect("an audit row")["unheld_lines"];
    let unheld = metadata.as_array().expect("an array");
    assert_eq!(unheld.len(), 1, "the line with no shelf must be named: {metadata}");
    let reason = unheld[0]["reason"].as_str().expect("a sentence");
    assert!(
        reason.contains("create it before promising"),
        "the sentence must tell somebody what to do about it: {reason}"
    );

    // And nothing was written to a shelf, so the order does not claim a total hold.
    let movements = order_movements(&fixture.db, fixture.org, order_id).await;
    assert!(
        movements.is_empty(),
        "a line with no inventory item must not write a phantom hold: {movements:?}"
    );
}

#[tokio::test]
#[ignore = "needs PostgreSQL; run with --ignored --test-threads=1"]
async fn a_second_confirm_does_not_hold_the_stock_twice() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.operator).await;
    let shelf = fixture.location("STOCK").await;

    let item_sku = sku("RSVD");
    let mut body = item_body();
    body["sku"] = json!(item_sku.clone());
    let item_id = create_item(&fixture.state, &token, body).await;
    let product_id = create_product(&fixture.state, &token, &item_sku).await;
    receive(&fixture.state, &token, item_id, shelf, "10.000").await;

    let order_id = create_order(&fixture.state, &token, json!([product_line(product_id, "4.000")])).await;
    confirm(&fixture.state, &token, order_id).await;
    // The double-click, and the retried request — both are ordinary, and both must be no-ops.
    let second = confirm(&fixture.state, &token, order_id).await;
    assert_eq!(second.status, StatusCode::OK, "a second confirm is a no-op, not a conflict: {}", second.body);

    // The unique index on the *sales* table already makes the sales row idempotent, so the only
    // way this fails is a bridge that re-runs the spread. The balance is the assertion.
    let (_, reserved, available) = stock_row(&fixture.state, &token, item_id).await;
    assert_eq!(
        (reserved.as_str(), available.as_str()),
        ("4.000", "6.000"),
        "confirming twice must not hold the stock twice"
    );
    let movements = order_movements(&fixture.db, fixture.org, order_id).await;
    assert_eq!(movements.len(), 1, "exactly one hold must exist: {movements:?}");
}

#[tokio::test]
#[ignore = "needs PostgreSQL; run with --ignored --test-threads=1"]
async fn the_ledger_still_replays_after_a_reservation() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.operator).await;
    let shelf = fixture.location("STOCK").await;

    let item_sku = sku("RSVR");
    let mut body = item_body();
    body["sku"] = json!(item_sku.clone());
    let item_id = create_item(&fixture.state, &token, body).await;
    let product_id = create_product(&fixture.state, &token, &item_sku).await;
    receive(&fixture.state, &token, item_id, shelf, "10.000").await;

    let order_id = create_order(&fixture.state, &token, json!([product_line(product_id, "4.000")])).await;
    confirm(&fixture.state, &token, order_id).await;
    cancel(&fixture.state, &token, order_id, "no longer needed").await;

    // A hold is the one write in this module that moves `reserved` and leaves `on_hand` alone, so
    // it is the one the replay has to agree about. A bridge that updated the rollup directly would
    // pass every other walk in this file and fail here — and so would a replay that added a
    // reservation to the shelf, which is exactly what the old one did.
    let replayed = omnion_module_inventory::ledger::replay(fixture.db.pool(), fixture.org)
        .await
        .expect("the ledger must replay");
    let row_id: Uuid = sqlx::query_scalar(
        "select id from inventory_stock where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(fixture.org)
    .bind(item_id)
    .bind(shelf)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the stock row must exist");
    let position = replayed
        .get(&row_id)
        .expect("a replayed stock row");
    assert_eq!(
        position.on_hand.to_text(),
        "10.000",
        "a hold must not move the physical count, in the replay or in the rollup"
    );
    assert_eq!(
        position.reserved.to_text(),
        "0.000",
        "a reserve followed by a release nets to zero, and the replay has to see both"
    );

    // And the module's own reconciliation agrees, which is the check a client would run.
    let report = omnion_module_inventory::store::reconciliation_report(fixture.db.pool(), fixture.org)
        .await
        .expect("the reconciliation must run");
    assert!(
        report.mismatches.is_empty(),
        "a reservation must leave the ledger and the rollup in agreement: {report:?}"
    );
}

#[tokio::test]
#[ignore = "needs PostgreSQL; run with --ignored --test-threads=1"]
async fn another_organizations_order_is_not_a_stock_source() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.operator).await;
    let shelf = fixture.location("STOCK").await;

    // Stock, and an order, in *this* organization — so the walk is about the bridge's tenancy and
    // not about a shortage.
    let item_sku = sku("RSVT");
    let mut body = item_body();
    body["sku"] = json!(item_sku.clone());
    let item_id = create_item(&fixture.state, &token, body).await;
    let product_id = create_product(&fixture.state, &token, &item_sku).await;
    receive(&fixture.state, &token, item_id, shelf, "10.000").await;

    // An order belonging to a *different* organization, written straight into its tables. The
    // `position` is 1, not 0: `sales_order_lines_position_positive` says so, and the walk that
    // guessed 0 failed on the constraint before it ever reached the bridge — a fixture mistake
    // reported as a database error, which is a bad way to spend a cycle.
    let other_org = create_organization_row(&fixture.db).await;
    let other_order: Uuid = sqlx::query_scalar(
        "insert into sales_orders (organization_id, number, status) values ($1, 'SO-OTHER-1', 'draft') \
         returning id",
    )
    .bind(other_org)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the foreign order must be created");
    sqlx::query(
        "insert into sales_order_lines (organization_id, order_id, position, description, unit, \
                quantity, unit_price) \
         values ($1, $2, 1, 'Steel bracket', 'piece', 4, 12.50)",
    )
    .bind(other_org)
    .bind(other_order)
    .execute(fixture.db.pool())
    .await
    .expect("the foreign line must be created");

    // Calling the bridge directly is the honest way to prove this: the route cannot reach a
    // foreign order (the sales module refuses first), but the bridge is what reads the order's
    // rows, and it is the bridge that has to refuse.
    let result = omnion_module_inventory::reserve_for_order(
        fixture.db.pool(),
        fixture.org,
        other_order,
        omnion_module_inventory::ReservationAction::Reserve,
        None,
    )
    .await;
    assert!(
        matches!(result, Err(omnion_module_inventory::InventoryError::NotFound("order"))),
        "another organization's order must be a 404, not a stock source: {result:?}"
    );

    // Nothing moved, and the proof is the balance rather than the absence of an error.
    let (_, reserved, available) = stock_row(&fixture.state, &token, item_id).await;
    assert_eq!(
        (reserved.as_str(), available.as_str()),
        ("0.000", "10.000"),
        "a foreign order must not hold anything here"
    );
}
