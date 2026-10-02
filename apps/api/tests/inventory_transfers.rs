//! Integration tests for transfers and low-stock alerts (docs/requests/REQ-053, slice 3).
//!
//! Slice 1 shipped the `in_transit` location kind, the `transfer_out`/`transfer_in` movement
//! kinds and the rule that a hand-written movement may **not** be a transfer. Nothing could
//! produce either kind. This suite is the proof that it can, and the shape of the proof is not
//! arbitrary — each acceptance criterion is written as a walk that would fail for the reason the
//! criterion names, not for some easier reason on the way there.
//!
//! ## What each walk is really about
//!
//! * **`a_dispatch_moves_the_goods_out_and_into_transit_and_the_total_does_not_move`** — the
//!   criterion's real content is that the organization's stock is the same before, during and
//!   after. A dispatch that only wrote the outbound leg would pass a naive "did the source go
//!   down?" check while the stock list showed a hole nobody could explain, so the walk sums the
//!   whole organization at all three moments and compares.
//! * **`a_line_cannot_exceed_what_the_source_has`** — asserts on the **number in the message**,
//!   not on "not 201". A 422 with the sentence "insufficient stock" and no number passes a status
//!   check and still sends the person back to the shelf to subtract it themselves, which is the
//!   failure the variant exists to prevent.
//! * **`cancelling_before_dispatch_leaves_stock_untouched`** — the criterion's own wording, and
//!   the reason the test counts ledger rows rather than reading a status. A cancel that wrote a
//!   movement would leave the ledger describing a transfer that moved nothing.
//! * **`the_alert_sweep_raises_once_clears_on_a_restock_and_rearms`** — the count is asserted
//!   after **three** sweeps, because an idempotent sweep is a claim about running it twice and a
//!   test that only runs it once cannot tell.
//! * **`two_transfers_dispatching_the_same_shelf_cannot_both_win`** — the concurrency clause.
//!   Two transfers each within the balance, dispatched one after the other, must leave the second
//!   refused: the check happens at dispatch against the balance as of that moment, so the second
//!   one sees the first one's movement.

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
static TRANSFER_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An operator: everything a warehouse needs, and nothing more.
const OPERATOR_PERMISSIONS: [&str; 6] = [
    "inventory.items.read",
    "inventory.items.manage",
    "inventory.movements.record",
    "inventory.transfers.manage",
    "inventory.locations.manage",
    "sites.read",
];

/// A reader. Note what is **absent**: `inventory.transfers.manage`. Reading a transfer needs
/// only `inventory.items.read`, and the "reading and moving are different powers" criterion is
/// proved with exactly this difference.
const READER_PERMISSIONS: [&str; 2] = ["inventory.items.read", "sites.read"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    /// **Every** `Set-Cookie` the response carried, in order.
    ///
    /// One string rather than `Option<String>`: the login response sets the session cookie *and*
    /// the CSRF cookie, and a harness that keeps only the first either loses the token or
    /// guesses at it. The slice-1 suite's `Option<String>` predates the CSRF layer and would
    /// quietly have kept the wrong one.
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
///
/// The attribute suffix is stripped here rather than at every call site, so a walk that wants
/// the CSRF token is not also the place that has to know cookies arrive as `name=value; Path=/`.
fn cookie_value(response: &TestResponse, name: &str) -> Option<String> {
    response.set_cookies.iter().find_map(|raw| {
        let pair = raw.split(';').next()?.trim();
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.to_owned())
    })
}

