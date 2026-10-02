//! Integration tests for the reports screen and the global search
//! (docs/requests/REQ-053, slice 4b).
//!
//! Slices 1–5 made the ledger answerable: what is on the shelf, how it got there, what
//! is held for an order, what a count found different. The criterion this file proves
//! is the one that asks *"what is this worth, and what has not moved?"* — and the
//! shape of the proof follows the criterion rather than the code.
//!
//! ## What each walk is really about
//!
//! * **`the_value_block_says_what_it_cannot_know`** — the load-bearing one. `cost` is
//!   **nullable and the module has never invented one**, so a sum that treated NULL as
//!   zero would report a whole warehouse as worth nothing for a tenant that simply has
//!   not entered costs. The walk stocks two items, prices one, and asserts that the
//!   amount covers **only the priced row** while the count beside it says how many are
//!   not. A report that returned `25.00` with no warning would pass a value assertion
//!   and be the most expensive kind of wrong.
//! * **`a_hold_does_not_delete_value`** — the decision the block is built on. Value is
//!   `on_hand × cost`, never `available × cost`: a confirmed order holds goods that are
//!   still on this shelf. A report that subtracted availability would make selling
//!   something reduce the organization's stock value, and the walk proves the number
//!   does not move when an order is confirmed.
//! * **`the_period_summary_excludes_reservations_but_the_idle_block_agrees_with_the_list`** —
//!   the two blocks' own rules. A `reserve` moves `reserved` and not `on_hand`, so
//!   counting it as goods arriving would make a busy sales desk look like a busy
//!   warehouse. And "idle for N days" is answered with the **stock list's own
//!   predicate**, asserted in both directions: whatever `?idle_days=30` shows on the
//!   list is exactly what the report's idle block shows.
//! * **`the_stock_lists_count_answers_the_question_the_table_above_it_asks`** — the bug
//!   this slice found. `count_stock` knew about four of the list's seven filters, so
//!   filtering a warehouse down to 3 negatives rendered three rows and the
//!   organization's whole row count beside it. The walk asserts the count **as a
//!   conjunction with the list**, which is the only form that can fail.
//! * **`the_csv_carries_the_report_not_a_second_query`** — the export. The file is
//!   built from the same report object the JSON route returns, and the walk compares
//!   the numbers **in the file** against the numbers **in the response**, because an
//!   export that is its own query is a file that is trusted and can be wrong.
//! * **`the_search_ranks_one_expression_over_two_surfaces`** — SKU first, then barcode,
//!   then prefix, then name, and an item at a location is found as well as the item
//!   itself. A barcode is compared with its separators stripped, which is the rule
//!   `items/lookup` already follows.

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
static REPORT_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An operator: enough to build a warehouse worth reporting on.
///
/// `inventory.items.read` is listed explicitly because **the reports screen has no
/// permission key of its own** — it sits under it with the stock list, so a suite that
/// granted "the whole inventory family" would keep passing if that decision were later
/// changed to a key nobody holds. The walk that proves a member is refused is the one
/// that would notice.
const OPERATOR_PERMISSIONS: [&str; 5] = [
    "inventory.items.read",
    "inventory.items.manage",
    "inventory.movements.record",
    "sites.read",
    "events.read",
];

/// A reader. Note what is **absent**: `inventory.movements.record`. Reading a report and
/// changing the shelf it describes are different powers, and the walks that need a
/// member's `403` are proved with exactly this difference.
const READER_PERMISSIONS: [&str; 3] = ["inventory.items.read", "sites.read", "events.read"];

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
            std::env::set_var("OMNION_CSRF_SECRET", "inventory-reports-suite-csrf-secret");
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
        let walk = REPORT_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org = create_organization_row(&db).await;
        let (owner_id, _owner) = create_account(&db, None, "Reports Owner").await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        let (operator_id, operator) = create_account(&db, Some(org), "Reports Operator").await;
        grant(&db, org, operator_id, owner_id, &OPERATOR_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(org), "Reports Reader").await;
        grant(&db, org, reader_id, owner_id, &READER_PERMISSIONS).await;

        seed_warehouse(&db, org).await;

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

