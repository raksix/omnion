//! Integration tests for the stocktake (docs/requests/REQ-053, slice 4).
//!
//! Slice 1 shipped `stocktake_variance` as a reason code and the reconciliation report as a list
//! of disagreements carrying both numbers. Both were a shape with nothing behind it. This suite
//! is the proof that a count can now be run, and the shape of the proof follows the acceptance
//! criterion rather than the code: each walk would fail for the reason the criterion names.
//!
//! ## What each walk is really about
//!
//! * **`the_sheet_freezes_the_expectation_before_anybody_counts`** — the criterion's "freezes the
//!   scope" half, and the load-bearing one. The walk opens a sheet, **moves stock underneath it**,
//!   and then counts. A design that re-read the rollup at close would report the sheet's expected
//!   numbers as what they were *after* the movement, so a count would either find a variance
//!   nobody saw or invent one. The assertion is that the frozen number is still the frozen one.
//! * **`an_uncounted_line_is_not_an_empty_one_and_the_close_says_so`** — the criterion's refusal.
//!   A close that treated `null` as `0` would post an adjustment of `-expected` for every shelf
//!   nobody visited: the module would **destroy the stock it claims to have measured** and report
//!   a clean success. The walk counts one line of two, refuses the close, and asserts on the
//!   **sentence** — a bare `422` would be satisfied by a module that destroyed the shelf.
//! * **`a_close_posts_one_variance_movement_per_deviation_and_nothing_for_the_rest`** — the
//!   criterion's "one movement per deviation". The count of **ledger rows** is the assertion, not
//!   the status, because a close that posted 200 rows of `0.000` would pass a status check while
//!   burying the one row that mattered.
//! * **`the_variance_report_reopens_and_agrees_with_the_ledger`** — the criterion's last clause.
//!   The report is read twice: once at the close and once after a movement that has nothing to
//!   do with the count, because "reopens" is a claim about time and a report that was correct
//!   once proves nothing about being correct later.
//! * **`the_stock_came_back_to_what_the_counter_saw`** — the reconciliation that ties the whole
//!   module together: after a count posts its variances, `replay` must still agree with the
//!   rollup. A close that wrote the rollup directly would pass every other walk in this file and
//!   fail this one.
//! * **`in_transit_cannot_be_counted`** — the scope's own rule. A count of a shelf nobody can
//!   reach posts a variance against goods that are on a van, and the walk asserts the refusal
//!   names the reason rather than the uuid.

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
static STOCKTAKE_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An operator: everything a warehouse needs, **including the stocktake key**.
///
/// The key is listed explicitly rather than assumed: a suite that granted the whole family
/// would keep passing if `inventory.stocktake.manage` were never registered, and a permission
/// that guards nothing is a comment.
const OPERATOR_PERMISSIONS: [&str; 7] = [
    "inventory.items.read",
    "inventory.items.manage",
    "inventory.movements.record",
    "inventory.stocktake.manage",
    "inventory.locations.manage",
    "sites.read",
    "events.read",
];

/// A reader. Note what is **absent**: `inventory.stocktake.manage`. Reading a count and running
/// one are different powers, and the criterion is proved with exactly this difference.
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
            std::env::set_var("OMNION_CSRF_SECRET", "inventory-stocktake-suite-csrf-secret");
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
        let walk = STOCKTAKE_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org = create_organization_row(&db).await;
        let (owner_id, _owner) = create_account(&db, None, "Stocktake Owner").await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        let (operator_id, operator) = create_account(&db, Some(org), "Stocktake Operator").await;
        grant(&db, org, operator_id, owner_id, &OPERATOR_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(org), "Stocktake Reader").await;
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
    let slug = format!("stocktake-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind("Stocktake Test Co")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("stocktake-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("stocktake-role-{}", Uuid::new_v4().simple()),
            name: "Stocktake Test Role".to_owned(),
            description: "A role of the stocktake suite".to_owned(),
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

/// An item with thresholds that do not interfere with the count.
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

/// One line of a sheet, as the JSON the API returns it.
fn line_for<'a>(sheet: &'a Value, item_id: Uuid) -> &'a Value {
    sheet["lines"]
        .as_array()
        .expect("a sheet has lines")
        .iter()
        .find(|line| line["item_id"] == json!(item_id))
        .unwrap_or_else(|| panic!("item {item_id} must be on the sheet: {sheet}"))
}