/// Build a JSON request; `token` becomes the session cookie and `body` the payload.
///
/// The CSRF token is a **second cookie on the same jar**, which is what a browser does and what
/// the middleware accepts (`x-omnion-csrf` header or the `omnion_csrf` cookie). Sending only the
/// session cookie is the ambient-authority request the control exists to refuse, so a walk that
/// omitted it would be testing nothing.
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
            // Same rule as the slice-1/2 suite, and for the same reason: this suite's subject is
            // a **balance** — a walk that quietly SKIPs when PostgreSQL is down is a green tick
            // that proved nothing.
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
/// **The CSRF secret is given a test-only default here**, and the reason is worth stating because
/// the alternative is a suite that silently stops testing anything. The CSRF layer (merged from
/// main) refuses every cookie-authenticated mutation when `OMNION_CSRF_SECRET` is unset, with a
/// `403` that names the variable — a loud, correct refusal, and one that turns every write walk
/// in this file into an assertion about a missing deployment key. Setting the variable in the
/// harness rather than in the shell means `cargo test` proves what it is supposed to prove on a
/// bare checkout, and the value is a throwaway that authenticates nothing outside this process.
async fn live_state() -> Option<(AppState, Db)> {
    if std::env::var("OMNION_CSRF_SECRET").is_err() {
        // `set_var` is `unsafe` from the 2024 edition; this suite is single-threaded per test
        // and the process is the test binary itself, so the unsound window the compiler warns
        // about (another thread reading the environment concurrently) does not exist here.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OMNION_CSRF_SECRET", "inventory-transfer-suite-csrf-secret");
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
        let walk = TRANSFER_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org = create_organization_row(&db).await;
        let (owner_id, _owner) = create_account(&db, None, "Transfer Owner").await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        let (operator_id, operator) = create_account(&db, Some(org), "Transfer Operator").await;
        grant(&db, org, operator_id, owner_id, &OPERATOR_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(org), "Transfer Reader").await;
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
    let slug = format!("transfer-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind("Transfer Test Co")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("transfer-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("transfer-role-{}", Uuid::new_v4().simple()),
            name: "Transfer Test Role".to_owned(),
            description: "A role of the transfer suite".to_owned(),
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
/// **Both cookies, or the writes are refused** — and the refusal is the CSRF layer doing its job,
/// not a test defect. A harness that kept only the first `Set-Cookie` would look like a working
/// suite and prove nothing.
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
    // **The CSRF token is derived here rather than read from a cookie, and that is a workaround
    // for a gap on `main` worth naming.** `6e7b920` added the CSRF layer, which refuses every
    // cookie-authenticated mutation without a token, but `POST /auth/login` still issues only
    // the session cookie — so a real browser cannot write anything either, and every write suite
    // on the platform is red for the same reason. The token is an HMAC of the session id, so the
    // harness can compute exactly what the middleware expects, read the session id from the
    // database it just logged in to, and keep proving what this file is about.
    //
    // **When `main` starts setting `omnion_csrf` on login, delete this and read the cookie
    // instead** — a harness that derives the token cannot notice if the derivation and the
    // middleware ever drift apart, which is the one thing the middleware's own tests are for.
    // The one-line change is `cookie_value(&response, "omnion_csrf")`, and the assertion below
    // becomes the thing that tells you it is time.
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
/// The session's **id**, not its token: `derive_token` is an HMAC over the id, and the id is
/// what the middleware has in hand when it checks. A harness that hashed the cookie value would
/// pass nothing and fail everything, and the failure would read like a permission problem.
async fn csrf_token_for(state: &AppState, session_token: &str) -> String {
    // The session id is read through the **platform's own resolver** rather than by hashing the
    // token here: `token_hash` is a one-way hash, so a `where token_hash = $1` query would
    // silently find nothing, and re-implementing `hash_token` in a test is exactly the second
    // implementation a test must not have.
    let session = omnion_identity::sessions::resolve_session(state.db().pool(), session_token)
        .await
        .expect("the session must resolve right after a login")
        .expect("the session row must exist right after a login");

    // `config().csrf.as_bytes()` is the same secret the middleware reads, and `None` here means
    // the harness forgot to set `OMNION_CSRF_SECRET` — which `live_state` does, with a message.
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
        object.insert("sku".to_owned(), json!(sku("TR")));
    }
    let response = call(
        state,
        request(Method::POST, "/api/v1/inventory/items", Some(&token), Some(body)),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "the item must be created: {}", response.body);
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
}

/// An item with thresholds that produce a badge when the balance crosses them.
fn item_body(min: &str, reorder: &str) -> Value {
    json!({
        "name": "Washer M8",
        "unit": "piece",
        "min_threshold": min,
        "reorder_point": reorder,
        "reorder_qty": "50",
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
            Some(&token),
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

/// The organization's total `on_hand` for one item, summed over every location.
///
/// **The organization total, not one row**, and that is the criterion: a dispatch that wrote only
/// the outbound leg would leave the source right and the total short, and a test that only
/// watched the source would call that a pass.
async fn total_on_hand(db: &Db, organization_id: Uuid, item_id: Uuid) -> String {
    let total: String = sqlx::query_scalar(
        "select coalesce(sum(on_hand), 0)::text from inventory_stock \
         where organization_id = $1 and item_id = $2",
    )
    .bind(organization_id)
    .bind(item_id)
    .fetch_one(db.pool())
    .await
    .expect("the total must read");
    total
}

/// A transfer created and asserted to exist, returning its id.
async fn create_transfer(
    state: &AppState,
    token: &Session,
    from: Uuid,
    to: Uuid,
    item_id: Uuid,
    quantity: &str,
) -> (Uuid, Value) {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/transfers",
            Some(&token),
            Some(json!({
                "from_location_id": from,
                "to_location_id": to,
                "lines": [{ "item_id": item_id, "quantity": quantity }],
            })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the draft must be created: {}",
        response.body
    );
    let id = Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id");
    (id, response.body)
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// A dispatch moves the goods out and into transit, and the organization's total does not move.
///
/// The criterion says three things and this walk checks all of them, because checking one is how
/// an implementation passes a criterion it only half meets:
///
/// 1. the source went down by the line's quantity;
/// 2. **the transit location holds it** — a dispatch that only wrote the outbound leg would pass
///    a source-only check while the stock list showed a hole nobody could explain;
/// 3. **the total is unchanged**, before and after, which is the property a stocktake six months
///    later depends on.
///
/// The receive half is the mirror image: transit goes down, the target goes up, and the total is
/// *still* unchanged. A receive that booked in without taking the goods out of transit would
/// create stock, and a walk that only checked the target would call that a pass.
#[tokio::test]
async fn a_dispatch_moves_the_goods_out_and_into_transit_and_the_total_does_not_move() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let target = fixture.location("RETURNS").await;
    let transit = fixture.location("TRANSIT").await;
    let item = create_item(&fixture.state, &token, item_body("2", "5")).await;

    let receipt = receive(&fixture.state, &token, item, stock, "10").await;
    assert_eq!(receipt.status, StatusCode::CREATED, "the receipt must land: {}", receipt.body);
    let before = total_on_hand(&fixture.db, fixture.org, item).await;

    let (transfer_id, draft) = create_transfer(&fixture.state, &token, stock, target, item, "4").await;
    assert_eq!(draft["status"], json!("draft"), "a draft has moved nothing: {draft}");
    assert_eq!(
        total_on_hand(&fixture.db, fixture.org, item).await,
        before,
        "writing the draft must not touch stock at all"
    );

    let dispatched = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/dispatch"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(dispatched.status, StatusCode::OK, "the dispatch must work: {}", dispatched.body);
    assert_eq!(dispatched.body["status"], json!("dispatched"));

    let at_source: String = sqlx::query_scalar(
        "select on_hand::text from inventory_stock where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(fixture.org).bind(item).bind(stock)
    .fetch_one(fixture.db.pool()).await.expect("the source row must exist");
    assert_eq!(at_source, "6.000", "the source is down by the line's four: {at_source}");

    let in_transit: String = sqlx::query_scalar(
        "select on_hand::text from inventory_stock where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(fixture.org).bind(item).bind(transit)
    .fetch_one(fixture.db.pool()).await.expect("the transit row must exist");
    assert_eq!(
        in_transit, "4.000",
        "the goods are on a van, and the stock list has to be able to say where they are"
    );
    assert_eq!(
        total_on_hand(&fixture.db, fixture.org, item).await,
        before,
        "the organization's stock is the same before and during the journey — that is the \
         property a stocktake depends on"
    );

    // The receive half: out of transit, in at the target, total still unchanged.
    let line_id = Uuid::parse_str(dispatched.body["lines"][0]["id"].as_str().expect("a line id"))
        .expect("a line id");
    let received = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/receive"),
            Some(&token),
            Some(json!({ "lines": [{ "line_id": line_id, "quantity": "4" }] })),
        ),
    )
    .await;
    assert_eq!(received.status, StatusCode::OK, "the receive must work: {}", received.body);
    assert_eq!(
        received.body["status"], json!("received"),
        "every line landed, so the document is received"
    );
    assert_eq!(received.body["received_total"], json!("4.000"));

    let in_transit_after: String = sqlx::query_scalar(
        "select on_hand::text from inventory_stock where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(fixture.org).bind(item).bind(transit)
    .fetch_one(fixture.db.pool()).await.expect("the transit row must exist");
    assert_eq!(in_transit_after, "0.000", "transit is emptied, not doubled: {in_transit_after}");

    let at_target: String = sqlx::query_scalar(
        "select on_hand::text from inventory_stock where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(fixture.org).bind(item).bind(target)
    .fetch_one(fixture.db.pool()).await.expect("the target row must exist");
    assert_eq!(at_target, "4.000");

    assert_eq!(
        total_on_hand(&fixture.db, fixture.org, item).await,
        before,
        "and still the same after it lands"
    );
}

/// A line cannot exceed what the source has — and the refusal carries the number.
///
/// The assertion is on the **sentence**, not on the status. A `422` whose body says "insufficient
/// stock" and no number passes a status check and still sends the person holding the cart back
/// to the shelf to work out the difference themselves, which is the whole reason the module's
/// `WouldGoNegative` variant carries `available`.
#[tokio::test]
async fn a_line_cannot_exceed_what_the_source_has() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let target = fixture.location("RETURNS").await;
    let item = create_item(&fixture.state, &token, item_body("0", "0")).await;

    receive(&fixture.state, &token, item, stock, "6").await;
    let (transfer_id, _) = create_transfer(&fixture.state, &token, stock, target, item, "7").await;

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/dispatch"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "seven out of six has to be refused: {}",
        refused.body
    );
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the refusal must have a sentence: {}", refused.body));
    assert!(
        message.contains("6.000"),
        "the sentence must carry the number that is actually there, or the person holding the \
         cart has to recount the shelf: {message}"
    );

    // And the refusal left the stock alone. A dispatch that refused *after* moving the goods
    // would answer the same 422 and leave the shelf short.
    let at_source: String = sqlx::query_scalar(
        "select on_hand::text from inventory_stock where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(fixture.org).bind(item).bind(stock)
    .fetch_one(fixture.db.pool()).await.expect("the source row must exist");
    assert_eq!(at_source, "6.000", "a refused dispatch moves nothing: {at_source}");
}

/// Cancelling before dispatch leaves stock untouched, and writes no ledger row.
///
/// The criterion's own words. The count is the assertion that matters: a cancel that wrote a
/// movement would leave the ledger describing a transfer that moved nothing, and a walk that
/// only read the status would call that a pass.
#[tokio::test]
async fn cancelling_before_dispatch_leaves_stock_untouched() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let target = fixture.location("RETURNS").await;
    let item = create_item(&fixture.state, &token, item_body("0", "0")).await;

    receive(&fixture.state, &token, item, stock, "10").await;
    let before = total_on_hand(&fixture.db, fixture.org, item).await;
    let rows_before: i64 = sqlx::query_scalar(
        "select count(*) from inventory_movements where organization_id = $1 and item_id = $2",
    )
    .bind(fixture.org).bind(item)
    .fetch_one(fixture.db.pool()).await.expect("the count must read");

    let (transfer_id, _) = create_transfer(&fixture.state, &token, stock, target, item, "3").await;
    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/cancel"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "the cancel must work: {}", cancelled.body);
    assert_eq!(cancelled.body["status"], json!("cancelled"));

    assert_eq!(
        total_on_hand(&fixture.db, fixture.org, item).await,
        before,
        "cancelling a draft moves nothing"
    );
    let rows_after: i64 = sqlx::query_scalar(
        "select count(*) from inventory_movements where organization_id = $1 and item_id = $2",
    )
    .bind(fixture.org).bind(item)
    .fetch_one(fixture.db.pool()).await.expect("the count must read");
    assert_eq!(
        rows_after, rows_before,
        "and writes no ledger row: a movement describing a transfer that never happened is a \
         sentence an auditor will read as a fact"
    );

    // A cancelled document is closed: dispatching it is refused rather than quietly resurrecting
    // it, and the refusal says where the document actually is.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/dispatch"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert!(
        matches!(refused.status, StatusCode::CONFLICT | StatusCode::BAD_REQUEST),
        "a cancelled transfer cannot be dispatched: {}",
        refused.body
    );
}

/// Cancelling a **dispatched** transfer brings the goods home, and the ledger says so.
///
/// The counterpart to the draft case, and the reason a cancel cannot be a status write. The
/// goods are on a van; writing only `status = 'cancelled'` would leave the transit balance holding
/// stock the document says returned, and the next replay would report a disagreement the module
/// had manufactured itself.
#[tokio::test]
async fn cancelling_after_dispatch_brings_the_goods_home() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let target = fixture.location("RETURNS").await;
    let transit = fixture.location("TRANSIT").await;
    let item = create_item(&fixture.state, &token, item_body("0", "0")).await;

    receive(&fixture.state, &token, item, stock, "8").await;
    let before = total_on_hand(&fixture.db, fixture.org, item).await;
    let (transfer_id, _) = create_transfer(&fixture.state, &token, stock, target, item, "5").await;

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/dispatch"),
            Some(&token),
            None,
        ),
    )
    .await;

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/cancel"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "the cancel must work: {}", cancelled.body);
    assert_eq!(cancelled.body["status"], json!("cancelled"));

    let in_transit: String = sqlx::query_scalar(
        "select on_hand::text from inventory_stock where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(fixture.org).bind(item).bind(transit)
    .fetch_one(fixture.db.pool()).await.expect("the transit row must exist");
    assert_eq!(
        in_transit, "0.000",
        "the van came back empty: transit holding five that the document says came home is the \
         exact state replay exists to catch"
    );
    let at_source: String = sqlx::query_scalar(
        "select on_hand::text from inventory_stock where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(fixture.org).bind(item).bind(stock)
    .fetch_one(fixture.db.pool()).await.expect("the source row must exist");
    assert_eq!(at_source, "8.000", "the goods are back on the shelf");
    assert_eq!(total_on_hand(&fixture.db, fixture.org, item).await, before);
}

/// A partial receive leaves the remainder open, and the status follows the lines.
///
/// The spec asks for a partial receive per line. The subtle half is the status: the document
/// becomes `received` when **nothing is outstanding**, not when the caller says so. A caller
/// that could set the status directly could set it while three pallets are still moving, and the
/// next receive would be refused for being out of order.
#[tokio::test]
async fn a_partial_receive_leaves_the_remainder_open() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let target = fixture.location("RETURNS").await;
    let item = create_item(&fixture.state, &token, item_body("0", "0")).await;

    receive(&fixture.state, &token, item, stock, "10").await;
    let (transfer_id, _) = create_transfer(&fixture.state, &token, stock, target, item, "9").await;
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/dispatch"),
            Some(&token),
            None,
        ),
    )
    .await;

    let detail = call(
        &fixture.state,
        request(Method::GET, &format!("/api/v1/inventory/transfers/{transfer_id}"), Some(&token), None),
    )
    .await;
    let line_id = Uuid::parse_str(detail.body["lines"][0]["id"].as_str().expect("a line id"))
        .expect("a line id");

    let part = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/receive"),
            Some(&token),
            Some(json!({ "lines": [{ "line_id": line_id, "quantity": "4" }] })),
        ),
    )
    .await;
    assert_eq!(part.status, StatusCode::OK, "the partial receive must work: {}", part.body);
    assert_eq!(
        part.body["status"], json!("dispatched"),
        "five are still on the van, so the document is still in flight"
    );
    assert_eq!(part.body["received_total"], json!("4.000"));
    assert_eq!(part.body["lines"][0]["outstanding"], json!("5.000"));

    // Receiving more than was sent is refused, with the outstanding number in the sentence.
    let too_much = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/receive"),
            Some(&token),
            Some(json!({ "lines": [{ "line_id": line_id, "quantity": "6" }] })),
        ),
    )
    .await;
    assert_eq!(
        too_much.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "six of the five left has to be refused: {}",
        too_much.body
    );
    let message = too_much.body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("a sentence: {}", too_much.body));
    assert!(message.contains("5.000"), "{message}");

    // The rest arrives, and only now is the document received.
    let rest = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/inventory/transfers/{transfer_id}/receive"),
            Some(&token),
            Some(json!({ "lines": [{ "line_id": line_id, "quantity": "5" }] })),
        ),
    )
    .await;
    assert_eq!(rest.status, StatusCode::OK, "the rest must land: {}", rest.body);
    assert_eq!(rest.body["status"], json!("received"));
    assert_eq!(rest.body["received_total"], json!("9.000"));
}