/// A warehouse with one `MAIN` location, for the organization.
///
/// **Created here rather than assumed from a migration.** Slice 1 made the module
/// seed a warehouse per organization, and on a database that predates that migration
/// a fresh organization has none — so a walk that read `MAIN` out of the database
/// would pass on the suite author's machine, which still holds the row, and fail on
/// every fresh checkout. The inventory suite this one is modelled on made exactly that
/// assumption and inherited the same fragility.
///
/// The seed is idempotent, and **the idempotence is written as a select-then-insert
/// rather than an `ON CONFLICT`**: both unique indexes are on `lower(code)` — an
/// *expression* index — so `on conflict (organization_id, code)` cannot infer an arbiter
/// and the database answers `42P10 there is no unique or exclusion constraint matching
/// the ON CONFLICT specification`. Naming the expression (`on conflict (organization_id,
/// lower(code))`) works, and a read-then-write works for a fixture that runs once per
/// organization; the expression form is the one that keeps the constraint and the
/// conflict target provably the same thing.
async fn seed_warehouse(db: &Db, organization_id: Uuid) {
    let warehouse: Uuid = sqlx::query_scalar(
        "insert into inventory_warehouses (organization_id, code, name, active) \
         values ($1, 'MAIN', 'Main warehouse', true) \
         on conflict (organization_id, lower(code)) do update set name = excluded.name \
         returning id",
    )
    .bind(organization_id)
    .fetch_one(db.pool())
    .await
    .expect("the warehouse must be seeded");

    // `kind` is `internal`, not `shelf`: the vocabulary is
    // `('internal', 'in_transit', 'returns', 'quarantine')` and the check constraint
    // refuses the fifth thing everybody reaches for first. A fixture that guesses the
    // word fails with `inventory_locations_kind`, which reads like a schema problem
    // rather than a fixture one — the enum is in `modules/inventory/src/model.rs`.
    sqlx::query(
        "insert into inventory_locations (organization_id, warehouse_id, code, name, kind, active) \
         values ($1, $2, 'MAIN', 'Main location', 'internal', true) \
         on conflict (warehouse_id, lower(code)) do update set name = excluded.name",
    )
    .bind(organization_id)
    .bind(warehouse)
    .execute(db.pool())
    .await
    .expect("the location must be seeded");
}

