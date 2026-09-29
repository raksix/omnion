//! Integration tests for the quote lifecycle (docs/requests/REQ-052, slice 2).
//!
//! These run against the development stack and skip themselves with a printed reason when
//! PostgreSQL is not reachable. They use the **same** `organization_of` rule the catalog slice
//! proved, and the same 401/403/404 ladder, so a reader who knows slice 1's walk knows this one.
//!
//! What the walk proves, in the words of the acceptance criteria:
//!
//! * every `/api/v1/sales/quotes/*` route answers 401 unauthenticated, 403 with the permission
//!   missing and 200 with it granted; a quote of another organization is **404, never 403**;
//! * **the totals are computed in SQL**: a hand-computed fixture with three lines (one at 20%
//!   discount, an awkward `2.5 × 19.90`, a free-text line) matches to the cent, and changing a
//!   line's quantity moves the persisted subtotal, discount, tax and grand total together;
//! * **numbering is per-organization and gap-free under concurrent creates** — ten quotes made at
//!   once produce ten distinct, consecutive numbers;
//! * a number is **immutable after send**;
//! * `Send` snapshots an immutable version with its totals, the lines are frozen afterwards, and a
//!   PATCH on a sent quote is a `409` that says to duplicate it;
//! * the public page resolves with **no session**, accepts once, declines with a reason, and a
//!   consumed or wrong token is one indistinguishable refusal;
//! * **a re-issued link invalidates the old one** — the revocation path;
//! * the expiry sweep flips a lapsed quote to `expired` on the next read;
//! * every mutation writes an audit row and emits the documented `sales.quote.*` event.
//!
//! The permission split is proved on purpose: a role that may **create** a quote must not be able
//! to **send** one, because sending is the moment a document stops being the seller's.

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

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Serialises this suite: the organizations and the IAM seed are shared state.
static QUOTES_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// May read the quotes and nothing else — the role that proves `read` is its own permission.
///
/// `crm.contacts.*` rather than a made-up `crm.read`: the quote's customer picker searches the
/// CRM, and a role that could open a quote but not search its customer would show an empty picker.
const READER_PERMISSIONS: [&str; 5] = [
    "sales.quotes.read",
    "sales.quotes.create",
    "sales.products.read",
    "crm.contacts.read",
    "sites.read",
];

/// May draft and edit, but **not** send: the separation the spec's approval gate depends on.
///
/// `sales.products.manage` is here for the fixture (it creates the product the quote lines sell)
/// and not for the quotes: a drafter who may add a product is normal, and a drafter who may send
/// is the thing the approval gate exists to prevent.
const DRAFTER_PERMISSIONS: [&str; 8] = [
    "sales.quotes.read",
    "sales.quotes.create",
    "sales.quotes.update",
    "sales.products.read",
    "sales.products.manage",
    "crm.contacts.read",
    "crm.contacts.create",
    "sites.read",
];

/// A full seller, and the only role that may put a link in a customer's hand.
const SELLER_PERMISSIONS: [&str; 7] = [
    "sales.quotes.read",
    "sales.quotes.create",
    "sales.quotes.update",
    "sales.quotes.send",
    "sales.products.read",
    "crm.contacts.read",
    "sites.read",
];

/// A writer in a **second** organization, for the cross-tenant `404`.
const FOREIGN_PERMISSIONS: [&str; 7] = [
    "sales.quotes.read",
    "sales.quotes.create",
    "sales.quotes.update",
    "sales.quotes.send",
    "sales.products.read",
    "crm.contacts.read",
    "sites.read",
];

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

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
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };

    TestResponse {
        status,
        set_cookie,
        body,
    }
}

/// Build a JSON request; `token` becomes the session cookie and `body` the payload.
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

/// Organizations, accounts and a CRM company, ready for a quote.
struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    company: Uuid,
    product: Uuid,
    drafter: String,
    seller: String,
    reader: String,
    foreign: String,
    no_permission: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = QUOTES_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org = create_organization_row(&db, "a").await;
        let other_org = create_organization_row(&db, "b").await;

        let (owner_id, _owner) = create_account(&db, None, "Quotes Owner").await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        let (drafter_id, drafter) = create_account(&db, Some(org), "Quotes Drafter").await;
        grant(&db, org, drafter_id, owner_id, &DRAFTER_PERMISSIONS).await;

        let (seller_id, seller) = create_account(&db, Some(org), "Quotes Seller").await;
        grant(&db, org, seller_id, owner_id, &SELLER_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(org), "Quotes Reader").await;
        grant(&db, org, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (foreign_id, foreign) = create_account(&db, Some(other_org), "Quotes Foreign").await;
        grant(&db, other_org, foreign_id, owner_id, &FOREIGN_PERMISSIONS).await;

        let (_none_id, no_permission) = create_account(&db, Some(org), "Quotes Nobody").await;

        let drafter_token = login(&state, &drafter).await;
        let company = create_company(&state, &drafter_token).await;
        let product = create_product(&state, &drafter_token).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            company,
            product,
            drafter,
            seller,
            reader,
            foreign,
            no_permission,
        })
    }

    async fn token(&self, email: &str) -> String {
        login(&self.state, email).await
    }
}

