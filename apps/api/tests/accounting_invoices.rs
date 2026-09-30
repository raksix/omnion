//! Integration tests for invoices (docs/requests/REQ-054, slice 2).
//!
//! Written around **what a bookkeeper would try to do**, not around the endpoints:
//!
//! * an invoice's stored totals are the ones its lines add up to, **including the tax** — a line
//!   of 100.00 at 20% is 120.00 on the document, and a header that says 100.00 is a document the
//!   customer disputes;
//! * the server recomputes them. A request that posts its own `subtotal` is not a way to make a
//!   free invoice, and the REQ's own "server recomputes all totals; the client copy is display
//!   only" is the rule the test enforces from the other side;
//! * a sales order becomes a draft with its lines copied, and a **second** conversion is refused
//!   — two invoices for one order is the mistake the whole handoff exists to prevent;
//! * sending stamps `sent_at` and moves the status, and a second send is refused with the way out
//!   named;
//! * voiding keeps the number, requires a reason, and takes the invoice out of the receivables
//!   while leaving it in the list;
//! * **the sweep flips once.** Run twice and the second call changes nothing and announces
//!   nothing — the property the documented automation depends on, since it fires an e-mail;
//! * every route answers 401 without a session, 403 without the permission, and **404, never
//!   403** for another organization's invoice.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::state::AppState;
use omnion_api::{routes, BuildInfo, Db, RedisClient};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support {
    //! The sign-in half, shared. See `support/walk_auth.rs` for why a hand-rolled `login()` is
    //! the exact defect this closes: it reads the FIRST `Set-Cookie` and silently discards the
    //! CSRF token beside it, and then every write answers `csrf_unavailable`.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static INVOICE_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The reader: may **see** invoices, and nothing else. A person who may look at what is owed may
/// not write a document that says it is owed.
const READER_PERMISSIONS: [&str; 4] = [
    "accounting.invoices.read",
    "crm.contacts.read",
    "sites.read",
    "sales.orders.read",
];

/// The bookkeeper: the reader plus both write keys.
const BOOKKEEPER_PERMISSIONS: [&str; 6] = [
    "accounting.invoices.read",
    "accounting.invoices.create",
    "accounting.invoices.send",
    "crm.contacts.read",
    "sites.read",
    "sales.orders.read",
];

/// A writer in a second organization holding the full set, for the cross-tenant `404`.
const FOREIGN_PERMISSIONS: [&str; 4] = [
    "accounting.invoices.read",
    "accounting.invoices.create",
    "accounting.invoices.send",
    "sites.read",
];

// ---------------------------------------------------------------------------------------------
// Harness (the shape `accounting.rs` uses)
// ---------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    set_cookie: Vec<String>,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let set_cookie: Vec<String> = response
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
    // **A body that is not JSON is a string, never a panic.** A list assertion that cannot print
    // what the server actually said is the reason a `syntax error at or near "$2"` took three
    // runs to name itself in the slice-1 suite.
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
    session: Option<&Session>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(session) = session {
        builder = session.apply(builder);
    }
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

async fn live_state() -> Option<(AppState, Db)> {
    let mut config = omnion_core::config::Config::from_env().expect("environment must be valid");
    walk_auth::with_csrf_secret(&mut config);
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
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
    bookkeeper: String,
    reader: String,
    foreign: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = INVOICE_WALK.lock().await;
        let (state, db) = live_state().await?;
        // The limiter is a process-wide cell and `sign_in` ships at ten requests per five
        // minutes. This suite signs in three accounts per walk, so without a larger budget the
        // eleventh sign-in is refused and every walk after it dies on a line that has nothing to
        // do with invoices.
        walk_auth::give_the_process_its_own_sign_in_budget(|| {
            use omnion_api::rate_limit_middleware::RateLimiter;
            use omnion_security::RatePolicy;
            let policies: Vec<RatePolicy> = RatePolicy::defaults()
                .into_iter()
                .map(|mut policy| {
                    if policy.scope == "sign_in" {
                        policy.limit = 10_000;
                    }
                    policy
                })
                .collect();
            omnion_api::rate_limit_middleware::install(RateLimiter::new(&state, policies));
        });
        omnion_permissions::seed::ensure(db.pool()).await.ok()?;

        let organization = create_organization_row(&db, "invoices").await;
        let other_org = create_organization_row(&db, "invoices-foreign").await;

        let (owner_id, _) = create_account(&db, None, "Invoice Owner").await;
        omnion_permissions::seed::bind_owner(db.pool(), owner_id)
            .await
            .ok()?;

        let (reader_id, reader) = create_account(&db, Some(organization), "Invoice Reader").await;
        grant(&db, organization, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (bookkeeper_id, bookkeeper) =
            create_account(&db, Some(organization), "Invoice Bookkeeper").await;
        grant(
            &db,
            organization,
            bookkeeper_id,
            owner_id,
            &BOOKKEEPER_PERMISSIONS,
        )
        .await;

        let (foreign_id, foreign) =
            create_account(&db, Some(other_org), "Invoice Foreign").await;
        grant(
            &db,
            other_org,
            foreign_id,
            owner_id,
            &FOREIGN_PERMISSIONS,
        )
        .await;

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            bookkeeper,
            reader,
            foreign,
        })
    }

    async fn book(&self) -> Session {
        login(&self.state, &self.bookkeeper).await
    }

    async fn read(&self) -> Session {
        login(&self.state, &self.reader).await
    }

    async fn outsider(&self) -> Session {
        login(&self.state, &self.foreign).await
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("inv-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Invoice Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    use omnion_identity::users::{self, NewUser};
    let email = format!("inv-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: walk_auth::PASSWORD.to_owned(),
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
    use omnion_permissions::model::{
        Effect, NewBinding, NewRole, RolePermissionInput, Scope as PermScope,
    };
    use omnion_permissions::roles as role_store;

    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("inv-role-{}", Uuid::new_v4().simple()),
            name: "Invoice Test Role".to_owned(),
            description: "A role of the invoice walk".to_owned(),
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

    omnion_permissions::bindings::grant(
        db.pool(),
        NewBinding {
            role_id: role.id,
            user_id,
            scope: PermScope::Organization { organization_id },
            granted_by: Some(granted_by),
            expires_at: None,
        },
    )
    .await
    .expect("the role binding must be created");
}

async fn login(state: &AppState, email: &str) -> Session {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(Session::login_body(email)),
        ),
    )
    .await;
    Session::from_set_cookies(response.set_cookie)
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One invoice line: description, quantity, unit price, tax percent.
fn line(description: &str, qty: &str, unit_price: &str, tax_percent: &str) -> Value {
    json!({
        "description": description,
        "qty": qty,
        "unit_price": unit_price,
        "tax_percent": tax_percent,
    })
}

/// A manual invoice with the given lines.
fn invoice_body(customer: &str, lines: Vec<Value>) -> Value {
    json!({
        "customer_name": customer,
        "currency": "USD",
        "lines": lines,
    })
}

fn id_of(value: &Value) -> Uuid {
    value
        .get("id")
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| panic!("the response carries an id: {}", &value.to_string()[..200.min(value.to_string().len())]))
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("the response carries {key}: {}", value))
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_invoice_totals_what_its_lines_add_up_to_including_the_tax() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    // Two lines: 100.00 at 20% and 50.00 at 20%. The tax is charged on the pre-discount net, so
    // the header is 150.00 + 30.00 = 180.00. A header that reads 150.00 is the defect this
    // assertion exists for: it is the number a customer disputes.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body(
                "Northwind Ltd",
                vec![line("Consulting", "1", "100.00", "20"), line("Support", "1", "50.00", "20")],
            )),
        ),
    )
    .await;

    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "a manual invoice must be creatable: {}",
        created.body
    );
    let invoice = &created.body;

    assert_eq!(text(invoice, "subtotal"), "150.00", "{invoice}");
    assert_eq!(text(invoice, "tax_total"), "30.00", "{invoice}");
    assert_eq!(text(invoice, "discount_total"), "0.00", "{invoice}");
    assert_eq!(text(invoice, "grand_total"), "180.00", "{invoice}");
    assert_eq!(text(invoice, "outstanding"), "180.00", "{invoice}");
    assert_eq!(text(invoice, "status"), "draft", "{invoice}");
    assert!(text(invoice, "number").starts_with("INV-"), "{invoice}");

    // The lines carry their own tax, so changing a rate later cannot rewrite this document.
    let lines = invoice.get("lines").and_then(Value::as_array).expect("lines");
    assert_eq!(lines.len(), 2, "{invoice}");
    assert_eq!(text(&lines[0], "tax_percent"), "20", "{lines:?}");
    assert_eq!(text(&lines[0], "line_total"), "120.00", "{lines:?}");
    assert_eq!(text(&lines[1], "line_total"), "60.00", "{lines:?}");
}