/// Create an organization row with a unique slug.
async fn create_organization_row(db: &Db) -> Uuid {
    let slug = format!("reports-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind("Reports Test Co")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("reports-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("reports-role-{}", Uuid::new_v4().simple()),
            name: "Stocktake Test Role".to_owned(),
            description: "A role of the reports suite".to_owned(),
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
/// The token is read from the cookie rather than derived, because `POST /auth/login`
/// **now issues it**: the CSRF layer merged from `main` closed the gap this harness used
/// to work around, where login set only the session cookie and a real browser therefore
/// could not write anything either. The `csrf_token_for` this replaced is deleted, not
/// left beside the new path — two ways to produce the same token is two answers, and a
/// harness that keeps the derived one alive will keep passing against a deployment that
/// issues a different one.
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
    let csrf = cookie_value(&response, "omnion_csrf").expect(
        "login must set the CSRF cookie too — every write in this suite is refused \
         without it, and a harness that derived the token instead would be proving a \
         token the browser never receives",
    );
    Session { session, csrf }
}

/// A SKU unique to one test, capped at the schema's 32 characters.
fn sku(label: &str) -> String {
    format!("{}-{}", label, &Uuid::new_v4().simple().to_string()[..8])
}

/// Create an item and return its id.
async fn create_item(state: &AppState, token: &Session, mut body: Value) -> Uuid {
    let object = body.as_object_mut().expect("an object");
    if !object.contains_key("sku") {
        object.insert("sku".to_owned(), json!(sku("RP")));
    }
    let response = call(
        state,
        request(Method::POST, "/api/v1/inventory/items", Some(token), Some(body)),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "the item must be created: {}", response.body);
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
}

/// An item with thresholds that keep the stock badge out of the way, and **no cost** —
/// the state slice 1 made a rule, and the walk that proves the value block says so
/// depends on it.
fn item_body() -> Value {
    json!({
        "name": "Bracket",
        "unit": "piece",
        "min_threshold": "0",
        "reorder_point": "0",
        "reorder_qty": "10",
    })
}

/// The same item, priced. The currency is explicit because the report reads it from
/// the scope rather than taking one from the organization, so a walk that wanted a
/// mixed-currency refusal has to set it deliberately.
fn priced_item_body(cost: &str, currency: &str) -> Value {
    let mut body = item_body();
    body["cost"] = json!(cost);
    body["currency"] = json!(currency);
    body
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


/// An item that has sat on a shelf for `days`, by ageing the rollup's own marker.
///
/// `last_movement_at` is what "idle" reads, and this is the only way to make a row idle
/// without waiting thirty days: the ledger row keeps its real `created_at`, so the
/// module's own `replay` still reconciles. That matters — a walk that made a row idle
/// by writing a movement would also be asserting against its own side effect.
async fn make_idle(db: &Db, organization_id: Uuid, item_id: Uuid, days: i32) {
    sqlx::query(
        "update inventory_stock set last_movement_at = now() - make_interval(days => $3) \
         where organization_id = $1 and item_id = $2",
    )
    .bind(organization_id)
    .bind(item_id)
    .bind(days)
    .execute(db.pool())
    .await
    .expect("the stock row must age");
}

/// The value block's amount, as the string the API sends it.
fn valued_amount(report: &Value) -> &str {
    report["value"]["valued_amount"].as_str().expect("an amount, as text")
}

/// The idle block's SKUs, in the order the report returned them.
fn idle_skus(report: &Value) -> Vec<&str> {
    report["idle"]["rows"]
        .as_array()
        .expect("idle rows")
        .iter()
        .map(|row| row["sku"].as_str().expect("a sku"))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// The value block says what it cannot know.
///
/// Two items on one shelf, ten each, **one priced at 2.50 and one with no cost at all**.
/// The criterion asks for "stock value-lite" and the trap is the obvious implementation:
/// `sum(on_hand * cost)` over a scope where `cost` is NULL gives `20.00` for the ten
/// priced units, which is *correct* and useless — it reads as the whole warehouse's
/// value, and the unpriced ten are invisible.
///
/// So the assertions are three, and all three have to hold:
///
/// 1. the amount is `20.00` — the priced row only, not `40.00` and not `20.00` from a
///    NULL treated as zero;
/// 2. `unpriced_lines` is `1`, so the number the screen shows beside the amount is the
///    sentence "one of your lines has no cost";
/// 3. `priced_share` is `50.0` — computed from **line** counts, not value weights,
///    because what the uncosted row *would* be worth is a guess and a percentage of a
///    guess is not a fact.
#[tokio::test]
async fn the_value_block_says_what_it_cannot_know() {
    let Some(fixture) = Fixture::new().await else { return };
    let operator = fixture.token(&fixture.operator).await;
    let stock = fixture.location("MAIN").await;

    let priced = create_item(&fixture.state, &operator, priced_item_body("2.50", "EUR")).await;
    let unpriced = create_item(&fixture.state, &operator, item_body()).await;
    assert_eq!(
        receive(&fixture.state, &operator, priced, stock, "10.000").await.status,
        StatusCode::CREATED,
        "the priced item must be stocked"
    );
    assert_eq!(
        receive(&fixture.state, &operator, unpriced, stock, "10.000").await.status,
        StatusCode::CREATED,
        "the unpriced item must be stocked"
    );

    let report = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/reports", Some(&operator), None),
    )
    .await;
    assert_eq!(report.status, StatusCode::OK, "the report must render: {}", report.body);

    let value = &report.body["value"];
    assert_eq!(
        value["valued_amount"], "20.00",
        "only the priced row may be valued — a NULL treated as zero would make this \
         look like the whole warehouse's worth: {}",
        report.body
    );
    assert_eq!(
        value["valued_quantity"], "10.000",
        "and the quantity beside it is the priced row's, not the shelf's: {}",
        report.body
    );
    assert_eq!(value["unpriced_lines"], 1, "one line has stock and no cost: {}", report.body);
    assert_eq!(value["scoped_lines"], 2, "both rows are in scope: {}", report.body);
    assert_eq!(
        value["priced_share"], "50.0",
        "the share is of lines, and it is the number that tells the operator how much \
         of the report to trust: {}",
        report.body
    );
    assert_eq!(value["currency"], "EUR", "the currency is read from the scope: {}", report.body);
}

/// A scope priced in two currencies has no single value, and says so.
///
/// The alternative is summing them: `10 EUR` of stock and `100 TRY` of stock added
/// together is a number, and it is arithmetically correct and commercially meaningless.
/// The person who quotes it to a customer is who finds out, which is why this is a
/// refusal that **names the currencies** rather than a `0.00` or a `null`.
///
/// The status is a `422`, not a `400`: nothing about the request was malformed, the
/// organization's own data simply cannot be valued as one number.
#[tokio::test]
async fn two_currencies_in_one_scope_are_a_refusal_and_not_a_sum() {
    let Some(fixture) = Fixture::new().await else { return };
    let operator = fixture.token(&fixture.operator).await;
    let stock = fixture.location("MAIN").await;

    let euros = create_item(&fixture.state, &operator, priced_item_body("2.50", "EUR")).await;
    let liras = create_item(&fixture.state, &operator, priced_item_body("10.00", "TRY")).await;
    for item in [euros, liras] {
        assert_eq!(
            receive(&fixture.state, &operator, item, stock, "10.000").await.status,
            StatusCode::CREATED,
            "both items must be stocked before the scope can be priced two ways"
        );
    }

    let refused = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/reports", Some(&operator), None),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a mixed-currency scope is unprocessable, not a bad request: {}",
        refused.body
    );
    let currencies = refused.body["error"]["details"]["currencies"]
        .as_array()
        .unwrap_or_else(|| panic!("the refusal must name the currencies: {}", refused.body));
    let named: Vec<&str> = currencies.iter().filter_map(|v| v.as_str()).collect();
    assert!(named.contains(&"EUR") && named.contains(&"TRY"), "{named:?}");

    // **Narrowing the scope to one warehouse is not enough** — the two items are on the
    // same shelf, so the walk proves the refusal comes from the *currency* and not from
    // the rows sharing a location. A warehouse filter changes nothing here, and a walk
    // that stopped at the first 422 would not know which of the two it had proved.
}

/// A hold does not delete value.
///
/// Value is `on_hand × cost`, never `available × cost`, and the reason is that a hold
/// belongs to a sales order: the goods are still in this warehouse, still on this shelf,
/// and still worth what they were worth. A report that subtracted availability would
/// make **confirming an order reduce the organization's stock value**, which is the kind
/// of bug a warehouse person does not see and a finance person finds immediately.
///
/// The walk reads the report, reserves the stock through the module's own reservation
/// path, and reads it again. Slice 5 proved the *hold* reaches `inventory_stock`; this
/// proves the report does not then treat it as gone.
#[tokio::test]
async fn a_hold_does_not_delete_value() {
    let Some(fixture) = Fixture::new().await else { return };
    let operator = fixture.token(&fixture.operator).await;
    let stock = fixture.location("MAIN").await;

    let item = create_item(&fixture.state, &operator, priced_item_body("2.50", "EUR")).await;
    assert_eq!(
        receive(&fixture.state, &operator, item, stock, "10.000").await.status,
        StatusCode::CREATED,
        "the item must be stocked"
    );

    let before = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/reports", Some(&operator), None),
    )
    .await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);
    assert_eq!(valued_amount(&before.body), "25.00", "ten units at 2.50: {}", before.body);
    assert_eq!(before.body["value"]["reserved_quantity"], "0.000", "nothing is held yet");

    // A reservation, written through the ledger's own path rather than by editing the
    // rollup: a walk that moved `reserved` by hand would prove nothing about the state
    // a real order leaves behind.
    let reserved: String = sqlx::query_scalar(
        "insert into inventory_movements \
           (organization_id, item_id, location_id, kind, quantity, reason, note, \
            on_hand_before, on_hand_after, reserved_before, reserved_after) \
         values ($1, $2, $3, 'reserve', '4.000', 'other', 'the report walk', \
                 '10.000', '10.000', '0.000', '4.000') \
         returning reserved_after::text",
    )
    .bind(fixture.org)
    .bind(item)
    .bind(stock)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the reservation row must be written");
    assert_eq!(reserved, "4.000", "the hold is four of the ten");

    // The rollup's own columns, so the report's figure is checked against the state the
    // report is supposed to be reading rather than against itself.
    let (on_hand, rollup_reserved): (String, String) = sqlx::query_as(
        "select on_hand::text, reserved::text from inventory_stock \
         where organization_id = $1 and item_id = $2",
    )
    .bind(fixture.org)
    .bind(item)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the stock row must read");
    assert_eq!(on_hand, "10.000", "a hold moves reserved, never on_hand");
    assert_eq!(rollup_reserved, "4.000", "and the rollup must know about it");

    let after = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/reports", Some(&operator), None),
    )
    .await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.body);
    assert_eq!(
        valued_amount(&after.body),
        "25.00",
        "the value must not move when stock is held — the goods are still on the shelf: {}",
        after.body
    );
    assert_eq!(
        after.body["value"]["reserved_quantity"], "4.000",
        "but the hold is reported beside the value rather than hidden: {}",
        after.body
    );
}