/// Create an organization row with a unique slug.
async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!(
        "quotes-fix-{}-{}",
        label.to_lowercase().replace([' ', '_'], "-"),
        Uuid::new_v4().simple()
    );
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Quotes Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("quotes-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("quotes-role-{}", Uuid::new_v4().simple()),
            name: "Quotes Test Role".to_owned(),
            description: "A role of the quotes walk".to_owned(),
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
    assert_eq!(response.status, StatusCode::OK, "login body: {}", response.body);
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
async fn audit_rows(db: &Db, action: &str) -> Vec<Value> {
    let rows: Vec<(Value, Option<String>, Option<String>)> = sqlx::query_as(
        "select metadata, target_type, target_id from audit_log \
         where action = $1 order by id desc limit 5",
    )
    .bind(action)
    .fetch_all(db.pool())
    .await
    .expect("the audit rows must read");
    rows.into_iter()
        .map(|(metadata, target_type, target_id)| {
            json!({ "metadata": metadata, "target_type": target_type, "target_id": target_id })
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

/// A CRM company to bill — the customer every quote below is addressed to.
async fn create_company(state: &AppState, token: &str) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/companies",
            Some(token),
            Some(json!({ "name": format!("Acme {}", &Uuid::new_v4().simple().to_string()[..6]) })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "company body: {}",
        response.body
    );
    Uuid::parse_str(response.body["id"].as_str().expect("a company id")).expect("a uuid")
}

/// A catalog product, so one quote line is a real product line with a real price.
async fn create_product(state: &AppState, token: &str) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/products",
            Some(token),
            Some(json!({
                "sku": format!("Q-{}", &Uuid::new_v4().simple().to_string()[..8]),
                "name": "Consulting hour",
                "unit": "hour",
                "tax_percent": 20,
                "default_price": "100.00",
                "currency": "TRY",
            })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "product body: {}",
        response.body
    );
    Uuid::parse_str(response.body["id"].as_str().expect("a product id")).expect("a uuid")
}

/// The three-line grid the money fixture is written against, by hand:
///
/// | line | qty | unit price | discount | tax | gross | off | payable | tax | net |
/// |------|-----|-----------|----------|-----|-------|-----|---------|-----|-----|
/// | 1    | 3   | 100.00    | 0        | 20  | 300.00| 0.00| 300.00  | 60.00| 360.00 |
/// | 2    | 2.5 | 19.90     | 20      | 20  | 49.75 | 9.95|  39.80  |  7.96|  47.76 |
/// | 3    | 1   | 0.00      | 0        | 0   |   0.00| 0.00|   0.00  |  0.00|   0.00 |
///
/// subtotal 349.75, discount 9.95, tax 67.96, grand total **407.76** — which is 360.00 + 47.76,
/// the sum of the rounded line totals, and *not* 349.75 − 9.95 + 67.96 by accident: both happen to
/// be 407.76 here, which is exactly why the fixture writes the per-line column down. (The first
/// version of this constant said 417.71 and the walk caught it.)
const FIXTURE_SUBTOTAL: &str = "349.75";
const FIXTURE_DISCOUNT: &str = "9.95";
const FIXTURE_TAX: &str = "67.96";
const FIXTURE_GRAND: &str = "407.76";

/// A quote body with the fixture grid, in the module's own request shape.
fn quote_body(company: Uuid, product: Uuid) -> Value {
    json!({
        "customer_id": company,
        "customer_type": "company",
        "customer_name": "Acme",
        "title": "Website rebuild",
        "currency": "TRY",
        "lines": [
            { "product_id": product, "description": "Consulting hour", "quantity": "3",
              "unit_price": "100.00", "discount_percent": 0, "tax_percent": 20 },
            { "product_id": product, "description": "Support block", "quantity": "2.5",
              "unit_price": "19.90", "discount_percent": 20, "tax_percent": 20 },
            { "description": "Goodwill", "quantity": "1", "unit_price": "0.00" }
        ]
    })
}

/// Create a quote and return its body, failing loudly with what the API said.
async fn create_quote(fixture: &Fixture, token: &str, body: Value) -> (Uuid, Value) {
    let response = call(
        &fixture.state,
        request(Method::POST, "/api/v1/sales/quotes", Some(token), Some(body)),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "quote create body: {}",
        response.body
    );
    let id = Uuid::parse_str(response.body["quote"]["id"].as_str().expect("a quote id"))
        .expect("a uuid");
    (id, response.body)
}

// ---------------------------------------------------------------------------------------------
// The money: totals computed in SQL
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_totals_are_computed_in_sql_and_match_a_hand_written_fixture_to_the_cent() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.drafter).await;

    let (id, body) = create_quote(
        &fixture,
        &token,
        quote_body(fixture.company, fixture.product),
    )
    .await;
    let totals = &body["quote"]["totals"];

    assert_eq!(totals["subtotal"].as_str(), Some(FIXTURE_SUBTOTAL), "subtotal");
    assert_eq!(
        totals["discount_total"].as_str(),
        Some(FIXTURE_DISCOUNT),
        "discount"
    );
    assert_eq!(totals["tax_total"].as_str(), Some(FIXTURE_TAX), "tax");
    assert_eq!(totals["grand_total"].as_str(), Some(FIXTURE_GRAND), "grand total");
    assert_eq!(body["quote"]["max_discount_percent"].as_i64(), Some(20));

    // The **stored** row, read straight from PostgreSQL, not the response the handler shaped.
    let stored: (String, String, String, String) = sqlx::query_as(
        "select subtotal::text, discount_total::text, tax_total::text, grand_total::text
           from sales_quotes where id = $1",
    )
    .bind(id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the quote row must read");
    assert_eq!(stored.0, FIXTURE_SUBTOTAL);
    assert_eq!(stored.1, FIXTURE_DISCOUNT);
    assert_eq!(stored.2, FIXTURE_TAX);
    assert_eq!(stored.3, FIXTURE_GRAND);

    // And each line's own total, which is the "round once per line" half of the rule.
    let lines: Vec<(String,)> =
        sqlx::query_as("select line_total::text from sales_quote_lines where quote_id = $1 order by position")
            .bind(id)
            .fetch_all(fixture.db.pool())
            .await
            .expect("the lines must read");
    let line_totals: Vec<&str> = lines.iter().map(|(value,)| value.as_str()).collect();
    assert_eq!(line_totals, ["360.00", "47.76", "0.00"]);
}

#[tokio::test]
async fn changing_one_line_moves_all_four_totals_together() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.drafter).await;
    let (id, _) = create_quote(&fixture, &token, quote_body(fixture.company, fixture.product)).await;

    // Quantity 3 → 5 on the first line adds 2 × 100.00 gross and 2 × 20.00 tax.
    let response = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sales/quotes/{id}/lines"),
            Some(&token),
            Some(json!({
                "lines": [
                    { "product_id": fixture.product, "quantity": "5", "unit_price": "100.00",
                      "tax_percent": 20 },
                    { "product_id": fixture.product, "quantity": "2.5", "unit_price": "19.90",
                      "discount_percent": 20, "tax_percent": 20 },
                    { "description": "Goodwill", "quantity": "1" }
                ]
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "line replace: {}", response.body);

    let totals = &response.body["quote"]["totals"];
    assert_eq!(totals["subtotal"].as_str(), Some("549.75"), "200.00 more gross");
    assert_eq!(totals["discount_total"].as_str(), Some(FIXTURE_DISCOUNT), "untouched");
    assert_eq!(totals["tax_total"].as_str(), Some("107.96"), "40.00 more tax");
    // 600.00 (five hours gross + tax) + 47.76 (the second line) + 0.00 = 647.76.
    assert_eq!(totals["grand_total"].as_str(), Some("647.76"));

    // The header and the grid may not drift: the persisted grand total is the sum of the stored
    // line totals, re-aggregated by the database.
    let (sum, stored): ((String,), String) = {
        let (total,): (String,) = sqlx::query_as(
            "select sum(round(quantity * unit_price * (100 - discount_percent) / 100.0, 2)
                      + round(quantity * unit_price * (100 - discount_percent) / 100.0
                              * tax_percent / 100.0, 2))::text
               from sales_quote_lines where quote_id = $1",
        )
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the lines must aggregate");
        let (stored,): (String,) = sqlx::query_as("select grand_total::text from sales_quotes where id = $1")
            .bind(id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the quote must read");
        ((total,), stored)
    };
    assert_eq!(sum.0, stored, "the header is the sum of the lines beside it");
}

// ---------------------------------------------------------------------------------------------
// Numbering
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn numbers_are_per_organization_gap_free_under_concurrent_creates_and_immutable_after_send() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.drafter).await;

    // Ten creates at once: the counter row is taken `for update`, so the numbers are distinct and
    // consecutive. Ten sequential calls would prove nothing about the lock.
    let mut created = Vec::new();
    for _ in 0..10 {
        created.push(tokio::spawn({
            let request_body = quote_body(fixture.company, fixture.product);
            let state = fixture.state.clone();
            let token = token.clone();
            async move {
                call(
                    &state,
                    request(
                        Method::POST,
                        "/api/v1/sales/quotes",
                        Some(&token),
                        Some(request_body),
                    ),
                )
                .await
            }
        }));
    }
    let mut numbers: Vec<i64> = Vec::new();
    for handle in created {
        let response = handle.await.expect("the create task must not panic");
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "concurrent create: {}",
            response.body
        );
        let number = response.body["quote"]["number"].as_str().expect("a number").to_owned();
        let sequence: i64 = number
            .rsplit('-')
            .next()
            .expect("Q-1 has a tail")
            .parse()
            .expect("the tail is a number");
        assert!(number.starts_with("Q-"), "the prefix is the settings row's: {number}");
        numbers.push(sequence);
    }
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), 10, "ten concurrent creates, ten numbers: {numbers:?}");
    for pair in numbers.windows(2) {
        assert_eq!(
            pair[1],
            pair[0] + 1,
            "the sequence is gap-free: {numbers:?}"
        );
    }

    // Immutability after send: the number a customer was given is the number the row still says.
    let (id, body) = create_quote(&fixture, &token, quote_body(fixture.company, fixture.product)).await;
    let before = body["quote"]["number"].as_str().expect("a number").to_owned();
    let seller = fixture.token(&fixture.seller).await;
    let sent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "send: {}", sent.body);
    assert_eq!(
        sent.body["quote"]["number"].as_str(),
        Some(before.as_str()),
        "sending does not renumber"
    );
}