/// How many ledger rows a stocktake posted.
async fn variance_rows(db: &Db, organization_id: Uuid, stocktake_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "select count(*) from inventory_movements \
         where organization_id = $1 and source_kind = 'stocktake' and source_id = $2",
    )
    .bind(organization_id)
    .bind(stocktake_id)
    .fetch_one(db.pool())
    .await
    .expect("the ledger must read")
}

/// The `on_hand` at one item × location, as text.
async fn on_hand(db: &Db, organization_id: Uuid, item_id: Uuid, location_id: Uuid) -> String {
    sqlx::query_scalar(
        "select on_hand::text from inventory_stock \
         where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(organization_id)
    .bind(item_id)
    .bind(location_id)
    .fetch_one(db.pool())
    .await
    .expect("the stock row must exist")
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// The sheet freezes the expectation, so stock moving underneath it changes nothing.
///
/// The criterion says "freezes the scope" and this is what that has to mean to be worth
/// anything: the walk opens a sheet over a shelf holding ten, **receives four more behind the
/// sheet's back**, and then counts. The frozen line must still say ten.
///
/// The naive design — re-read `on_hand` at close — would print fourteen, and a count would then
/// either invent a variance nobody saw or miss one that happened. This is the walk that kills
/// it, and it is why `expected_qty` lives on the line rather than in a query.
#[tokio::test]
async fn the_sheet_freezes_the_expectation_before_anybody_counts() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let item = create_item(&fixture.state, &token, item_body()).await;

    let receipt = receive(&fixture.state, &token, item, stock, "10").await;
    assert_eq!(receipt.status, StatusCode::CREATED, "the receipt must land: {}", receipt.body);

    let opened = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/stocktake",
            Some(&token),
            Some(json!({ "location_ids": [stock] })),
        ),
    )
    .await;
    assert_eq!(opened.status, StatusCode::CREATED, "the sheet must open: {}", opened.body);
    let line = line_for(&opened.body, item);
    assert_eq!(line["expected_qty"], json!("10.000"), "the sheet froze ten: {line}");

    // Somebody receives more while the count is under way. A stocktake that re-read the rollup
    // would now disagree with the number it printed.
    let late = receive(&fixture.state, &token, item, stock, "4").await;
    assert_eq!(late.status, StatusCode::CREATED, "the late receipt must land: {}", late.body);

    let reopened = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/stocktake/{}", opened.body["id"].as_str().unwrap()),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(reopened.status, StatusCode::OK, "the sheet must reopen: {}", reopened.body);
    let line = line_for(&reopened.body, item);
    assert_eq!(
        line["expected_qty"], json!("10.000"),
        "the expectation is frozen at the moment the sheet opened, and a later receipt must not \
         rewrite the sheet somebody is holding"
    );
    assert_eq!(
        on_hand(&fixture.db, fixture.org, item, stock).await, "14.000",
        "the rollup itself did move — the sheet is the historical record, not a live view"
    );
}

/// An uncounted line is not an empty one, and the close says so in a sentence.
///
/// The criterion is a refusal, so the walk is about **what the refusal says**. A `422` with the
/// bare code would be satisfied by a module that had already destroyed the shelf; the sentence
/// is what tells the person standing there that the count is incomplete rather than that
/// something went wrong.
///
/// The second assertion is the expensive one: after the refusal the stock is **untouched**,
/// because a close that refused after posting would have written the `−expected` rows it was
/// supposed to decline.
#[tokio::test]
async fn an_uncounted_line_is_not_an_empty_one_and_the_close_says_so() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let first = create_item(&fixture.state, &token, item_body()).await;
    let second = create_item(&fixture.state, &token, item_body()).await;

    for item in [first, second] {
        let receipt = receive(&fixture.state, &token, item, stock, "10").await;
        assert_eq!(receipt.status, StatusCode::CREATED, "the receipt must land: {}", receipt.body);
    }

    let opened = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/stocktake",
            Some(&token),
            Some(json!({ "location_ids": [stock] })),
        ),
    )
    .await;
    assert_eq!(opened.status, StatusCode::CREATED, "the sheet must open: {}", opened.body);
    let sheet_id = opened.body["id"].as_str().expect("an id");
    assert_eq!(opened.body["lines_pending"], json!(2), "both lines start uncounted");

    // Count the first shelf. The second is left alone.
    let counted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/stocktake/{sheet_id}/count"),
            Some(&token),
            Some(json!({
                "lines": [{
                    "line_id": line_for(&opened.body, first)["id"].as_str().unwrap(),
                    "quantity": "10",
                }],
            })),
        ),
    )
    .await;
    assert_eq!(counted.status, StatusCode::OK, "the count must be recorded: {}", counted.body);
    assert_eq!(counted.body["lines_pending"], json!(1), "one line is still uncounted");
    assert_eq!(counted.body["variances_count"], json!(0), "and nothing disagrees yet");

    // The `bystander` item is stocked at the same location, so it is on the sheet too and would
    // have to be counted — that is the scope working, not a nuisance: a sheet of everything at
    // the location is what a stocktake is.
    assert_eq!(
        counted.body["lines"].as_array().map(Vec::len),
        Some(2),
        "both items are at this location, so both are on the sheet"
    );

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/stocktake/{sheet_id}/close"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "the close must be refused by the document's state, not by a bad field: {}",
        refused.body
    );
    // The sentence, read from the error envelope the API actually returns.
    let sentence = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        sentence.contains("nobody has counted"),
        "the sentence must say the shelf was not counted, not merely that something is wrong: \
         {sentence:?}"
    );
    assert!(
        sentence.contains("1"),
        "and it must say how many: {sentence:?}"
    );

    // The expensive half: a refused close wrote nothing.
    assert_eq!(
        variance_rows(&fixture.db, fixture.org, Uuid::parse_str(sheet_id).unwrap()).await,
        0,
        "a refused close must post no ledger rows at all"
    );
    for item in [first, second] {
        assert_eq!(
            on_hand(&fixture.db, fixture.org, item, stock).await, "10.000",
            "the uncounted shelf must be untouched — treating null as zero would have destroyed it"
        );
    }
}