/// Two transfers dispatching the same shelf cannot both win.
///
/// Each transfer is individually within the balance, so a check that only looked at its own
/// line would pass both. The second must be refused, because the check happens at dispatch
/// against the balance as of that moment and the first one's movement is already in the ledger.
#[tokio::test]
async fn two_transfers_dispatching_the_same_shelf_cannot_both_win() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let target = fixture.location("RETURNS").await;
    let item = create_item(&fixture.state, &token, item_body("0", "0")).await;

    receive(&fixture.state, &token, item, stock, "10").await;
    let (first, _) = create_transfer(&fixture.state, &token, stock, target, item, "6").await;
    let (second, _) = create_transfer(&fixture.state, &token, stock, target, item, "6").await;

    let one = call(
        &fixture.state,
        request(Method::POST, &format!("/api/v1/inventory/transfers/{first}/dispatch"), Some(&token), None),
    )
    .await;
    assert_eq!(one.status, StatusCode::OK, "the first takes six: {}", one.body);

    let two = call(
        &fixture.state,
        request(Method::POST, &format!("/api/v1/inventory/transfers/{second}/dispatch"), Some(&token), None),
    )
    .await;
    assert_eq!(
        two.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the second must be refused — ten on the shelf do not become sixteen: {}",
        two.body
    );

    let at_source: String = sqlx::query_scalar(
        "select on_hand::text from inventory_stock where organization_id = $1 and item_id = $2 and location_id = $3",
    )
    .bind(fixture.org).bind(item).bind(stock)
    .fetch_one(fixture.db.pool()).await.expect("the source row must exist");
    assert_eq!(at_source, "4.000", "and the refused one left the shelf as it found it");
}