// ---------------------------------------------------------------------------------------------
// Immutability and versioning
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn sending_snapshots_a_version_freezes_the_lines_and_a_patch_afterwards_is_a_conflict() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;
    let (id, _) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;

    // A draft is editable, and the edit moves the totals.
    let edited = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/sales/quotes/{id}"),
            Some(&drafter),
            Some(json!({ "title": "Website rebuild — phase 1" })),
        ),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "header edit: {}", edited.body);
    assert_eq!(edited.body["quote"]["title"].as_str(), Some("Website rebuild — phase 1"));

    // Send: a version is snapshotted with the totals as they stood.
    let sent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "send: {}", sent.body);
    assert_eq!(sent.body["quote"]["status"].as_str(), Some("sent"));
    assert_eq!(sent.body["quote"]["version"].as_i64(), Some(1));
    let versions = sent.body["versions"].as_array().expect("versions");
    assert_eq!(versions.len(), 1, "one snapshot, not one per read");
    assert_eq!(versions[0]["version"].as_i64(), Some(1));
    assert_eq!(
        versions[0]["totals"]["grand_total"].as_str(),
        Some(FIXTURE_GRAND),
        "the snapshot carries the totals the customer read"
    );
    assert_eq!(
        versions[0]["lines"].as_array().map(Vec::len),
        Some(3),
        "the snapshot carries the grid"
    );

    // Now frozen: a header edit and a line replacement are both a 409 that says what to do.
    let after_send = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/sales/quotes/{id}"),
            Some(&seller),
            Some(json!({ "title": "Cheaper" })),
        ),
    )
    .await;
    assert_eq!(
        after_send.status,
        StatusCode::CONFLICT,
        "a sent quote is not editable: {}",
        after_send.body
    );
    let message = after_send.body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("duplicate"), "{message} must name the way out");

    let lines_after_send = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/sales/quotes/{id}/lines"),
            Some(&seller),
            Some(json!({ "lines": [{ "description": "One line", "quantity": "1" }] })),
        ),
    )
    .await;
    assert_eq!(
        lines_after_send.status,
        StatusCode::CONFLICT,
        "nor may its lines change: {}",
        lines_after_send.body
    );

    // The refusal changed nothing: the stored totals are still the sent ones.
    let (stored,): (String,) = sqlx::query_as("select grand_total::text from sales_quotes where id = $1")
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote must read");
    assert_eq!(stored, FIXTURE_GRAND);
}

