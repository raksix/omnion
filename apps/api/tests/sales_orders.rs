//! Integration tests for the order chain (docs/requests/REQ-052, slice 4).
//!
//! The chain is the part of sales a person acts on with real consequences — stock is held, stock
//! is given back, a document goes to accounting — so these walks are written around **what
//! somebody would try to do**, not around the endpoints:
//!
//! * an accepted quote becomes an order whose lines and totals are the quote's, and a **second**
//!   conversion returns that same order rather than a second delivery;
//! * a draft quote cannot become an order, and the refusal names the quote;
//! * confirming takes a hold on every line, and the order detail shows it;
//! * **confirming twice is a no-op** — the same order, the same holds, no second promise in the
//!   timeline — because a person who pressed the button after a slow response is not an attacker;
//! * cancelling needs a reason, releases every hold, records the release, and makes the order
//!   unconfirmable afterwards;
//! * an invoice draft freezes the order's money, and asking twice returns the draft that exists;
//! * a draft order cannot be invoiced, and a cancelled order never is;
//! * a reader who may **see** the deliveries may not confirm one, and every order route answers
//!   401 without a session, 403 without the permission, and **404, never 403** for another
//!   organization's order.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{
    Effect, NewBinding, NewRole, RolePermissionInput, Scope as PermScope,
};
use omnion_permissions::{roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// Serialises this suite: the organizations and the IAM seed are shared state.
static ORDERS_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// May draft a quote, convert it to an order, and read the deliveries — but **not** confirm,
/// cancel or invoice one. This is the role the acceptance criteria are about, because a person
/// who can promise stock they have not checked is the failure this slice exists to prevent.
const DRAFTER_PERMISSIONS: [&str; 10] = [
    "sales.quotes.read",
    "sales.quotes.create",
    "sales.quotes.update",
    "sales.orders.read",
    "sales.orders.create",
    "sales.products.read",
    "sales.products.manage",
    // The fixture creates its own company and product through the API, so the account that runs
    // the walk needs to be allowed to create them — a test that cannot reach its own fixture
    // fails at line 1 and tells the reader nothing about orders.
    "crm.contacts.read",
    "crm.contacts.create",
    "sites.read",
];

/// May send, and may run every transition that **commits the organization** — confirm, cancel,
/// invoice draft. It holds no `sales.quotes.update`, which is the point: the power a buyer has is
/// over the delivery, not over the draft it came from.
const MANAGER_PERMISSIONS: [&str; 7] = [
    "sales.quotes.read",
    "sales.quotes.send",
    "sales.orders.read",
    "sales.orders.create",
    "sales.orders.confirm",
    "sales.products.read",
    "sites.read",
];

/// A reader: may **see** the deliveries and nothing else. This is the role that proves the split
/// between `sales.orders.read` and `sales.orders.confirm` — a person who may look at what was
/// promised may not promise it.
const READER_PERMISSIONS: [&str; 5] = [
    "sales.orders.read",
    "sales.quotes.read",
    "sales.products.read",
    "crm.contacts.read",
    "sites.read",
];

/// A writer in a second organization holding the **full** order permission set, for the
/// cross-tenant `404`.
const FOREIGN_PERMISSIONS: [&str; 6] = [
    "sales.orders.read",
    "sales.orders.create",
    "sales.orders.confirm",
    "sales.quotes.read",
    "sales.products.read",
    "sites.read",
];

// ---------------------------------------------------------------------------------------------
// Harness (the same shape sales_quotes.rs uses — a reader who knows that walk knows this one)
// ---------------------------------------------------------------------------------------------

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
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };
    TestResponse {
        status,
        set_cookie,
        body,
    }
}