#[tokio::test]
async fn the_server_recomputes_the_totals_rather_than_trusting_the_request() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    // The request claims totals. The struct has no field for them, so they are dropped on
    // deserialization — which is the point: a client cannot talk the server into a free invoice.
    let mut body = invoice_body("Fraud Ltd", vec![line("Widget", "1", "1000.00", "0")]);
    body["subtotal"] = json!("0.01");
    body["grand_total"] = json!("0.01");
    body["tax_total"] = json!("0.00");

    let created = call(
        &fixture.state,
        request(Method::POST, "/api/v1/accounting/invoices", Some(&book), Some(body)),
    )
    .await;

    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(text(&created.body, "grand_total"), "1000.00", "{}", created.body);
    assert_eq!(
        text(&created.body, "subtotal"), "1000.00",
        "the stored subtotal is the line's own amount, not the number the request claimed"
    );
}

#[tokio::test]
async fn a_discount_is_taken_before_the_tax() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    // 100.00 with a 10% discount is 90.00 of net; 20% tax on THAT is 18.00; gross 108.00.
    // Taxing the pre-discount 100.00 and then discounting would give 108.00 too here, but the
    // order is what makes the two agree everywhere, and this pins it.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body(
                "Discounted Ltd",
                vec![json!({
                    "description": "Discounted line",
                    "qty": "1",
                    "unit_price": "100.00",
                    "discount_percent": "10",
                    "tax_percent": "20",
                })],
            )),
        ),
    )
    .await;

    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(text(&created.body, "discount_total"), "10.00", "{}", created.body);
    assert_eq!(text(&created.body, "subtotal"), "100.00", "{}", created.body);
    assert_eq!(text(&created.body, "tax_total"), "18.00", "{}", created.body);
    assert_eq!(text(&created.body, "grand_total"), "108.00", "{}", created.body);
}