#[tokio::test]
async fn duplicating_a_sent_quote_gives_a_new_number_and_a_draft_that_can_be_sent_again() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;
    let (id, body) =
        create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;
    let first_number = body["quote"]["number"].as_str().expect("a number").to_owned();

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;

    let copy = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/duplicate"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(copy.status, StatusCode::CREATED, "duplicate: {}", copy.body);
    assert_eq!(copy.body["quote"]["status"].as_str(), Some("draft"));
    let second_number = copy.body["quote"]["number"].as_str().expect("a number");
    assert_ne!(second_number, first_number, "a duplicate is a new document");
    assert_eq!(
        copy.body["quote"]["totals"]["grand_total"].as_str(),
        Some(FIXTURE_GRAND),
        "with the same money"
    );
    assert_eq!(
        copy.body["lines"].as_array().map(Vec::len),
        Some(3),
        "and the same grid"
    );

    // And the copy is sendable, which is the whole point of duplicating rather than editing.
    let copy_id = copy.body["quote"]["id"].as_str().expect("an id");
    let resent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{copy_id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(resent.status, StatusCode::OK, "re-send: {}", resent.body);
    assert_eq!(resent.body["quote"]["version"].as_i64(), Some(1));
}

// ---------------------------------------------------------------------------------------------
// The public link
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_public_page_reads_without_a_session_and_accepts_once() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;
    let (id, _) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;
    send_quote(&fixture, &seller, id).await;

    let token = issue_link(&fixture, &seller, id).await;

    // No session: the token is the credential.
    let page = call(
        &fixture.state,
        request(Method::GET, &format!("/api/v1/sales/public/quotes/{token}"), None, None),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "public read: {}", page.body);
    let number = page.body["number"].as_str().expect("a number");
    assert!(number.starts_with("Q-"), "the customer reads a real document number: {number}");
    assert_eq!(page.body["totals"]["grand_total"].as_str(), Some(FIXTURE_GRAND));
    assert_eq!(page.body["currency"].as_str(), Some("TRY"));
    assert_eq!(page.body["status"].as_str(), Some("sent"));
    // Nothing about the seller's side leaks into the customer's copy.
    for forbidden in ["owner", "price_list_id", "organization_id", "versions", "decline_reason"] {
        assert!(
            page.body.get(forbidden).is_none(),
            "the public payload must not carry `{forbidden}`: {}",
            page.body
        );
    }

    // Accept: no session, a body with a note.
    let accepted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/public/quotes/{token}/accept"),
            None,
            Some(json!({ "note": "Looks good, let's start" })),
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "accept: {}", accepted.body);
    assert_eq!(accepted.body["status"].as_str(), Some("accepted"));
    assert_eq!(accepted.body["decided"].as_bool(), Some(true));

    // Once. A second acceptance is the same refusal as a wrong token, because both are one
    // answer: a caller cannot use the endpoint to learn which links exist.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/public/quotes/{token}/accept"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "accept twice: {}", again.body);

    let wrong = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/public/quotes/00000000-0000-0000-0000-000000000000",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::NOT_FOUND);
    assert_eq!(
        wrong.body["error"]["code"],
        again.body["error"]["code"],
        "a wrong token and a consumed one are one answer"
    );
}