/// A close posts one variance movement per deviation, and nothing for the lines that agree.
///
/// The count of **ledger rows** is the assertion, not the status. A close that wrote a row for
/// every line — 200 rows of `0.000` for a 200-line count with one real deviation — would pass a
/// status check and bury the row that mattered in a ledger nobody reads.
///
/// Three lines: one short, one exact, one over. The walk requires **two** rows, and reads them
/// back to confirm the signs, because a close that posted the shortfall with the wrong sign would
/// have made the shelf worse.
#[tokio::test]
async fn a_close_posts_one_variance_movement_per_deviation_and_nothing_for_the_rest() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let short = create_item(&fixture.state, &token, item_body()).await;
    let exact = create_item(&fixture.state, &token, item_body()).await;
    let over = create_item(&fixture.state, &token, item_body()).await;

    for item in [short, exact, over] {
        let receipt = receive(&fixture.state, &token, item, stock, "10").await;
        assert_eq!(receipt.status, StatusCode::CREATED, "the receipt must land: {}", receipt.body);
    }

    let opened = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/stocktake",
            Some(&token),
            Some(json!({ "location_ids": [stock] })),
        ),
    )
    .await;
    assert_eq!(opened.status, StatusCode::CREATED, "the sheet must open: {}", opened.body);
    let sheet_id = opened.body["id"].as_str().expect("an id").to_owned();
    let sheet_uuid = Uuid::parse_str(&sheet_id).expect("an id");

    let counts: Vec<Value> = [(short, "7"), (exact, "10"), (over, "12")]
        .iter()
        .map(|(item, quantity)| {
            json!({
                "line_id": line_for(&opened.body, *item)["id"].as_str().unwrap(),
                "quantity": quantity,
            })
        })
        .collect();
    let counted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/stocktake/{sheet_id}/count"),
            Some(&token),
            Some(json!({ "lines": counts })),
        ),
    )
    .await;
    assert_eq!(counted.status, StatusCode::OK, "the count must be recorded: {}", counted.body);
    assert_eq!(counted.body["lines_pending"], json!(0), "every line is counted");
    assert_eq!(counted.body["variances_count"], json!(2), "two of the three disagree");

    // The signs, read off the sheet, before anything is posted.
    assert_eq!(
        line_for(&counted.body, short)["variance"], json!("-3.000"),
        "a shortfall is negative"
    );
    assert_eq!(line_for(&counted.body, exact)["variance"], json!("0.000"), "agreement is zero");
    assert_eq!(
        line_for(&counted.body, over)["variance"], json!("2.000"),
        "a surplus is positive, through the same subtraction"
    );

    let closed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/stocktake/{sheet_id}/close"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(closed.status, StatusCode::OK, "the close must work: {}", closed.body);
    assert_eq!(closed.body["lines"], json!(3), "three lines were on the sheet");
    assert_eq!(closed.body["variances"], json!(2), "two disagreed");
    assert_eq!(closed.body["variance_total"], json!("-1.000"), "-3 + 2 = -1");

    assert_eq!(
        variance_rows(&fixture.db, fixture.org, sheet_uuid).await,
        2,
        "**one row per deviation**, not one per line: a close that wrote 0.000 for the agreeing \
         line would bury the two that mattered"
    );

    // The stock now says what the counter saw.
    assert_eq!(on_hand(&fixture.db, fixture.org, short, stock).await, "7.000");
    assert_eq!(on_hand(&fixture.db, fixture.org, exact, stock).await, "10.000");
    assert_eq!(on_hand(&fixture.db, fixture.org, over, stock).await, "12.000");
}