#[tokio::test]
async fn a_line_without_a_description_or_a_product_is_refused() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body(
                "Empty Ltd",
                vec![json!({ "qty": "1", "unit_price": "10.00" })],
            )),
        ),
    )
    .await;

    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a line that says nothing about what it bills is refused: {}",
        refused.body
    );
    assert_eq!(
        refused.body.pointer("/details/field"),
        Some(&json!("description")),
        "{}",
        refused.body
    );
}

#[tokio::test]
async fn a_zero_quantity_is_refused_as_a_heading_not_a_line() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body(
                "Zero Ltd",
                vec![line("Nothing", "0", "10.00", "0")],
            )),
        ),
    )
    .await;

    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body.pointer("/details/field"), Some(&json!("qty")), "{}", refused.body);
}

#[tokio::test]
async fn a_due_date_before_the_issue_date_is_refused_naming_both_dates() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(json!({
                "customer_name": "Backdated Ltd",
                "issue_date": "2026-03-10",
                "due_date": "2026-03-01",
                "lines": [line("Work", "1", "100.00", "0")],
            })),
        ),
    )
    .await;

    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let message = refused.body.get("message").and_then(Value::as_str).unwrap_or_default();
    assert!(message.contains("2026-03-01"), "{message}");
    assert!(message.contains("2026-03-10"), "{message}");
}