fn request(
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
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
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

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

struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    organization: Uuid,
    owner_id: Uuid,
    company: Uuid,
    product: Uuid,
    drafter: String,
    manager: String,
    reader: String,
    foreign: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = ORDERS_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.ok()?;

        let organization = create_organization_row(&db, "orders").await;
        let other_org = create_organization_row(&db, "orders-foreign").await;

        let (owner_id, _) = create_account(&db, None, "Orders Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (drafter_id, drafter) = create_account(&db, Some(organization), "Orders Drafter").await;
        grant(&db, organization, drafter_id, owner_id, &DRAFTER_PERMISSIONS).await;

        let (manager_id, manager) = create_account(&db, Some(organization), "Orders Manager").await;
        grant(&db, organization, manager_id, owner_id, &MANAGER_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(organization), "Orders Reader").await;
        grant(&db, organization, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (foreign_id, foreign) =
            create_account(&db, Some(other_org), "Orders Foreign").await;
        grant(&db, other_org, foreign_id, owner_id, &FOREIGN_PERMISSIONS).await;

        let drafter_token = login(&state, &drafter).await;
        let company = create_company(&state, &drafter_token).await;
        let product = create_product(&state, &drafter_token).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            owner_id,
            company,
            product,
            drafter,
            manager,
            reader,
            foreign,
        })
    }

    async fn token(&self, email: &str) -> String {
        login(&self.state, email).await
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("orders-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Orders Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("orders-{}@omnion.test", Uuid::new_v4().simple());
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

async fn grant(
    db: &Db,
    organization_id: Uuid,
    user_id: Uuid,
    granted_by: Uuid,
    permissions: &[&str],
) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("orders-role-{}", Uuid::new_v4().simple()),
            name: "Orders Test Role".to_owned(),
            description: "A role of the orders walk".to_owned(),
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
    assert_eq!(response.status, StatusCode::OK, "login body: {}", response.body);
    response
        .set_cookie
        .and_then(|cookie| {
            cookie
                .split(';')
                .next()?
                .split_once('=')
                .map(|(_, value)| value.to_string())
        })
        .expect("a session cookie")
}

async fn create_company(state: &AppState, token: &str) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/companies",
            Some(token),
            Some(json!({ "name": "Gate Test Co" })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "company: {}", response.body);
    Uuid::parse_str(response.body["id"].as_str().expect("a company id")).expect("a uuid")
}

async fn create_product(state: &AppState, token: &str) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/products",
            Some(token),
            Some(json!({ "sku": "GATE-1", "name": "Gate widget", "default_price": "100.00" })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "product: {}", response.body);
    Uuid::parse_str(response.body["id"].as_str().expect("a product id")).expect("a uuid")
}

/// A quote whose largest line discount is `discount`, against the default 15% threshold.
fn quote_with_discount(company: Uuid, product: Uuid, discount: i64) -> Value {
    json!({
        "customer_id": company,
        "customer_type": "company",
        "customer_name": "Gate Test Co",
        "title": "Gate quote",
        "currency": "TRY",
        "lines": [
            { "product_id": product, "description": "Widget", "quantity": "2",
              "unit_price": "100.00", "discount_percent": discount, "tax_percent": 20 }
        ]
    })
}

async fn create_quote(fixture: &Fixture, token: &str, body: Value) -> Uuid {
    let response = call(
        &fixture.state,
        request(Method::POST, "/api/v1/sales/quotes", Some(token), Some(body)),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "quote: {}", response.body);
    Uuid::parse_str(response.body["quote"]["id"].as_str().expect("a quote id")).expect("a uuid")
}