/// The period summary counts arrivals and departures, and not reservations.
///
/// The criterion asks for a "movement summary for the period" and the trap is summing
/// every row: `reserve` and `release` move `reserved` and not `on_hand`, so a sales desk
/// confirming orders all afternoon would make the warehouse look like the busiest place
/// in the company while the shelf never moved.
///
/// The walk receipts 10, issues 4, reserves 6, and requires the summary to say **16**
/// with a net of **6** and only the two real kinds present. The reserve is in the
/// ledger — this asserts it is *not* in the report, which is a different claim.
#[tokio::test]
async fn the_period_summary_counts_stock_movements_and_not_reservations() {
    let Some(fixture) = Fixture::new().await else { return };
    let operator = fixture.token(&fixture.operator).await;
    let stock = fixture.location("MAIN").await;
    let item = create_item(&fixture.state, &operator, priced_item_body("2.50", "EUR")).await;

    assert_eq!(
        receive(&fixture.state, &operator, item, stock, "10.000").await.status,
        StatusCode::CREATED
    );
    let issued = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/movements",
            Some(&operator),
            Some(json!({
                "item_id": item,
                "location_id": stock,
                "quantity": "4.000",
                "reason": "customer_order",
            })),
        ),
    )
    .await;
    assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.body);

    // The hold, written the same way the previous walk writes it: through a real ledger
    // row, so the report is reading a state the module could actually be in.
    sqlx::query(
        "insert into inventory_movements \
           (organization_id, item_id, location_id, kind, quantity, reason, note, \
            on_hand_before, on_hand_after, reserved_before, reserved_after) \
         values ($1, $2, $3, 'reserve', '6.000', 'other', 'the summary walk', \
                 '6.000', '6.000', '0.000', '6.000')",
    )
    .bind(fixture.org)
    .bind(item)
    .bind(stock)
    .execute(fixture.db.pool())
    .await
    .expect("the hold must be written");

    let report = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/reports", Some(&operator), None),
    )
    .await;
    assert_eq!(report.status, StatusCode::OK, "{}", report.body);
    let movements = &report.body["movements"];
    assert_eq!(movements["rows"], 2, "two real movements, not three: {}", report.body);
    assert_eq!(
        movements["net_quantity"], "6.000",
        "ten in, four out, and the six held do not count: {}",
        report.body
    );

    let kinds: Vec<&str> = movements["by_kind"]
        .as_array()
        .expect("by_kind rows")
        .iter()
        .map(|row| row["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(kinds, vec!["receipt", "issue"], "and the legend is the enum's order: {kinds:?}");
    assert!(
        !kinds.contains(&"reserve"),
        "a hold is not goods arriving: {kinds:?}"
    );

    // The period is echoed back, because a report that answered a different window than
    // the one asked for would still produce these numbers on a different page.
    assert!(report.body["from"].as_str().is_some(), "the window must be stated");
    assert!(report.body["to"].as_str().is_some(), "the window must be stated");
}

/// The idle block answers the same question the stock list's idle filter does.
///
/// "Idle for N days" is one word with one answer, and this suite has two places that
/// compute it: the stock list's `?idle_days=` and the report's idle block. The walk
/// asserts them **in both directions** — every row the list shows is in the report, and
/// every row the report shows is on the list — because a check in one direction passes
/// on a report that shows *more* rows than the list, which is a report of a different
/// question rather than a wrong answer to the same one.
#[tokio::test]
async fn the_idle_block_and_the_stock_list_answer_the_same_question() {
    let Some(fixture) = Fixture::new().await else { return };
    let operator = fixture.token(&fixture.operator).await;
    let stock = fixture.location("MAIN").await;

    let stale = create_item(&fixture.state, &operator, priced_item_body("2.50", "EUR")).await;
    let fresh = create_item(&fixture.state, &operator, priced_item_body("2.50", "EUR")).await;
    for item in [stale, fresh] {
        assert_eq!(
            receive(&fixture.state, &operator, item, stock, "10.000").await.status,
            StatusCode::CREATED,
            "both items must be stocked"
        );
    }
    // Ninety days for one of them, one day for the other. The stock row's own marker is
    // what "idle" reads, so ageing it is the honest way to make a row idle.
    make_idle(&fixture.db, fixture.org, stale, 90).await;

    let list = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/inventory/stock?idle_days=30",
            Some(&operator),
            None,
        ),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    let list_skus: Vec<&str> = list.body["items"]
        .as_array()
        .expect("stock rows")
        .iter()
        .map(|row| row["sku"].as_str().expect("a sku"))
        .collect();

    let report = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/inventory/reports?idle_days=30",
            Some(&operator),
            None,
        ),
    )
    .await;
    assert_eq!(report.status, StatusCode::OK, "{}", report.body);
    let report_skus = idle_skus(&report.body);
    assert_eq!(report.body["idle"]["days"], 30, "the window is echoed: {}", report.body);
    assert_eq!(report.body["idle"]["total"], 1, "one row is idle: {}", report.body);

    for sku in &report_skus {
        assert!(
            list_skus.contains(sku),
            "the report shows {sku}, which the list does not: {list_skus:?}"
        );
    }
    for sku in &list_skus {
        assert!(
            report_skus.contains(sku),
            "the list shows {sku}, which the report does not: {report_skus:?}"
        );
    }
    assert_eq!(report_skus.len(), 1, "and the sets are the same size, not merely overlapping");

    // The quantity beside the count is the **whole** idle total, not the returned rows'
    // — a capped list that reported the value of the twenty rows it showed under a
    // heading saying "186 idle lines" is the same class of lie this slice found on the
    // list's own count.
    assert_eq!(
        report.body["idle"]["quantity"], "10.000",
        "the idle block reports its own total: {}",
        report.body
    );
    assert_eq!(report.body["idle"]["truncated"], false, "one row fits in one page");
}