/// The stock comes back to what the counter saw — `replay` still agrees with the rollup.
///
/// This is the walk that ties slice 4 to the module's one invariant. Every other walk in this
/// file would pass against a close that updated `inventory_stock` directly, because they all
/// read the rollup. This one replays the ledger, and a direct rollup write leaves no rows behind
/// it — the exact state `0126`'s reconciliation is written to catch.
#[tokio::test]
async fn the_stock_came_back_to_what_the_counter_saw_and_the_ledger_still_replays() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let item = create_item(&fixture.state, &token, item_body()).await;

    let receipt = receive(&fixture.state, &token, item, stock, "25").await;
    assert_eq!(receipt.status, StatusCode::CREATED, "the receipt must land: {}", receipt.body);

    let opened = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/stocktake",
            Some(&token),
            Some(json!({ "location_ids": [stock] })),
        ),
    )
    .await;
    assert_eq!(opened.status, StatusCode::CREATED, "the sheet must open: {}", opened.body);
    let sheet_id = opened.body["id"].as_str().expect("an id").to_owned();

    let counted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/stocktake/{sheet_id}/count"),
            Some(&token),
            Some(json!({
                "lines": [{
                    "line_id": line_for(&opened.body, item)["id"].as_str().unwrap(),
                    "quantity": "21.5",
                }],
            })),
        ),
    )
    .await;
    assert_eq!(counted.status, StatusCode::OK, "the count must be recorded: {}", counted.body);

    let closed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/stocktake/{sheet_id}/close"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(closed.status, StatusCode::OK, "the close must work: {}", closed.body);
    assert_eq!(closed.body["variance_total"], json!("-3.500"), "21.5 against 25 is -3.5");

    assert_eq!(
        on_hand(&fixture.db, fixture.org, item, stock).await, "21.500",
        "the shelf must hold what the counter saw"
    );

    // The module's own reconciliation, through the API the overview uses.
    let reconciliation = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/inventory/reconciliation",
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        reconciliation.status,
        StatusCode::OK,
        "the reconciliation must run: {}",
        reconciliation.body
    );
    let mismatches = reconciliation.body["mismatches"]
        .as_array()
        .expect("the report has a list of disagreements");
    assert!(
        mismatches.is_empty(),
        "a close that wrote the rollup directly would leave no ledger rows and the report would \
         name it here: {mismatches:?}"
    );
}