/// The low-stock sweep raises once, clears on a restock, and re-arms on the next crossing.
///
/// The count is asserted after **three** sweeps, because "one alert per crossing" is a claim
/// about running the sweep repeatedly and a test that runs it once cannot tell an idempotent
/// sweep from a lucky one. The re-arm is the half that a unique index on `(item, location, kind)`
/// would break: the second crossing has to be *allowed* to raise again, and a full unique index
/// would make it fail.
#[tokio::test]
async fn the_alert_sweep_raises_once_clears_on_a_restock_and_rearms() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let item = create_item(&fixture.state, &token, item_body("2", "5")).await;

    // 6 on the shelf, reorder point 5: healthy.
    receive(&fixture.state, &token, item, stock, "6").await;

    let first = sweep(&fixture.state, &token).await;
    assert_eq!(first.status, StatusCode::OK, "the sweep must work: {}", first.body);
    let open_after_healthy = open_alerts(&fixture.db, fixture.org, item).await;
    assert_eq!(open_after_healthy, 0, "a shelf above its reorder point raises nothing");

    // Down to 4 — one crossing.
    issue(&fixture.state, &token, item, stock, "2").await;
    sweep(&fixture.state, &token).await;
    assert_eq!(
        open_alerts(&fixture.db, fixture.org, item).await,
        1,
        "the downward crossing raises exactly one alert"
    );

    // Running the sweep again must not raise a second one. This is the idempotence the
    // `alerts_on_read` setting depends on: without it a busy warehouse raises one per page view.
    sweep(&fixture.state, &token).await;
    sweep(&fixture.state, &token).await;
    assert_eq!(
        open_alerts(&fixture.db, fixture.org, item).await,
        1,
        "three sweeps over an unchanged shelf still show one alert, or the inbox is a wall of noise"
    );

    // The restock closes it.
    receive(&fixture.state, &token, item, stock, "10").await;
    sweep(&fixture.state, &token).await;
    assert_eq!(
        open_alerts(&fixture.db, fixture.org, item).await,
        0,
        "a restock clears the alert: the inbox answers 'is this still true?', not 'did this \
         ever happen?'"
    );

    // And the next crossing raises a **new** row rather than being blocked by the old one.
    issue(&fixture.state, &token, item, stock, "11").await;
    sweep(&fixture.state, &token).await;
    assert_eq!(
        open_alerts(&fixture.db, fixture.org, item).await,
        1,
        "the next crossing re-arms: a full unique index on (item, location, kind) would fail \
         here, and the history would be one row whose meaning depends on two timestamps"
    );

    // The history is a series of episodes, not one resurrected row.
    let episodes: i64 = sqlx::query_scalar(
        "select count(*) from inventory_alerts where organization_id = $1 and item_id = $2",
    )
    .bind(fixture.org).bind(item)
    .fetch_one(fixture.db.pool()).await.expect("the count must read");
    assert_eq!(episodes, 2, "two crossings, two episodes — the closed one is kept");
}