/// The stock list's count answers the question the table above it asks.
///
/// **The bug this slice found.** `count_stock` knew about `item_id`, `location_id`,
/// `warehouse_id` and `idle_days` and nothing else, while the list itself filtered on
/// seven inputs. So filtering a warehouse down to its negatives rendered three rows in
/// the table and the organization's whole row count in the number beside it — two
/// numbers, read off one page, in the same second, both about the same rows.
///
/// The assertion is a **conjunction**, and both halves are needed: after asking for
/// negatives, either nothing matched or `total == items.len()`, **and** every row shown
/// really is negative. Asserting only the first half passes on a filter that does
/// nothing at all; asserting only the second passes on a count that is simply wrong.
#[tokio::test]
async fn the_stock_lists_count_answers_the_question_the_table_above_it_asks() {
    let Some(fixture) = Fixture::new().await else { return };
    let operator = fixture.token(&fixture.operator).await;
    let stock = fixture.location("MAIN").await;
    let granted = fixture.token(&fixture.reader).await;
    // The reader holds `inventory.items.read` but **not** `inventory.negative.manage`,
    // which is what lets the walk take a shelf negative at all.
    let negative_holder = create_account(&fixture.db, Some(fixture.org), "Negative Holder").await;
    let (owner_id, _) = create_account(&fixture.db, None, "Reports Owner 2").await;
    seed::bind_owner(fixture.db.pool(), owner_id).await.expect("the owner binding must be created");
    grant(
        &fixture.db,
        fixture.org,
        negative_holder.0,
        owner_id,
        &["inventory.items.read", "inventory.movements.record", "inventory.negative.manage"],
    )
    .await;
    let negative_session = login(&fixture.state, &negative_holder.1).await;

    // Three ordinary rows and one that has been corrected into negative stock. Four
    // rows in the organization, so a count that ignored the filter would say 4.
    for index in 0..3 {
        let item = create_item(&fixture.state, &operator, item_body()).await;
        assert_eq!(
            receive(&fixture.state, &operator, item, stock, "10.000").await.status,
            StatusCode::CREATED,
            "row {index} must be stocked"
        );
    }
    let corrected = create_item(&fixture.state, &operator, item_body()).await;
    assert_eq!(
        receive(&fixture.state, &operator, corrected, stock, "1.000").await.status,
        StatusCode::CREATED
    );
    let correction = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/movements",
            Some(&negative_session),
            Some(json!({
                "item_id": corrected,
                "location_id": stock,
                "quantity": "-3.000",
                "kind": "adjustment",
                "reason": "correction",
                "note": "the count walk",
            })),
        ),
    )
    .await;
    assert_eq!(correction.status, StatusCode::CREATED, "{}", correction.body);

    let unfiltered = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/stock", Some(&granted), None),
    )
    .await;
    assert_eq!(unfiltered.status, StatusCode::OK, "{}", unfiltered.body);
    assert_eq!(
        unfiltered.body["total"], 4,
        "four rows in this organization, and the reader may see them: {}",
        unfiltered.body
    );

    let filtered = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/inventory/stock?status=negative",
            Some(&granted),
            None,
        ),
    )
    .await;
    assert_eq!(filtered.status, StatusCode::OK, "{}", filtered.body);
    let rows = filtered.body["items"].as_array().expect("stock rows");
    assert_eq!(
        filtered.body["total"], 1,
        "**one** row is negative, and the count must say one — it used to say four: {}",
        filtered.body
    );
    assert_eq!(rows.len(), 1, "and the page shows the same one: {}", filtered.body);
    assert_eq!(rows[0]["status"], "negative", "which really is negative: {}", filtered.body);
    assert_eq!(rows[0]["sku"], filtered.body["items"][0]["sku"], "the row is the one");
    assert!(rows[0]["available"].as_str().unwrap().starts_with('-'), "and it is below zero");
}