#[tokio::test]
async fn a_due_date_the_caller_omits_is_the_issue_date_plus_thirty_days() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(json!({
                "customer_name": "Terms Ltd",
                "issue_date": "2026-03-10",
                "lines": [line("Work", "1", "100.00", "0")],
            })),
        ),
    )
    .await;

    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(
        created.body.get("due_date").and_then(Value::as_str),
        Some("2026-04-09"),
        "the default terms are the server's, so the list and the document cannot disagree: {}",
        created.body
    );
}

#[tokio::test]
async fn sending_stamps_the_date_moves_the_status_and_refuses_a_second_send() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body("Sent Ltd", vec![line("Work", "1", "100.00", "0")])),
        ),
    )
    .await;
    let invoice_id = id_of(&created.body);

    let sent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/invoices/{invoice_id}/send"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;

    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    assert_eq!(text(&sent.body, "status"), "sent", "{}", sent.body);
    assert!(
        sent.body.get("sent_at").and_then(Value::as_str).is_some(),
        "sending stamps sent_at: {}",
        sent.body
    );

    // A second send would give the same number to a second issue of the document.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/invoices/{invoice_id}/send"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);
    assert!(
        again.body.get("message").and_then(Value::as_str).unwrap_or_default().contains("void"),
        "the refusal names the way out: {}",
        again.body
    );
}

#[tokio::test]
async fn voiding_keeps_the_number_requires_a_reason_and_takes_it_out_of_the_receivables() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body("Void Ltd", vec![line("Work", "1", "100.00", "0")])),
        ),
    )
    .await;
    let invoice_id = id_of(&created.body);
    let number = text(&created.body, "number");

    // A void with no reason is refused. "Withdrawn" with no sentence attached is
    // indistinguishable from a mistake, and the row is permanent.
    let no_reason = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/invoices/{invoice_id}/void"),
            Some(&book),
            Some(json!({ "reason": "  " })),
        ),
    )
    .await;
    assert_eq!(no_reason.status, StatusCode::BAD_REQUEST, "{}", no_reason.body);
    assert_eq!(
        no_reason.body.pointer("/details/field"),
        Some(&json!("reason")),
        "{}",
        no_reason.body
    );

    let voided = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/invoices/{invoice_id}/void"),
            Some(&book),
            Some(json!({ "reason": "billed against the wrong purchase order" })),
        ),
    )
    .await;
    assert_eq!(voided.status, StatusCode::OK, "{}", voided.body);
    assert_eq!(text(&voided.body, "status"), "void", "{}", voided.body);
    assert_eq!(
        text(&voided.body, "number"),
        number,
        "the number is kept: a customer's ledger refers to it"
    );
    assert_eq!(text(&voided.body, "void_reason"), "billed against the wrong purchase order", "{}", voided.body);

    // Still listed, and no longer a receivable: the Void tab must find it and the aging must not.
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/invoices?status=void",
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let rows = listed.body.as_array().cloned().unwrap_or_default();
    assert!(
        rows.iter().any(|row| row.get("id").and_then(Value::as_str) == Some(invoice_id.to_string().as_str())),
        "a voided invoice stays visible under the Void tab: {rows:?}"
    );
}