async fn quote_status(fixture: &Fixture, token: &str, quote_id: Uuid) -> String {
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/quotes/{quote_id}"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "quote read: {}", response.body);
    response.body["quote"]["status"]
        .as_str()
        .expect("a status")
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------


// ---------------------------------------------------------------------------------------------
// Fixtures for the chain
// ---------------------------------------------------------------------------------------------

/// A quote with two lines, sent and then accepted through the public link.
///
/// The acceptance is the honest way in: a quote is `accepted` because a customer said yes, and
/// the walk that converts it should have had to do what a person does.
///
/// It takes **two** tokens on purpose. The drafter role has no `sales.quotes.send` — that is the
/// role the acceptance criteria are about — so the quote is written by one person and put in the
/// customer's hands by another, which is how the split between drafting and sending reads in
/// practice. A fixture that quietly gave the drafter the send key would stop proving it.
async fn accepted_quote(fixture: &Fixture, drafter: &str, sender: &str) -> Uuid {
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sales/quotes",
            Some(drafter),
            Some(json!({
                "customer_id": fixture.company,
                "customer_type": "company",
                "customer_name": "Orders Test Co",
                "title": "Order chain quote",
                "currency": "TRY",
                "lines": [
                    { "product_id": fixture.product, "description": "Widget", "quantity": "2",
                      "unit_price": "100.00", "tax_percent": 20 },
                    { "product_id": fixture.product, "description": "Widget", "quantity": "1.5",
                      "unit_price": "40.00", "tax_percent": 20 }
                ]
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "quote: {}", created.body);
    let quote_id = Uuid::parse_str(created.body["quote"]["id"].as_str().expect("a quote id"))
        .expect("a uuid");

    let sent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/send"),
            Some(sender),
            None,
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "send: {}", sent.body);

    let link = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/link"),
            Some(sender),
            None,
        ),
    )
    .await;
    assert_eq!(link.status, StatusCode::OK, "link: {}", link.body);
    // The link route answers `{ "url": "/q/<token>" }` — the one response that ever carries the
    // token in clear, because the database only holds its hash. So the walk takes the last path
    // segment, which is the credential, rather than inventing a field.
    let url = link.body["url"].as_str().expect("a public link");
    let public_token = url
        .rsplit('/')
        .next()
        .expect("a path segment")
        .to_string();
    assert!(!public_token.is_empty(), "the link must carry a token: {url}");

    let accepted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/public/quotes/{public_token}/accept"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "accept: {}", accepted.body);
    quote_id
}

async fn read_order(fixture: &Fixture, token: &str, order_id: Uuid) -> Value {
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/orders/{order_id}"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "order read: {}", response.body);
    response.body
}

async fn convert(fixture: &Fixture, token: &str, quote_id: Uuid) -> Value {
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sales/orders",
            Some(token),
            Some(json!({ "quote_id": quote_id })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "convert: {}", response.body);
    response.body
}

async fn confirm(fixture: &Fixture, token: &str, order_id: Uuid) -> Value {
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/confirm"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "confirm: {}", response.body);
    response.body
}

fn order_id_of(detail: &Value) -> Uuid {
    Uuid::parse_str(detail["order"]["id"].as_str().expect("an order id")).expect("a uuid")
}

// ---------------------------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_accepted_quote_becomes_an_order_that_copies_its_lines_and_totals() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;
    let quote_id = accepted_quote(&fixture, &drafter, &manager).await;

    let quote = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/quotes/{quote_id}"),
            Some(&drafter),
            None,
        ),
    )
    .await
    .body;
    let order = convert(&fixture, &drafter, quote_id).await;

    // Both documents are linked, in both directions: the order names the quote, and the order
    // list shows the quote's number so a seller does not have to open each order to find it.
    assert_eq!(order["order"]["quote_id"], quote_id.to_string());
    assert_eq!(order["order"]["quote_number"], quote["quote"]["number"]);

    // The copy, not a reference: same count, same order, same money.
    assert_eq!(
        order["lines"].as_array().expect("lines").len(),
        quote["lines"].as_array().expect("lines").len()
    );
    for (order_line, quote_line) in order["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .zip(quote["lines"].as_array().expect("lines"))
    {
        assert_eq!(order_line["quantity"], quote_line["quantity"]);
        assert_eq!(order_line["unit_price"], quote_line["unit_price"]);
        assert_eq!(order_line["line_total"], quote_line["line_total"]);
    }
    for total in ["subtotal", "tax_total", "grand_total"] {
        assert_eq!(
            order["totals"][total].as_str(),
            quote["quote"]["totals"][total].as_str(),
            "{total} must agree to the cent"
        );
    }

    // The order is a draft and holds nothing yet.
    assert_eq!(order["order"]["status"], "draft");
    assert_eq!(order["order"]["reservation_state"], "none");
    assert_eq!(order["order"]["invoice_state"], "none");

    // And the history says where it came from, in words a person reads.
    let history = order["history"].as_array().expect("history");
    assert_eq!(history.len(), 1, "a new order has one entry");
    assert!(
        history[0]["note"]
            .as_str()
            .expect("a note")
            .contains(&quote["quote"]["number"].as_str().expect("a number")),
        "the entry names the quote: {}",
        history[0]["note"]
    );
}