#[tokio::test]
async fn a_decline_without_a_reason_is_refused_and_one_with_a_reason_is_recorded() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;
    let (id, _) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;
    send_quote(&fixture, &seller, id).await;
    let token = issue_link(&fixture, &seller, id).await;

    // "No" with no reason is the one thing a seller cannot act on, so it is refused.
    let bare = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/public/quotes/{token}/decline"),
            None,
            Some(json!({ "note": "" })),
        ),
    )
    .await;
    assert_eq!(bare.status, StatusCode::BAD_REQUEST, "bare decline: {}", bare.body);
    assert_eq!(bare.body["error"]["details"]["field"], "reason");

    let declined = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/public/quotes/{token}/decline"),
            None,
            Some(json!({ "note": "Too expensive this quarter" })),
        ),
    )
    .await;
    assert_eq!(declined.status, StatusCode::OK, "decline: {}", declined.body);
    assert_eq!(declined.body["status"].as_str(), Some("declined"));

    // The reason is on the quote the seller opens, and the row agrees.
    let detail = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/quotes/{id}"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(
        detail.body["decline_reason"].as_str(),
        Some("Too expensive this quarter")
    );
    let (status,): (String,) = sqlx::query_as("select status from sales_quotes where id = $1")
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote must read");
    assert_eq!(status, "declined");
}

#[tokio::test]
async fn re_issuing_a_link_invalidates_the_previous_one() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;
    let (id, _) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;

    send_quote(&fixture, &seller, id).await;
    let first = issue_link(&fixture, &seller, id).await;
    let second = issue_link(&fixture, &seller, id).await;
    assert_ne!(first, second, "each link is a fresh token");

    let old = call(
        &fixture.state,
        request(Method::GET, &format!("/api/v1/sales/public/quotes/{first}"), None, None),
    )
    .await;
    assert_eq!(old.status, StatusCode::NOT_FOUND, "the old link is dead");

    let live = call(
        &fixture.state,
        request(Method::GET, &format!("/api/v1/sales/public/quotes/{second}"), None, None),
    )
    .await;
    assert_eq!(live.status, StatusCode::OK, "{}", live.body);

    // And only the hash is stored: a database reader must not be able to open the link.
    let (hash,): (Option<String>,) = sqlx::query_as("select public_token_hash from sales_quotes where id = $1")
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote must read");
    let stored = hash.expect("a hash is stored");
    assert_ne!(stored, second, "the token itself is never stored");
    assert_eq!(stored.len(), 64, "sha256 as hex");
    assert!(stored.chars().all(|c| c.is_ascii_hexdigit()));

    // The audit row and the event must not carry the token either.
    for row in audit_rows(&fixture.db, "sales.quote.link_issued").await {
        let rendered = row.to_string();
        assert!(!rendered.contains(&second), "the token leaked into an audit row");
    }
}