#[tokio::test]
async fn a_sales_order_becomes_a_draft_with_its_lines_and_is_not_converted_twice() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let order_id = confirmed_order(&fixture.db, fixture.organization).await;

    let converted = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(json!({ "order_id": order_id })),
        ),
    )
    .await;
    assert_eq!(converted.status, StatusCode::CREATED, "{}", converted.body);
    assert_eq!(
        converted.body.get("order_id").and_then(Value::as_str),
        Some(order_id.to_string().as_str()),
        "the invoice keeps the pointer back to the order: {}",
        converted.body
    );

    let lines = converted.body.get("lines").and_then(Value::as_array).expect("lines");
    assert_eq!(lines.len(), 2, "the order's lines are copied: {}", converted.body);
    assert_eq!(text(&lines[0], "description"), "Order line one", "{lines:?}");
    // 2 × 100.00 at 20% is 240.00, and 1 × 50.00 at 0% is 50.00.
    assert_eq!(text(&converted.body, "subtotal"), "250.00", "{}", converted.body);
    assert_eq!(text(&converted.body, "tax_total"), "40.00", "{}", converted.body);
    assert_eq!(text(&converted.body, "grand_total"), "290.00", "{}", converted.body);

    // The sales side now says the order has a draft against it.
    let order_state: Option<String> =
        sqlx::query_scalar("select invoice_state from sales_orders where id = $1")
            .bind(order_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the order row must read");
    assert_eq!(order_state.as_deref(), Some("draft"), "the sales side is told");

    // A second conversion is the mistake the handoff exists to prevent.
    let twice = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(json!({ "order_id": order_id })),
        ),
    )
    .await;
    assert_eq!(twice.status, StatusCode::CONFLICT, "{}", twice.body);
    assert!(
        twice.body.get("message").and_then(Value::as_str).unwrap_or_default().contains("already"),
        "{}",
        twice.body
    );
}

#[tokio::test]
async fn a_draft_cannot_carry_its_own_lines_when_it_is_converted_from_an_order() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let order_id = confirmed_order(&fixture.db, fixture.organization).await;

    // Silently preferring one over the other is how an invoice for 1,200 is created for an order
    // of 900, so the conflict is a refusal that names the field.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(json!({
                "order_id": order_id,
                "lines": [line("Something else", "1", "10.00", "0")],
            })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body.pointer("/details/field"), Some(&json!("lines")), "{}", refused.body);
}

#[tokio::test]
async fn the_overdue_sweep_flips_a_past_due_invoice_once_and_only_once() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    // Dated in the past, so the sweep has something to find.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(json!({
                "customer_name": "Late Ltd",
                "issue_date": "2026-01-10",
                "due_date": "2026-01-20",
                "lines": [line("Old work", "1", "100.00", "0")],
            })),
        ),
    )
    .await;
    let invoice_id = id_of(&created.body);

    // Sent, so it is a receivable: a draft nobody has seen is not late, it is unfinished.
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/invoices/{invoice_id}/send"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices/sweep-overdue",
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let flipped: Vec<String> = first
        .body
        .get("invoice_ids")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    assert!(
        flipped.contains(&invoice_id.to_string()),
        "the first sweep flips the invoice it found: {flipped:?}"
    );

    // **The second sweep changes nothing.** The REQ's acceptance criterion is "exactly once per
    // invoice", and it matters because the event drives a documented automation that sends an
    // e-mail: a sweep that fired per tick would mail the customer every tick.
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices/sweep-overdue",
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    assert_eq!(
        second.body.get("flipped").and_then(Value::as_u64),
        Some(0),
        "a second sweep must find nothing to do: {}",
        second.body
    );
    assert!(
        !second
            .body
            .get("invoice_ids")
            .and_then(Value::as_array)
            .map(|rows| rows.contains(&json!(invoice_id.to_string())))
            .unwrap_or(true),
        "the already-announced invoice is not announced again: {}",
        second.body
    );

    // And the invoice reads as overdue, with the days the list's red hint shows.
    let detail = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/invoices/{invoice_id}"),
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(text(&detail.body, "status"), "overdue", "{}", detail.body);
    let days = detail.body.get("days_past_due").and_then(Value::as_i64).unwrap_or(0);
    assert!(days > 0, "a past-due invoice carries its days: {}", detail.body);
}