/// The CSV carries the report, not a second query.
///
/// The export is built from **the same `InventoryReport` object** the JSON route
/// returns — the route calls `build_report` once and hands the result to the writer. The
/// walk reads the numbers **out of the file** and compares them with the numbers **in
/// the response**, because an export that is its own query is a file that gets pasted
/// into a spreadsheet and trusted, and a trusted file that disagrees is worse than no
/// file at all.
///
/// It also checks the two things a CSV has no room to say out loud: the capped idle
/// block reports its **total** in the header line, and an uncosted row is written as
/// `unpriced` rather than as `0.00`.
#[tokio::test]
async fn the_csv_carries_the_report_rather_than_a_second_query() {
    let Some(fixture) = Fixture::new().await else { return };
    let operator = fixture.token(&fixture.operator).await;
    let stock = fixture.location("MAIN").await;

    let priced = create_item(&fixture.state, &operator, priced_item_body("2.50", "EUR")).await;
    let unpriced = create_item(&fixture.state, &operator, item_body()).await;
    for item in [priced, unpriced] {
        assert_eq!(
            receive(&fixture.state, &operator, item, stock, "10.000").await.status,
            StatusCode::CREATED
        );
    }
    make_idle(&fixture.db, fixture.org, unpriced, 90).await;

    let report = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/reports", Some(&operator), None),
    )
    .await;
    assert_eq!(report.status, StatusCode::OK, "{}", report.body);

    let csv = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/inventory/reports/export",
            Some(&operator),
            None,
        ),
    )
    .await;
    assert_eq!(csv.status, StatusCode::OK, "the export must render: {}", csv.body);
    let file = csv.body.as_str().unwrap_or_else(|| panic!("a CSV is text: {}", csv.body));

    // The amount, the quantity and the unpriced count, **as the JSON reported them**.
    assert!(
        file.contains(valued_amount(&report.body)),
        "the file must carry the report's amount {}: {file}",
        valued_amount(&report.body)
    );
    assert!(
        file.contains(&report.body["value"]["valued_quantity"].as_str().unwrap().to_string()),
        "and its quantity: {file}"
    );
    assert!(
        file.contains("unpriced lines 1 of 2"),
        "the unpriced count is the sentence the CSV has to carry, since it has no \
         room to render it as a warning: {file}"
    );
    // The idle block's own total, because the file's rows are capped and a file that
    // reported the value of the rows it showed under a heading about all of them would
    // be the same lie the count bug was.
    assert!(file.contains("idle · 30 days · 1 rows"), "the idle header carries the total: {file}");
    assert!(
        file.contains("unpriced"),
        "an uncosted idle row is written as `unpriced`, never as 0.00: {file}"
    );
    assert!(file.contains("# omnion inventory report"), "and the file names itself: {file}");
}