#[tokio::test]
async fn a_draft_has_no_public_link_and_a_honeypot_is_answered_as_if_the_link_did_not_exist() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;
    let (id, _) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;

    // A link before the send is a refusal, not a working URL a customer could open.
    let early = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/link"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(early.status, StatusCode::BAD_REQUEST, "{}", early.body);

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;
    let token = issue_link(&fixture, &seller, id).await;

    let trapped = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/public/quotes/{token}/accept"),
            None,
            Some(json!({ "note": "buy now", "website": "http://spam.example" })),
        ),
    )
    .await;
    assert_eq!(
        trapped.status,
        StatusCode::NOT_FOUND,
        "a bot that fills every field learns nothing: {}",
        trapped.body
    );
    // And the quote is untouched: the honeypot path must not have accepted anything.
    let (status,): (String,) = sqlx::query_as("select status from sales_quotes where id = $1")
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote must read");
    assert_eq!(status, "sent");
}

/// Send a quote as the seller, failing loudly.
///
/// A named step rather than four copies of the same seven lines: three of the tests below needed
/// a "send, then link" sequence and the first draft of them simply called `issue_link` on a draft
/// and was surprised by a 400 — which is the product being right.
async fn send_quote(fixture: &Fixture, token: &str, id: Uuid) {
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "send {id}: {}", response.body);
    assert_eq!(response.body["quote"]["status"].as_str(), Some("sent"));
}

/// Issue a public link and return the token, failing loudly.
async fn issue_link(fixture: &Fixture, token: &str, id: Uuid) -> String {
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/link"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "issue link: {}", response.body);
    response.body["url"]
        .as_str()
        .expect("a url")
        .rsplit('/')
        .next()
        .expect("the token is the last segment")
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// Expiry
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_quote_past_its_validity_flips_to_expired_on_the_next_read_and_cannot_be_sent() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;

    // A quote valid **yesterday**, built directly because the API refuses a past validity — which
    // is the other half of this rule and is proved in `a_validity_in_the_past_is_refused`.
    let (id, body) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;
    sqlx::query(
        "update sales_quotes set valid_until = current_date - 1 where id = $1",
    )
    .bind(id)
    .execute(fixture.db.pool())
    .await
    .expect("the validity must move");
    let (status,): (String,) = sqlx::query_as("select status from sales_quotes where id = $1")
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote must read");
    assert_eq!(status, "draft", "a draft that lapsed is still a draft until it is sent");

    // A lapsed **draft** cannot be sent: the seller is told why.
    let too_late = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(too_late.status, StatusCode::BAD_REQUEST, "{}", too_late.body);
    assert!(
        too_late.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("expired"),
        "the message says it lapsed: {}",
        too_late.body
    );

    // A sent quote that lapses is badged on the next read — the sweep runs on the way in.
    let (sent_id, _) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;
    let sent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{sent_id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    let token = issue_link(&fixture, &seller, sent_id).await;
    sqlx::query("update sales_quotes set valid_until = current_date - 1 where id = $1")
        .bind(sent_id)
        .execute(fixture.db.pool())
        .await
        .expect("the validity must move");

    let list = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sales/quotes?search=Q-", Some(&drafter), None),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "list: {}", list.body);
    let row = list.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["id"].as_str() == Some(sent_id.to_string().as_str()))
        .expect("the sent quote is in the list");
    assert_eq!(row["status"].as_str(), Some("expired"), "{}", row);

    // And the public link is gone with it: an expired quote is not a document anybody may accept.
    let public = call(
        &fixture.state,
        request(Method::GET, &format!("/api/v1/sales/public/quotes/{token}"), None, None),
    )
    .await;
    assert_eq!(public.status, StatusCode::NOT_FOUND, "{}", public.body);
    let _ = body;
}