#[tokio::test]
async fn the_overdue_tab_and_the_days_column_agree_with_each_other() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(json!({
                "customer_name": "Aging Ltd",
                "issue_date": "2026-01-10",
                "due_date": "2026-01-20",
                "lines": [line("Old work", "1", "250.00", "0")],
            })),
        ),
    )
    .await;
    let invoice_id = id_of(&created.body);
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/invoices/{invoice_id}/send"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;

    let all = list(&fixture.state, &book, "").await;
    let overdue_only = list(&fixture.state, &book, "&overdue_only=true").await;

    // The filter and the derived column are the same predicate spelled twice; a tab that
    // disagrees with the red date on the row is a receivables report a bookkeeper stops trusting.
    let in_all = all
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(invoice_id.to_string().as_str()));
    assert!(in_all.is_some(), "the invoice is in the unfiltered list: {all:?}");
    let days = in_all
        .and_then(|row| row.get("days_past_due"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    assert!(days > 0, "the unfiltered list already says it is late: {in_all:?}");
    assert!(
        overdue_only
            .iter()
            .any(|row| row.get("id").and_then(Value::as_str) == Some(invoice_id.to_string().as_str())),
        "and the overdue tab finds it: {overdue_only:?}"
    );
    // Its outstanding is the whole 250.00: nothing has been paid.
    assert_eq!(text(in_all.expect("present"), "outstanding"), "250.00");
}

#[tokio::test]
async fn the_list_searches_the_number_and_the_customer() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body("Zanzibar Trading", vec![line("Work", "1", "10.00", "0")])),
        ),
    )
    .await;
    let number = text(&created.body, "number");

    let by_customer = list(&fixture.state, &book, "&search=Zanzibar").await;
    assert!(
        by_customer
            .iter()
            .any(|row| text(row, "number") == number),
        "the customer name finds it: {by_customer:?}"
    );

    let by_number = list(&fixture.state, &book, &format!("&search={number}")).await;
    assert!(
        by_number.iter().any(|row| text(row, "number") == number),
        "the number finds it: {by_number:?}"
    );

    let nobody = list(&fixture.state, &book, "&search=NoSuchCustomerAtAll").await;
    assert!(nobody.is_empty(), "a search that matches nothing returns nothing: {nobody:?}");
}

#[tokio::test]
async fn a_misspelled_status_is_refused_with_the_ones_that_exist() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    // An empty list would read like "no invoices match", which is a different fact and a worse
    // one: the person would think the tab is broken.
    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/invoices?status=partially_paid",
            Some(&book),
            None,
        ),
    )
    .await;

    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let message = refused.body.get("message").and_then(Value::as_str).unwrap_or_default();
    for status in ["draft", "sent", "partial", "paid", "overdue", "void"] {
        assert!(message.contains(status), "the refusal names {status}: {message}");
    }
}

#[tokio::test]
async fn a_reader_may_see_the_invoices_and_may_not_write_one() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let reader = fixture.read().await;

    call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body("Visible Ltd", vec![line("Work", "1", "10.00", "0")])),
        ),
    )
    .await;

    let seen = call(
        &fixture.state,
        request(Method::GET, "/api/v1/accounting/invoices", Some(&reader), None),
    )
    .await;
    assert_eq!(seen.status, StatusCode::OK, "{}", seen.body);

    // Drafting is a document nobody has seen — but this role does not hold the key, and the
    // point of the test is that the guard is per-key rather than "any accounting key opens it".
    let written = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&reader),
            Some(invoice_body("Nope Ltd", vec![line("Work", "1", "10.00", "0")])),
        ),
    )
    .await;
    assert_eq!(written.status, StatusCode::FORBIDDEN, "{}", written.body);
}