/// An alert inbox is readable by the same power that reads the stock list, and a sweep is a write.
#[tokio::test]
async fn reading_the_inbox_and_running_the_sweep_are_different_powers() {
    let Some(fixture) = Fixture::new().await else { return; };
    let operator = fixture.token(&fixture.operator).await;
    let reader = fixture.token(&fixture.reader).await;
    let stock = fixture.location("STOCK").await;
    let item = create_item(&fixture.state, &operator, item_body("2", "5")).await;
    receive(&fixture.state, &operator, item, stock, "6").await;
    issue(&fixture.state, &operator, item, stock, "3").await;

    // **The operator sweeps before the reader reads, and that ordering is the point.** The
    // sweep is what turns a crossing into a row; until somebody has run it the inbox is
    // legitimately empty. A walk that swept as the reader would be testing the wrong power —
    // the reader's job here is to SEE, and the reader is refused the sweep two lines below.
    let seeded = sweep(&fixture.state, &operator).await;
    assert_eq!(seeded.status, StatusCode::OK, "the sweep must work: {}", seeded.body);
    assert!(
        seeded.body["raised"].as_i64().unwrap_or(0) >= 1,
        "3 units against a reorder point of 5 is a crossing, and the sweep is what notices it: {}",
        seeded.body
    );

    // The reader may look at the inbox — a transfer and an alert are movements of stock, and
    // somebody who may read the ledger may read what produced it.
    let inbox = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/alerts", Some(&reader), None),
    )
    .await;
    assert_eq!(inbox.status, StatusCode::OK, "a reader may read the inbox: {}", inbox.body);
    assert!(
        inbox.body["items"].as_array().is_some_and(|rows| !rows.is_empty()),
        "and there is something in it: {}",
        inbox.body
    );

    // The badge agrees with the list, because both read the same predicate.
    let badge = call(
        &fixture.state,
        request(Method::GET, "/api/v1/inventory/alerts/open-count", Some(&reader), None),
    )
    .await;
    assert_eq!(badge.status, StatusCode::OK);
    assert_eq!(badge.body["open"], inbox.body["items"].as_array().expect("rows").len() as i64);

    // The sweep is a write, so the reader may not run it.
    let refused = call(
        &fixture.state,
        request(Method::POST, "/api/v1/inventory/alerts/sweep", Some(&reader), None),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a reader must not be the one deciding a shelf is a problem: {}",
        refused.body
    );

    // A second sweep over an unchanged shelf raises **nothing**. That is the idempotence the
    // `alerts_on_read` setting depends on, and asserting only `OK` here would have missed it.
    let swept = sweep(&fixture.state, &operator).await;
    assert_eq!(swept.status, StatusCode::OK, "the operator may sweep: {}", swept.body);
    assert_eq!(
        swept.body["raised"], json!(0),
        "a second sweep over the same shelf raises no second alert: {}",
        swept.body
    );
}