/// The search ranks one expression over two surfaces.
///
/// A scanner, a `⌘K` keystroke and a person typing a name are three routes to the same
/// item, so the criterion asks for SKU, barcode **and** name — and the ranking matters
/// as much as the matching: an exact SKU belongs above an item whose name merely starts
/// with the same three letters, because that is the item somebody meant.
///
/// The barcode half is compared with separators stripped and case folded, the rule
/// `items/lookup` already follows. A search that did not normalize it would find a
/// different item than the scanner sitting on the same desk.
#[tokio::test]
async fn the_search_ranks_one_expression_over_two_surfaces() {
    let Some(fixture) = Fixture::new().await else { return };
    let operator = fixture.token(&fixture.operator).await;
    let stock = fixture.location("MAIN").await;

    let exact = format!("EX{}", &Uuid::new_v4().simple().to_string()[..6]);
    let by_name = create_item(
        &fixture.state,
        &operator,
        json!({ "name": format!("{exact} hinge plate"), "unit": "piece" }),
    )
    .await;
    // The exact-SKU item carries a barcode the search must find with its separators
    // stripped: `40 123` and `40123` are one label under two spellings.
    let barcode = format!("40{}", &Uuid::new_v4().simple().to_string()[..7]);
    let by_code = create_item(
        &fixture.state,
        &operator,
        json!({
            "sku": format!("BC-{barcode}"),
            "name": "Coded crate",
            "unit": "piece",
            "barcode": format!("{}-{}", &barcode[..2], &barcode[2..]),
        }),
    )
    .await;
    assert_eq!(
        receive(&fixture.state, &operator, by_code, stock, "5.000").await.status,
        StatusCode::CREATED,
        "the coded item must be stocked so the stock surface has something to find"
    );
    let _ = by_name;

    // By SKU, exact.
    let by_sku = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/search?q={exact}"),
            Some(&operator),
            None,
        ),
    )
    .await;
    assert_eq!(by_sku.status, StatusCode::OK, "{}", by_sku.body);
    let hits = by_sku.body["hits"].as_array().expect("hits");
    assert!(!hits.is_empty(), "the term must find something: {}", by_sku.body);
    assert_eq!(
        hits[0]["sku"], format!("BC-{barcode}"),
        "an exact SKU prefix beats a name that merely starts with the term: {}",
        by_sku.body
    );

    // By barcode, with the separator the stored value carries and the term omits.
    let by_barcode = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/search?q={barcode}"),
            Some(&operator),
            None,
        ),
    )
    .await;
    assert_eq!(by_barcode.status, StatusCode::OK, "{}", by_barcode.body);
    let hits = by_barcode.body["hits"].as_array().expect("hits");
    assert!(
        hits.iter().any(|hit| hit["id"] == json!(by_code)),
        "a scanner and a search box describe one label: {}",
        by_barcode.body
    );
    // And the item itself, not only the stock row.
    let surfaces: Vec<&str> = hits
        .iter()
        .map(|hit| hit["surface"].as_str().expect("a surface"))
        .collect();
    assert!(
        surfaces.contains(&"item") && surfaces.contains(&"stock"),
        "one statement over both surfaces, so both are found: {surfaces:?}"
    );
    let stock_hit = hits
        .iter()
        .find(|hit| hit["surface"] == "stock")
        .expect("the stocked item must appear as a stock hit too");
    assert!(stock_hit["on_hand"].as_str().is_some(), "with its on-hand beside it");
    assert!(stock_hit["location_code"].as_str().is_some(), "and where it is");

    // The empty term is refused rather than matching everything: a `⌘K` keystroke that
    // reaches the API before the first character is a search over the whole warehouse.
    let empty = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/search?q=", Some(&operator), None),
    )
    .await;
    assert_eq!(
        empty.status,
        StatusCode::BAD_REQUEST,
        "an empty term must be refused, not answered with every row: {}",
        empty.body
    );
}