#[tokio::test]
async fn converting_the_same_quote_twice_returns_one_order_not_two_deliveries() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;
    let quote_id = accepted_quote(&fixture, &drafter, &manager).await;

    let first = convert(&fixture, &drafter, quote_id).await;
    let second = convert(&fixture, &drafter, quote_id).await;

    // Two deliveries for one accepted quote is a duplicated order, not a duplicated row, so the
    // second ask must be the same order. A unique index would have made this a 500; the read
    // inside the transaction is what makes it an answer.
    assert_eq!(first["order"]["id"], second["order"]["id"]);
    assert_eq!(first["order"]["number"], second["order"]["number"]);
}

#[tokio::test]
async fn a_quote_nobody_accepted_cannot_become_an_order_and_the_refusal_names_it() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sales/quotes",
            Some(&drafter),
            Some(json!({
                "customer_id": fixture.company,
                "customer_type": "company",
                "customer_name": "Orders Test Co",
                "currency": "TRY",
                "lines": [
                    { "product_id": fixture.product, "description": "Widget", "quantity": "1",
                      "unit_price": "10.00" }
                ]
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "quote: {}", created.body);
    let quote_id =
        Uuid::parse_str(created.body["quote"]["id"].as_str().expect("an id")).expect("a uuid");
    let number = created.body["quote"]["number"].as_str().expect("a number").to_string();

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sales/orders",
            Some(&drafter),
            Some(json!({ "quote_id": quote_id })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let message = refused.body["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(message.contains(&number), "the refusal names the quote: {message}");
    assert!(message.contains("draft"), "and says where it is: {message}");
}

// ---------------------------------------------------------------------------------------------
// Reservation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn confirming_holds_every_line_and_confirming_again_changes_nothing() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;
    let quote_id = accepted_quote(&fixture, &drafter, &manager).await;
    let order = convert(&fixture, &drafter, quote_id).await;
    let order_id = order_id_of(&order);

    let confirmed = confirm(&fixture, &manager, order_id).await;
    assert_eq!(confirmed["order"]["status"], "confirmed");
    assert_eq!(confirmed["order"]["reservation_state"], "total");

    // Every line carries a hold, and the hold says what and how much.
    let lines = confirmed["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 2);
    for line in lines {
        let hold = &line["reservation"];
        assert!(!hold.is_null(), "line {} has no hold: {line}", line["position"]);
        assert_eq!(hold["state"], "held");
        assert_eq!(hold["quantity"], line["quantity"]);
        assert!(hold["released_at"].is_null(), "a live hold has no release time");
    }

    // The second confirm: the same order, the same holds, and — the part a naive implementation
    // gets wrong — **no second promise** in the timeline. A person who pressed the button again
    // after a slow response must not find their order's history rewritten.
    // Two entries: the conversion, then the confirmation. Captured here so the next assertion
    // measures the *second* confirm against the first, rather than against a count that does not
    // yet include the confirmation at all.
    let entries_after_first = confirmed["history"].as_array().expect("history").len();
    assert_eq!(entries_after_first, 2, "conversion then confirmation: {}", confirmed["history"]);

    let again = confirm(&fixture, &manager, order_id).await;
    assert_eq!(again["order"]["id"], confirmed["order"]["id"]);
    assert_eq!(again["order"]["status"], "confirmed");
    assert_eq!(again["lines"], confirmed["lines"], "the holds are the same rows");
    assert_eq!(
        again["history"].as_array().expect("history").len(),
        entries_after_first,
        "a no-op confirm writes no history: {}",
        again["history"]
    );

    // And the database agrees, not just the response: still one hold per line.
    let held: (i64,) = sqlx::query_as(
        "select count(*) from sales_order_reservations where order_id = $1 and state = 'held'",
    )
    .bind(order_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the holds must be countable");
    assert_eq!(held.0, 2, "two lines, two holds — not four");
}

#[tokio::test]
async fn cancelling_needs_a_reason_releases_every_hold_and_is_final() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;
    let quote_id = accepted_quote(&fixture, &drafter, &manager).await;
    let order = convert(&fixture, &drafter, quote_id).await;
    let order_id = order_id_of(&order);
    confirm(&fixture, &manager, order_id).await;

    // No reason: refused. The person reading the released stock next month is not the person who
    // pulled it, so a blank release is not a record.
    let blunt = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/cancel"),
            Some(&manager),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(blunt.status, StatusCode::BAD_REQUEST, "{}", blunt.body);
    assert_eq!(blunt.body["error"]["details"]["field"], "reason");

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/cancel"),
            Some(&manager),
            Some(json!({ "reason": "customer bought elsewhere" })),
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "cancel: {}", cancelled.body);
    assert_eq!(cancelled.body["order"]["status"], "cancelled");
    assert_eq!(cancelled.body["order"]["reservation_state"], "released");

    // The holds are **released, not deleted**: the rows are still there with the reason, which
    // is the only way "when did this stop holding that stock?" gets answered.
    let released: Vec<(String, String)> = sqlx::query_as(
        "select state, released_reason from sales_order_reservations where order_id = $1",
    )
    .bind(order_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the holds must still be readable");
    assert_eq!(released.len(), 2);
    for (state, reason) in released {
        assert_eq!(state, "released");
        assert_eq!(reason, "customer bought elsewhere");
    }

    // A cancelled order cannot be confirmed afterwards — the stock is somebody else's now.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/confirm"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let message = refused.body["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(message.contains("released"), "the refusal explains why: {message}");
}

// ---------------------------------------------------------------------------------------------
// The invoice handoff
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_invoice_draft_freezes_the_money_and_asking_twice_returns_the_one_that_exists() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;
    let quote_id = accepted_quote(&fixture, &drafter, &manager).await;
    let order = convert(&fixture, &drafter, quote_id).await;
    let order_id = order_id_of(&order);

    // A draft order is a promise nobody has made yet, so it is not something to invoice.
    let too_early = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/invoice-draft"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(too_early.status, StatusCode::BAD_REQUEST, "{}", too_early.body);
    assert!(
        too_early.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("confirm"),
        "the refusal says what to do first"
    );

    confirm(&fixture, &manager, order_id).await;
    let raised = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/invoice-draft"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(raised.status, StatusCode::OK, "invoice draft: {}", raised.body);
    assert_eq!(raised.body["state"], "draft");
    let handoff_id = raised.body["id"].as_str().expect("a handoff id").to_string();

    // The money accounting will be handed agrees with the order, to the cent. This is the
    // criterion "the invoice number and the order totals agree" at the boundary where it can
    // actually be checked: REQ-054 is not built, so the frozen columns are the contract.
    let detail = read_order(&fixture, &manager, order_id).await;
    assert_eq!(raised.body["grand_total"], detail["order"]["grand_total"]);
    assert_eq!(raised.body["tax_total"], detail["order"]["tax_total"]);
    assert_eq!(raised.body["currency"], detail["order"]["currency"]);

    // And the order now says so, in the column the list filters on.
    assert_eq!(detail["order"]["invoice_state"], "draft");
    assert!(!detail["invoice"].is_null());

    // Twice: the same draft, and no second line in the order's timeline.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/invoice-draft"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(again.body["id"], handoff_id, "a second ask returns the draft that exists");

    let drafts: (i64,) = sqlx::query_as(
        "select count(*) from sales_invoice_handoffs where order_id = $1 and state = 'draft'",
    )
    .bind(order_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the drafts must be countable");
    assert_eq!(drafts.0, 1, "one delivery, one draft invoice");
}

#[tokio::test]
async fn a_cancelled_order_is_never_invoiced_and_voids_the_draft_it_already_had() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;
    let quote_id = accepted_quote(&fixture, &drafter, &manager).await;
    let order = convert(&fixture, &drafter, quote_id).await;
    let order_id = order_id_of(&order);
    confirm(&fixture, &manager, order_id).await;
    let raised = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/invoice-draft"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(raised.status, StatusCode::OK, "{}", raised.body);

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/cancel"),
            Some(&manager),
            Some(json!({ "reason": "duplicate order" })),
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.body);

    // A draft nobody has turned into an accounting document must not be left hanging: it is
    // voided with the same reason, rather than deleted, because "we did raise one" is a fact.
    let state: (String, String) = sqlx::query_as(
        "select state, void_reason from sales_invoice_handoffs where order_id = $1",
    )
    .bind(order_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the handoff row survives");
    assert_eq!(state.0, "void");
    assert_eq!(state.1, "duplicate order");

    // And asking again on a cancelled order is refused, not a fresh draft.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/orders/{order_id}/invoice-draft"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
}

// ---------------------------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_order_list_filters_and_paginates() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;

    let mut ids = Vec::new();
    for _ in 0..3 {
        let quote_id = accepted_quote(&fixture, &drafter, &manager).await;
        ids.push(order_id_of(&convert(&fixture, &drafter, quote_id).await));
    }
    confirm(&fixture, &manager, ids[0]).await;

    let all = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sales/orders?limit=50", Some(&drafter), None),
    )
    .await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.body);
    let items = all.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 3, "three orders exist: {}", all.body);
    assert!(all.body["next_cursor"].is_null(), "one page holds all three");

    // Each order carries the quote's number, so the list is readable without a fetch per row.
    let first = items
        .iter()
        .find(|item| item["id"] == ids[0].to_string())
        .expect("the confirmed order is in the list");
    assert_eq!(first["reservation_state"], "total");
    assert!(!first["quote_number"].is_null(), "the list shows where it came from");

    // A status filter narrows it, and a search term that matches nothing is an **empty list**,
    // not an error and not everything.
    let confirmed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/orders?status=confirmed",
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(confirmed.body["items"].as_array().expect("items").len(), 1);

    let miss = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/orders?search=nothing-matches-this",
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(miss.status, StatusCode::OK, "{}", miss.body);
    assert!(miss.body["items"].as_array().expect("items").is_empty());

    // A wildcard typed into the search box is a term, not a "match everything" — otherwise the
    // filter looks broken rather than literal.
    let wildcard = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sales/orders?search=%25", Some(&drafter), None),
    )
    .await;
    assert!(
        wildcard.body["items"].as_array().expect("items").is_empty(),
        "`%` is a character, not a wildcard"
    );

    // A sort key outside the closed set is refused rather than interpolated.
    let bad_sort = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/orders?sort=number%3B%20drop%20table%20sales_orders",
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(bad_sort.status, StatusCode::BAD_REQUEST, "{}", bad_sort.body);
}