/// The variance report reopens, and it agrees with the ledger.
///
/// "Reopens correctly" is a claim about **time**: a report that was right at the close says
/// nothing about being right six months later. The walk reads the report, then writes a
/// movement that has nothing to do with the count, and reads it again — the report must be
/// unchanged, because a variance report that moves when unrelated stock moves is a live view
/// wearing a report's clothes.
///
/// The `agrees` flag is the report's own proof: the header's frozen total beside the ledger's
/// own sum, computed by different paths.
#[tokio::test]
async fn the_variance_report_reopens_and_agrees_with_the_ledger() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let elsewhere = fixture.location("RETURNS").await;
    let counted_item = create_item(&fixture.state, &token, item_body()).await;
    let bystander = create_item(&fixture.state, &token, item_body()).await;

    // The bystander lives at **another** location on purpose: the sheet is every item at the
    // counted location, so a bystander stocked at the same shelf would be on the sheet and would
    // have to be counted too. Putting it elsewhere is what makes "unrelated stock moves" true.
    let receipt = receive(&fixture.state, &token, counted_item, stock, "10").await;
    assert_eq!(receipt.status, StatusCode::CREATED, "the receipt must land: {}", receipt.body);
    let far = receive(&fixture.state, &token, bystander, elsewhere, "10").await;
    assert_eq!(far.status, StatusCode::CREATED, "the far receipt must land: {}", far.body);

    let opened = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/stocktake",
            Some(&token),
            Some(json!({ "location_ids": [stock] })),
        ),
    )
    .await;
    assert_eq!(opened.status, StatusCode::CREATED, "the sheet must open: {}", opened.body);
    let sheet_id = opened.body["id"].as_str().expect("an id").to_owned();

    let counted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/stocktake/{sheet_id}/count"),
            Some(&token),
            Some(json!({
                "lines": [{
                    "line_id": line_for(&opened.body, counted_item)["id"].as_str().unwrap(),
                    "quantity": "6",
                }],
            })),
        ),
    )
    .await;
    assert_eq!(counted.status, StatusCode::OK, "the count must be recorded: {}", counted.body);

    let closed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/stocktake/{sheet_id}/close"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(closed.status, StatusCode::OK, "the close must work: {}", closed.body);

    let report_uri = format!("/api/v1/inventory/stocktake/{sheet_id}/report");
    let first = call(&fixture.state, request(Method::GET, &report_uri, Some(&token), None)).await;
    assert_eq!(first.status, StatusCode::OK, "the report must open: {}", first.body);
    assert_eq!(first.body["variance_total"], json!("-4.000"), "ten counted as six");
    assert_eq!(first.body["ledger_total"], json!("-4.000"), "and the ledger says the same");
    assert_eq!(first.body["agrees"], json!(true), "the two must agree");
    assert_eq!(first.body["movements"].as_array().map(Vec::len), Some(1), "one row posted");

    // Unrelated stock moves — at the **other** location, so the count's sheet cannot have
    // included it even in principle. The report is a historical document and must not follow it.
    //
    // **The size is deliberate and the first attempt got it wrong.** A receipt of 500 came back
    // `202` rather than `201`, because the over-threshold rule from slice 2 governs *every*
    // movement and not only adjustments — which is correct, and not what this walk is about. A
    // walk that trips a second feature's guard is testing two things at once and fails for a
    // reason its name does not mention; 50 sits under the organization's 100 threshold.
    let later = receive(&fixture.state, &token, bystander, elsewhere, "50").await;
    assert_eq!(later.status, StatusCode::CREATED, "the later receipt must land: {}", later.body);

    let second = call(&fixture.state, request(Method::GET, &report_uri, Some(&token), None)).await;
    assert_eq!(second.status, StatusCode::OK, "the report must reopen: {}", second.body);
    assert_eq!(
        second.body["variance_total"], json!("-4.000"),
        "a report that moves when unrelated stock moves is a live view, not a record"
    );
    assert_eq!(
        second.body["movements"].as_array().map(Vec::len),
        Some(1),
        "and it must still show only the movement this count caused"
    );
    assert_eq!(second.body["agrees"], json!(true));
    // The sheet's own lines are still readable, with the frozen expectation intact.
    assert_eq!(
        line_for(&second.body["stocktake"], counted_item)["expected_qty"], json!("10.000"),
        "March's sheet must still say what the shelf held in March"
    );
}

/// In-transit cannot be counted: it is a van, not a shelf.
///
/// The refusal has to be **legible**. A uuid in the message tells the person standing in a
/// warehouse nothing about why the sheet they are about to print cannot include that location,
/// and they will go looking for the missing permission that does not exist.
#[tokio::test]
async fn in_transit_cannot_be_counted_and_the_refusal_says_why() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let transit = fixture.location("TRANSIT").await;

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/stocktake",
            Some(&token),
            Some(json!({ "location_ids": [transit] })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a count of goods on a van must be refused: {}",
        refused.body
    );
    let sentence = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        sentence.contains("in transit") || sentence.contains("transit"),
        "the sentence must name the reason, not a uuid: {sentence:?}"
    );
}

/// A reader may open a count and may not run one.
///
/// "Reading and moving are different powers" is the permission family\'s whole argument, and a
/// test that only tried the granted case would pass with the guard missing — the reader would
/// simply never be asked. The walk asserts **both** answers for the same account, which is also
/// what makes the `403` meaningful rather than an accident of role wiring.
#[tokio::test]
async fn a_reader_may_open_a_count_and_may_not_run_one() {
    let Some(fixture) = Fixture::new().await else { return; };
    let reader = fixture.token(&fixture.reader).await;
    let stock = fixture.location("STOCK").await;

    // The read keys on `inventory.items.read`, so the list and the report are reachable.
    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/stocktake", Some(&reader), None),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "a reader may open the list: {}", listed.body);

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/stocktake",
            Some(&reader),
            Some(json!({ "location_ids": [stock] })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a reader must not open a sheet: {}",
        refused.body
    );
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("inventory.stocktake.manage"),
        "and the refusal must name the key it wanted: {}",
        refused.body
    );
}