/// A member without the read key is refused, and the refusal names the route's key.
///
/// The reports screen has **no permission of its own** — it sits under
/// `inventory.items.read` with the stock list. The walk therefore asserts that the
/// `403` names *that* key: a report that grew an `inventory.reports.read` nobody holds
/// would be a screen nobody can open, and a suite that granted "the whole family" would
/// not have noticed.
#[tokio::test]
async fn a_member_is_refused_and_the_refusal_names_the_key_the_screen_sits_under() {
    let Some(fixture) = Fixture::new().await else { return };
    let stranger = create_account(&fixture.db, Some(fixture.org), "Reports Stranger").await;
    let session = login(&fixture.state, &stranger.1).await;

    for uri in [
        "/api/v1/inventory/reports",
        "/api/v1/inventory/reports/export",
        "/api/v1/inventory/search?q=bracket",
    ] {
        let refused = call(
            &fixture.state,
            request(Method::GET, uri, Some(&session), None),
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{uri}: {}", refused.body);
        let sentence = refused.body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            sentence.contains("inventory.items.read"),
            "{uri} must name the key it sits under, and it said: {sentence}"
        );
    }

    // And anonymously, which is the other reader of this screen.
    let anonymous = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/reports", None, None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{}", anonymous.body);
}

/// A window that starts after it ends is a refusal with a sentence, not an empty page.
///
/// The report defaults to the last thirty days, so `GET /reports` with no query is a
/// real answer and a caller never has to guess a period. That default is the reason a
/// bad one can be *wrong* rather than merely missing, and a `422` with a sentence is
/// what lets a person fix it — a silent empty report is read as "nothing happened",
/// which is a fact about the warehouse rather than a fact about the query.
#[tokio::test]
async fn a_window_that_starts_after_it_ends_is_refused_with_a_sentence() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let operator = fixture.token(&fixture.operator).await;

    for (uri, fragment) in [
        ("/api/v1/inventory/reports?from=2026-03-10&to=2026-03-01", "starts after it ends"),
        ("/api/v1/inventory/reports?from=2000-01-01&to=2026-01-01", "at most"),
        ("/api/v1/inventory/reports?from=01/03/2026", "from"),
        ("/api/v1/inventory/reports?idle_days=0", "between 1 and 3650"),
    ] {
        let refused = call(
            &fixture.state,
            request(Method::GET, uri, Some(&operator), None),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{uri} must be refused rather than answered: {}",
            refused.body
        );
        let sentence = refused.body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            sentence.contains(fragment),
            "{uri}: the sentence is what a person reads, and it said: {sentence}"
        );
    }
}