// ---------------------------------------------------------------------------------------------
// The permission and the tenant boundary
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_reader_may_see_the_deliveries_and_may_not_promise_one() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;
    let reader = fixture.token(&fixture.reader).await;

    let quote_id = accepted_quote(&fixture, &drafter, &manager).await;
    let order = convert(&fixture, &drafter, quote_id).await;
    let order_id = order_id_of(&order);

    // The list and the detail are the reader's.
    let list = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sales/orders", Some(&reader), None),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    let detail = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/orders/{order_id}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);

    // Every transition is refused. Confirming, cancelling and invoicing all commit the
    // organization, so one missing key closes all three.
    for (path, body) in [
        ("confirm", None),
        ("cancel", Some(json!({ "reason": "not mine to cancel" }))),
        ("invoice-draft", None),
    ] {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                &format!("/api/v1/sales/orders/{order_id}/{path}"),
                Some(&reader),
                body,
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}: {}", response.body);
    }
}

#[tokio::test]
async fn every_order_route_is_guarded_and_another_organization_is_a_404() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;
    let foreign = fixture.token(&fixture.foreign).await;

    let quote_id = accepted_quote(&fixture, &drafter, &manager).await;
    let order = convert(&fixture, &drafter, quote_id).await;
    let order_id = order_id_of(&order);

    // No session: 401 everywhere.
    for (method, path, body) in [
        (Method::GET, "/api/v1/sales/orders".to_string(), None),
        (Method::GET, format!("/api/v1/sales/orders/{order_id}"), None),
        (
            Method::POST,
            "/api/v1/sales/orders".to_string(),
            Some(json!({ "quote_id": quote_id })),
        ),
        (Method::POST, format!("/api/v1/sales/orders/{order_id}/confirm"), None),
        (
            Method::POST,
            format!("/api/v1/sales/orders/{order_id}/cancel"),
            Some(json!({ "reason": "x" })),
        ),
        (
            Method::POST,
            format!("/api/v1/sales/orders/{order_id}/invoice-draft"),
            None,
        ),
    ] {
        let response = call(&fixture.state, request(method, &path, None, body)).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}: {}", response.body);
    }

    // A different organization holding the *full* permission set: `404`, never `403`. A 403
    // would confirm the order exists, and one organization's deliveries are the thing this
    // module exists to keep apart.
    for (method, path, body) in [
        (Method::GET, format!("/api/v1/sales/orders/{order_id}"), None),
        (Method::POST, format!("/api/v1/sales/orders/{order_id}/confirm"), None),
        (
            Method::POST,
            format!("/api/v1/sales/orders/{order_id}/cancel"),
            Some(json!({ "reason": "not mine" })),
        ),
        (
            Method::POST,
            format!("/api/v1/sales/orders/{order_id}/invoice-draft"),
            None,
        ),
    ] {
        let response = call(&fixture.state, request(method, &path, Some(&foreign), body)).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}: {}", response.body);
    }

    // And their own list is empty rather than showing somebody else's deliveries.
    let their_list = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sales/orders", Some(&foreign), None),
    )
    .await;
    assert_eq!(their_list.status, StatusCode::OK, "{}", their_list.body);
    assert!(their_list.body["items"].as_array().expect("items").is_empty());

    // Their order id is not convertible either: the quote is not theirs.
    let their_convert = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sales/orders",
            Some(&foreign),
            Some(json!({ "quote_id": quote_id })),
        ),
    )
    .await;
    assert_eq!(their_convert.status, StatusCode::NOT_FOUND, "{}", their_convert.body);

    // The manager's own transitions still work, so the guards above are refusals and not a
    // broken route.
    let confirmed = confirm(&fixture, &manager, order_id).await;
    assert_eq!(confirmed["order"]["status"], "confirmed");
}