// ---------------------------------------------------------------------------------------------
// Permissions, tenancy and refusals
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_quote_routes_answer_the_full_ladder_and_a_foreign_quote_is_a_four_oh_four() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;
    let reader = fixture.token(&fixture.reader).await;
    let nobody = fixture.token(&fixture.no_permission).await;
    let (id, _) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;

    // 401 with no session at all.
    for (method, path) in [
        (Method::GET, "/api/v1/sales/quotes".to_string()),
        (Method::POST, "/api/v1/sales/quotes".to_string()),
        (Method::GET, format!("/api/v1/sales/quotes/{id}")),
    ] {
        let response = call(
            &fixture.state,
            request(method.clone(), &path, None, Some(json!({}))),
        )
        .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}");
    }

    // 403 with an account that holds no sales permission.
    let forbidden = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sales/quotes", Some(&nobody), None),
    )
    .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN, "{}", forbidden.body);

    // `read` is its own permission: a role that may create a quote still cannot send one, because
    // sending is the moment the document leaves the building.
    let reader_send = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(
        reader_send.status,
        StatusCode::FORBIDDEN,
        "creating a quote must not imply sending it: {}",
        reader_send.body
    );
    let reader_link = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/link"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(reader_link.status, StatusCode::FORBIDDEN);

    // A drafter may create and edit, and may NOT send.
    let drafter_send = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(
        drafter_send.status,
        StatusCode::FORBIDDEN,
        "the draft/send split is the gate the approval rule sits on: {}",
        drafter_send.body
    );

    // A full seller can.
    let seller_send = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(seller_send.status, StatusCode::OK, "{}", seller_send.body);

    // 200 for the reader.
    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sales/quotes", Some(&reader), None),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);

    // Another organization's quote is a 404, not a 403 — a 403 would confirm it exists.
    let foreign = fixture.token(&fixture.foreign).await;
    let read = call(
        &fixture.state,
        request(Method::GET, &format!("/api/v1/sales/quotes/{id}"), Some(&foreign), None),
    )
    .await;
    assert_eq!(read.status, StatusCode::NOT_FOUND, "{}", read.body);
    let edit = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/sales/quotes/{id}"),
            Some(&foreign),
            Some(json!({ "title": "mine now" })),
        ),
    )
    .await;
    assert_eq!(edit.status, StatusCode::NOT_FOUND, "{}", edit.body);
    let cancel = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/cancel"),
            Some(&foreign),
            Some(json!({ "reason": "not mine" })),
        ),
    )
    .await;
    assert_eq!(cancel.status, StatusCode::NOT_FOUND, "{}", cancel.body);

    // The foreign refusal wrote nothing.
    let (title,): (String,) = sqlx::query_as("select title from sales_quotes where id = $1")
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote must read");
    assert_eq!(title, "Website rebuild");
}

#[tokio::test]
async fn every_refusal_names_the_field_it_is_rendered_under() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;

    let cases: Vec<(&str, Value, &str)> = vec![
        ("no customer", json!({ "lines": [{ "description": "x" }] }), "customer_id"),
        (
            "no lines",
            json!({ "customer_id": fixture.company, "lines": [] }),
            "lines",
        ),
        (
            "a line with neither a product nor a description",
            json!({ "customer_id": fixture.company, "lines": [{ "quantity": "1" }] }),
            "description",
        ),
        (
            "a zero quantity",
            json!({ "customer_id": fixture.company, "lines": [{ "description": "x", "quantity": "0" }] }),
            "quantity",
        ),
        (
            "a discount over 100",
            json!({ "customer_id": fixture.company,
                    "lines": [{ "description": "x", "discount_percent": 140 }] }),
            "discount_percent",
        ),
        (
            "a four-letter currency",
            json!({ "customer_id": fixture.company, "currency": "TRYL",
                    "lines": [{ "description": "x" }] }),
            "currency",
        ),
    ];

    for (label, body, field) in cases {
        let response = call(
            &fixture.state,
            request(Method::POST, "/api/v1/sales/quotes", Some(&drafter), Some(body)),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{label}: {}", response.body);
        assert_eq!(
            response.body["error"]["details"]["field"].as_str(),
            Some(field),
            "{label} must name the field: {}",
            response.body
        );
    }

    // A validity in the past is refused at the field too.
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sales/quotes",
            Some(&drafter),
            Some(json!({
                "customer_id": fixture.company,
                "valid_until": "2020-01-01",
                "lines": [{ "description": "x" }]
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "{}", response.body);
    assert_eq!(response.body["error"]["details"]["field"].as_str(), Some("valid_until"));
}

#[tokio::test]
async fn a_cancelled_quote_is_final_and_says_why() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let (id, _) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/cancel"),
            Some(&drafter),
            Some(json!({ "reason": "Customer went with another supplier" })),
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.body);
    assert_eq!(cancelled.body["quote"]["status"].as_str(), Some("cancelled"));
    assert_eq!(
        cancelled.body["cancel_reason"].as_str(),
        Some("Customer went with another supplier")
    );

    // Final: a decided quote cannot be cancelled again, nor edited.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/cancel"),
            Some(&drafter),
            Some(json!({ "reason": "changed my mind" })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST, "{}", again.body);

    let edit = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/sales/quotes/{id}"),
            Some(&drafter),
            Some(json!({ "title": "back please" })),
        ),
    )
    .await;
    assert_eq!(edit.status, StatusCode::CONFLICT, "{}", edit.body);
}