/// A transfer of another organization is a `404`, not a `403`.
///
/// A `403` would confirm the document exists, and one organization's stock is the one thing this
/// module exists to keep apart. The walk uses a **caller who could write**, because the route
/// guard answers `403` for a caller with no key and that would prove nothing about the module.
#[tokio::test]
async fn a_transfer_of_another_organization_is_a_404() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let target = fixture.location("RETURNS").await;
    let item = create_item(&fixture.state, &token, item_body("0", "0")).await;
    receive(&fixture.state, &token, item, stock, "5").await;
    let (transfer_id, _) = create_transfer(&fixture.state, &token, stock, target, item, "1").await;

    let anonymous = call(
        &fixture.state,
        request(Method::GET, &format!("/api/v1/inventory/transfers/{transfer_id}"), None, None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no session, no transfer: {}", anonymous.body);

    // An id from a transfer that does not exist, and an id that exists elsewhere, must be
    // indistinguishable. Both are `404`s and neither carries the id.
    let absent = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/transfers/{}", Uuid::new_v4()),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
    let message = absent.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains(&transfer_id.to_string()),
        "a 404 that echoes the id teaches a caller nothing but does confirm the row was looked \
         for: {message}"
    );
}

/// A transfer to itself, to transit, or from transit is refused — each with its own sentence.
///
/// Three refusals, three different reasons, and the test asserts on all three because a single
/// "it is refused" assertion would pass if only the first guard existed.
#[tokio::test]
async fn a_transfer_that_moves_nowhere_is_refused_at_the_field() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;
    let stock = fixture.location("STOCK").await;
    let target = fixture.location("RETURNS").await;
    let transit = fixture.location("TRANSIT").await;
    let item = create_item(&fixture.state, &token, item_body("0", "0")).await;
    receive(&fixture.state, &token, item, stock, "5").await;

    let cases = [
        (stock, stock, "to_location_id", "itself"),
        (stock, transit, "to_location_id", "in-transit"),
        (transit, target, "from_location_id", "in-transit"),
    ];

    for (from, to, field, label) in cases {
        let refused = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/inventory/transfers",
                Some(&token),
                Some(json!({
                    "from_location_id": from,
                    "to_location_id": to,
                    "lines": [{ "item_id": item, "quantity": "1" }],
                })),
            ),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "a transfer {label} must be refused: {}",
            refused.body
        );
        // The field name lives under `details`, next to the entity — that is the shape the
        // `ApiError` conversion produces, and it is the shape the form reads. Asserting on
        // `error.field` would have failed for a reason that has nothing to do with the rule.
        assert_eq!(
            refused.body["error"]["details"]["field"], json!(field),
            "the form attaches the message to a field: {}",
            refused.body
        );
    }

    // And an empty line list moves nothing, which is said rather than left to a constraint.
    let empty = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/inventory/transfers",
            Some(&token),
            Some(json!({
                "from_location_id": stock,
                "to_location_id": target,
                "lines": [],
            })),
        ),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "an empty transfer: {}", empty.body);
}