// ---------------------------------------------------------------------------------------------
// A hand-written order
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_order_written_by_hand_totals_its_own_lines() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sales/orders",
            Some(&drafter),
            Some(json!({
                "customer_name": "Walk-in customer",
                "lines": [
                    { "product_id": fixture.product, "quantity": "2", "unit_price": "100.00",
                      "tax_percent": 20 },
                    { "description": "Rush handling", "quantity": "1", "unit_price": "15.50" }
                ]
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let order = created.body;

    // 2 × 100 at 20% tax = 240.00, plus a 15.50 free-tax line: subtotal 215.50, tax 40.00,
    // grand total 255.50.
    assert_eq!(order["totals"]["subtotal"], "215.50");
    assert_eq!(order["totals"]["tax_total"], "40.00");
    assert_eq!(order["totals"]["grand_total"], "255.50");
    assert_eq!(order["order"]["status"], "draft");
    assert!(order["order"]["quote_id"].is_null(), "a hand-written order has no quote");

    // The product line inherited the catalog's unit, which is the whole reason the server
    // resolves defaults rather than the browser.
    assert_eq!(order["lines"][0]["unit"], "piece");

    // A hand-written order with no lines, or no customer, is refused at the field.
    for (body, field) in [
        (json!({ "customer_name": "Nobody", "lines": [] }), "lines"),
        (
            json!({ "lines": [{ "description": "x", "quantity": "1", "unit_price": "1" }] }),
            "customer_name",
        ),
    ] {
        let refused = call(
            &fixture.state,
            request(Method::POST, "/api/v1/sales/orders", Some(&drafter), Some(body)),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
        assert_eq!(refused.body["error"]["details"]["field"], field);
    }
}