// ---------------------------------------------------------------------------------------------
// The list, the vocabulary, audit and events
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_list_filters_composes_and_refuses_an_unknown_sort() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;

    let (draft_id, _) =
        create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;
    let (sent_id, _) =
        create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{sent_id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;

    let draft_number: (String,) = sqlx::query_as("select number from sales_quotes where id = $1")
        .bind(draft_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote must read");
    let sent_number: (String,) = sqlx::query_as("select number from sales_quotes where id = $1")
        .bind(sent_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote must read");

    // Search by the exact number of the draft.
    let found = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/quotes?search={}", draft_number.0),
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.body);
    let items = found.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{}", found.body);
    assert_eq!(items[0]["id"].as_str(), Some(draft_id.to_string().as_str()));

    // A status filter.
    let sent_only = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/quotes?search={}&status=sent", sent_number.0),
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(sent_only.status, StatusCode::OK, "{}", sent_only.body);
    assert_eq!(
        sent_only.body["items"].as_array().map(Vec::len),
        Some(1),
        "{}",
        sent_only.body
    );
    assert_eq!(
        sent_only.body["items"][0]["status"].as_str(),
        Some("sent")
    );

    // An unknown status and an unknown sort are both refused, with the accepted values named.
    let bad_status = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/quotes?status=quoted",
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(bad_status.status, StatusCode::BAD_REQUEST, "{}", bad_status.body);

    let bad_sort = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/quotes?sort=nonsense",
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(bad_sort.status, StatusCode::BAD_REQUEST, "{}", bad_sort.body);
    let message = bad_sort.body["error"]["message"].as_str().unwrap_or_default();
    for accepted in ["updated", "number", "valid_until", "total"] {
        assert!(message.contains(accepted), "the message names `{accepted}`: {message}");
    }

    // A bad day filter is refused rather than silently ignored.
    let bad_day = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/quotes?valid_from=next+tuesday",
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(bad_day.status, StatusCode::BAD_REQUEST, "{}", bad_day.body);
}

#[tokio::test]
async fn the_vocabulary_names_every_status_and_the_organizations_own_defaults() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;

    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sales/quotes/vocabulary", Some(&drafter), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let statuses = response.body["statuses"].as_array().expect("statuses");
    assert_eq!(statuses.len(), 8, "one per status the module knows");
    for status in statuses {
        assert!(
            status["label"].as_str().is_some_and(|label| !label.is_empty()),
            "every status has a label: {status}"
        );
    }
    // The open flag the overview counts on is the module's own definition.
    let draft = statuses
        .iter()
        .find(|status| status["value"].as_str() == Some("draft"))
        .expect("draft");
    assert_eq!(draft["open"].as_bool(), Some(true));
    let accepted = statuses
        .iter()
        .find(|status| status["value"].as_str() == Some("accepted"))
        .expect("accepted");
    assert_eq!(accepted["open"].as_bool(), Some(false));
    assert!(response.body["default_currency"].as_str().is_some());
    assert!(response.body["discount_approval_threshold"].as_i64().is_some());
}

#[tokio::test]
async fn the_quote_lifecycle_writes_audit_rows_and_emits_the_documented_events() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let seller = fixture.token(&fixture.seller).await;
    let (id, _) = create_quote(&fixture, &drafter, quote_body(fixture.company, fixture.product)).await;

    // Created: an audit row with the actor, the target and the totals.
    let created = audit_rows(&fixture.db, "sales.quote.created").await;
    assert!(!created.is_empty(), "creating a quote writes an audit row");
    let row = &created[0];
    assert_eq!(row["target_type"].as_str(), Some("sales_quote"));
    assert_eq!(row["target_id"].as_str(), Some(id.to_string().as_str()));
    assert_eq!(row["metadata"]["after"]["grand_total"].as_str(), Some(FIXTURE_GRAND));
    // The grid is not in the audit row: the compliance screen reads these and a hundred-line grid
    // would make the row unreadable.
    assert!(row["metadata"]["after"].get("lines").is_none());

    // And the event, with the documented payload.
    let events = event_payloads(&fixture.db, "sales.quote.created").await;
    assert!(!events.is_empty(), "creating a quote emits sales.quote.created");
    assert!(
        events[0]["number"].as_str().is_some_and(|n| n.starts_with("Q-")),
        "the event carries the document number: {}",
        events[0]
    );
    assert_eq!(events[0]["grand_total"].as_str(), Some(FIXTURE_GRAND));

    // Sent.
    let sent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/send"),
            Some(&seller),
            None,
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    assert!(
        !audit_rows(&fixture.db, "sales.quote.sent").await.is_empty(),
        "sending writes an audit row"
    );
    assert!(!event_payloads(&fixture.db, "sales.quote.sent").await.is_empty());

    // Accepted through the public link, with no actor: the event is one of the spec's pinned
    // integration hooks, so its payload is the documented document.
    let token = issue_link(&fixture, &seller, id).await;
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/public/quotes/{token}/accept"),
            None,
            None,
        ),
    )
    .await;
    let accepted_events = event_payloads(&fixture.db, "sales.quote.accepted").await;
    assert!(!accepted_events.is_empty(), "accepting emits sales.quote.accepted");
    assert!(
        accepted_events[0]["number"].as_str().is_some_and(|n| n.starts_with("Q-")),
        "the pinned integration event carries the number: {}",
        accepted_events[0]
    );
    assert_eq!(accepted_events[0]["grand_total"].as_str(), Some(FIXTURE_GRAND));
    // Nothing about the seller's side travels to a webhook subscriber.
    assert!(accepted_events[0].get("price_list_id").is_none());
    assert!(!audit_rows(&fixture.db, "sales.quote.accepted").await.is_empty());
}