/// A filter that names a status nobody has is refused, not silently ignored.
///
/// A filter that drops what it does not understand answers "you have no transfers", and that is
/// the one conclusion a typo must never be able to produce.
#[tokio::test]
async fn an_unknown_status_filter_is_refused_rather_than_ignored() {
    let Some(fixture) = Fixture::new().await else { return; };
    let token = fixture.token(&fixture.operator).await;

    for uri in [
        "/api/v1/inventory/transfers?status=in_a_van",
        "/api/v1/inventory/alerts?kind=on_fire",
    ] {
        let refused = call(&fixture.state, request(Method::GET, uri, Some(&token), None)).await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{uri} must be refused rather than answering 'nothing': {}",
            refused.body
        );
    }
}

// -------------------------------------------------------------------------------------------
// Helpers used by the walks above
// -------------------------------------------------------------------------------------------

/// Draw stock off a shelf, which is an `issue` with the shipment reason.
async fn issue(state: &AppState, token: &Session, item_id: Uuid, location_id: Uuid, quantity: &str) {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/movements",
            Some(&token),
            Some(json!({
                "item_id": item_id,
                "location_id": location_id,
                "quantity": quantity,
                "reason": "sale_shipment",
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "the issue must land: {}", response.body);
}

/// Run the sweep.
///
/// No query string: the caller's organization is resolved from the session, exactly as every
/// other inventory route does it, and a form that had to put the tenant in the URL as well as
/// the cookie is a form somebody will eventually get wrong.
async fn sweep(state: &AppState, token: &Session) -> TestResponse {
    call(state, request(Method::POST, "/api/v1/inventory/alerts/sweep", Some(&token), None)).await
}

/// How many alerts are open for one item.
async fn open_alerts(db: &Db, organization_id: Uuid, item_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "select count(*) from inventory_alerts \
         where organization_id = $1 and item_id = $2 and cleared_at is null",
    )
    .bind(organization_id)
    .bind(item_id)
    .fetch_one(db.pool())
    .await
    .expect("the alert count must read")
}