#[tokio::test]
async fn every_invoice_route_refuses_an_anonymous_caller() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body("Anon Ltd", vec![line("Work", "1", "10.00", "0")])),
        ),
    )
    .await;
    let invoice_id = id_of(&created.body);

    for (method, uri) in [
        (Method::GET, "/api/v1/accounting/invoices".to_owned()),
        (
            Method::GET,
            format!("/api/v1/accounting/invoices/{invoice_id}"),
        ),
        (
            Method::POST,
            format!("/api/v1/accounting/invoices/{invoice_id}/send"),
        ),
        (
            Method::POST,
            format!("/api/v1/accounting/invoices/{invoice_id}/void"),
        ),
        (
            Method::POST,
            "/api/v1/accounting/invoices/sweep-overdue".to_owned(),
        ),
    ] {
        let response = call(
            &fixture.state,
            request(
                method.clone(),
                &uri,
                None,
                Some(json!({ "reason": "anonymous" })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must refuse an anonymous caller: {}",
            response.body
        );
    }
}

#[tokio::test]
async fn another_organizations_invoice_is_404_and_never_403() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(invoice_body("Private Ltd", vec![line("Work", "1", "999.00", "0")])),
        ),
    )
    .await;
    let invoice_id = id_of(&created.body);
    let outsider = fixture.outsider().await;

    // **404, not 403.** A 403 confirms the record exists, and one organization's receivables are
    // the thing this module exists to keep apart.
    for (method, uri) in [
        (
            Method::GET,
            format!("/api/v1/accounting/invoices/{invoice_id}"),
        ),
        (
            Method::POST,
            format!("/api/v1/accounting/invoices/{invoice_id}/send"),
        ),
        (
            Method::POST,
            format!("/api/v1/accounting/invoices/{invoice_id}/void"),
        ),
    ] {
        let response = call(
            &fixture.state,
            request(method.clone(), &uri, Some(&outsider), Some(json!({ "reason": "x" }))),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{method} {uri} must be 404 for another organization: {}",
            response.body
        );
    }

    // And the outsider's own list does not contain it.
    let theirs = list(&fixture.state, &outsider, "").await;
    assert!(
        !theirs
            .iter()
            .any(|row| row.get("id").and_then(Value::as_str) == Some(invoice_id.to_string().as_str())),
        "{theirs:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// A page of invoices with the given extra query string.
async fn list(state: &AppState, book: &Session, extra: &str) -> Vec<Value> {
    let uri = format!("/api/v1/accounting/invoices{extra}");
    let response = call(state, request(Method::GET, &uri, Some(book), None)).await;
    assert_eq!(response.status, StatusCode::OK, "{uri}: {}", response.body);
    response.body.as_array().cloned().unwrap_or_default()
}

/// A confirmed sales order with two lines, ready to be converted.
async fn confirmed_order(db: &Db, organization_id: Uuid) -> Uuid {
    let order_id: Uuid = sqlx::query_scalar(
        "insert into sales_orders (organization_id, number, customer_type, customer_name, \
             status, currency, subtotal, tax_total, grand_total) \
         values ($1, $2, 'company', 'Order Customer', 'confirmed', 'USD', 250.00, 40.00, 290.00) \
         returning id",
    )
    .bind(organization_id)
    .bind(format!("SO-{}", Uuid::new_v4().simple()))
    .fetch_one(db.pool())
    .await
    .expect("the test order must be created");

    for (position, (description, qty, price, tax)) in [
        ("Order line one", "2.000", "100.00", "20.00"),
        ("Order line two", "1.000", "50.00", "0.00"),
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query(
            "insert into sales_order_lines \
                 (organization_id, order_id, position, description, quantity, unit_price, \
                  tax_percent, line_total) \
             values ($1, $2, $3, $4, $5::numeric, $6::numeric, $7::numeric, $8::numeric)",
        )
        .bind(organization_id)
        .bind(order_id)
        .bind(position as i32 + 1)
        .bind(description)
        .bind(qty)
        .bind(price)
        .bind(tax)
        // The line total is the gross: 2 × 100 = 200 + 20% = 240, and 1 × 50 = 50 + 0% = 50.
        .bind(if position == 0 { "240.00" } else { "50.00" })
        .execute(db.pool())
        .await
        .expect("the test order line must be created");
    }

    order_id
}
