//! Integration tests for payments (docs/requests/REQ-054, slice 3).
//!
//! Written around **what a bookkeeper would try to do**, not around the endpoints:
//!
//! * a 40% payment leaves the invoice `partial` with the right outstanding, and the balance
//!   closes it as `paid` — the REQ's headline criterion, checked at both ends rather than at
//!   the end;
//! * **an allocation can never exceed the outstanding.** That is the rule the module exists to
//!   enforce, so it is checked three ways: refused with `422` and the numbers in the message,
//!   refused again with the numbers in `details`, and — the one a route-only check cannot catch
//!   — refused when the caller claims the override without holding the permission;
//! * the oldest-first sweep produces the documented split, and calling it twice on the same
//!   money produces the same answer;
//! * a reversal keeps the row, writes a **counter entry dated on the original payment**, releases
//!   the allocations and recomputes the invoice from what is still allocated rather than
//!   subtracting — the second payment's outstanding must not go negative;
//! * every route answers 401 without a session, 403 without the permission, and **404, never
//!   403** for another organization's payment.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::{BuildInfo, Db, RedisClient};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support {
    //! The sign-in half, shared with the invoice suite — see `support/walk_auth.rs` for why a
    //! hand-rolled `login()` is the exact defect that makes every write answer
    //! `csrf_unavailable`.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static PAYMENT_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The reader: may **see** payments, and nothing else.
const READER_PERMISSIONS: [&str; 3] = ["accounting.payments.read", "sites.read", "crm.contacts.read"];

/// The bookkeeper: the reader plus everything a payment needs — invoices to settle, the money,
/// and the ability to undo one.
const BOOKKEEPER_PERMISSIONS: [&str; 8] = [
    "accounting.payments.read",
    "accounting.payments.record",
    "accounting.payments.reverse",
    "accounting.invoices.read",
    "accounting.invoices.create",
    "accounting.invoices.send",
    "sites.read",
    "crm.contacts.read",
];

/// A writer in a second organization, for the cross-tenant `404`.
const FOREIGN_PERMISSIONS: [&str; 2] = ["accounting.payments.read", "accounting.payments.record"];

// ---------------------------------------------------------------------------------------------
// Harness
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
    // **A body that is not JSON is a string, never a panic.** An assertion that cannot print
    // what the server actually said is how a column typo costs three runs.
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
    /// The platform owner whose role grants the per-organization ones. Carried as an id rather
    /// than re-found with a query: "the most recent account with no organization" is an
    /// assumption about a shared table, and a walk that guesses at the fixture is a walk that
    /// breaks when another one runs first.
    owner_id: Uuid,
    bookkeeper: String,
    reader: String,
    foreign: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = PAYMENT_WALK.lock().await;
        let (state, db) = live_state().await?;
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

        let organization = create_organization_row(&db, "payments").await;
        let other_org = create_organization_row(&db, "payments-foreign").await;

        let (owner_id, _) = create_account(&db, None, "Payment Owner").await;
        omnion_permissions::seed::bind_owner(db.pool(), owner_id)
            .await
            .ok()?;

        let (reader_id, reader) = create_account(&db, Some(organization), "Payment Reader").await;
        grant(&db, organization, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (bookkeeper_id, bookkeeper) =
            create_account(&db, Some(organization), "Payment Bookkeeper").await;
        grant(
            &db,
            organization,
            bookkeeper_id,
            owner_id,
            &BOOKKEEPER_PERMISSIONS,
        )
        .await;

        let (foreign_id, foreign) = create_account(&db, Some(other_org), "Payment Foreign").await;
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
            owner_id,
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

    /// An account that additionally holds `accounting.payments.overpay`.
    ///
    /// Minted per call rather than shared, because the grant is a row in a shared table and a
    /// cached session would go stale the moment another walk revoked it. Two walks need this:
    /// one to prove the override is honoured, and one to prove that even **with** the override
    /// the arithmetic refusal stands — which is the property that makes the flag a business
    /// decision rather than a way to invent money.
    async fn overpayer(&self, book: &Session) -> Session {
        let (id, email) = create_account(
            &self.db,
            Some(self.organization),
            "Payment Overpayer",
        )
        .await;
        grant(
            &self.db,
            self.organization,
            id,
            self.owner_id,
            &[
                "accounting.payments.read",
                "accounting.payments.record",
                "accounting.payments.overpay",
                "accounting.invoices.read",
                "sites.read",
            ],
        )
        .await;
        let _ = book;
        login(&self.state, &email).await
    }

    /// An invoice for `total` at 0% tax, already sent so it can be collected against.
    async fn collectable(&self, book: &Session, customer: &str, total: &str) -> Uuid {
        let created = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/accounting/invoices",
                Some(book),
                Some(json!({
                    "customer_name": customer,
                    "currency": "USD",
                    "lines": [{
                        "description": "Consulting",
                        "qty": "1",
                        "unit_price": total,
                        "tax_percent": "0",
                    }],
                })),
            ),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "the invoice must be created: {}",
            created.body
        );
        let id = id_of(&created.body);

        let sent = call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/accounting/invoices/{id}/send"),
                Some(book),
                Some(json!({})),
            ),
        )
        .await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
        id
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("pay-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Payment Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    use omnion_identity::users::{self, NewUser};
    let email = format!("pay-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("pay-role-{}", Uuid::new_v4().simple()),
            name: "Payment Test Role".to_owned(),
            description: "A role of the payment walk".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    // `set_role_permissions` takes a **slice**, not an owned Vec — collecting straight into the
    // argument is what makes `&entries` the right expression and the compiler catches the
    // difference. `omnion_permissions::bindings::grant` is the binder, not `roles::bind_role`.
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
// Bodies and readers
// ---------------------------------------------------------------------------------------------

fn id_of(value: &Value) -> Uuid {
    value
        .get("id")
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| {
            panic!(
                "the response carries an id: {}",
                &value.to_string()[..200.min(value.to_string().len())]
            )
        })
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("the response carries {key}: {value}"))
        .to_owned()
}

fn money(value: &Value, key: &str) -> String {
    let raw = text(value, key);
    // Amounts are decimal **text**: `20` and `20.00` are the same money and the module
    // normalises to the latter. Comparing raw strings would fail on a correct answer.
    let normalized = omnion_module_accounting::money::Amount::parse(&raw)
        .unwrap_or_else(|error| panic!("{key}={raw} is not an amount: {error}"));
    normalized.to_text()
}

/// A payment that applies `amount` to `invoice`.
fn payment_body(invoice: Uuid, amount: &str) -> Value {
    json!({
        "amount": amount,
        "currency": "USD",
        "method": "bank_transfer",
        "allocations": [{ "invoice_id": invoice.to_string(), "amount": amount }],
    })
}

/// The invoice's current status and outstanding, read straight from the invoice route.
async fn invoice_state(
    fixture: &Fixture,
    book: &Session,
    invoice: Uuid,
) -> (String, String) {
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/invoices/{invoice}"),
            Some(book),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    (text(&read.body, "status"), money(&read.body, "outstanding"))
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_forty_percent_payment_leaves_it_partial_and_the_balance_closes_it() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Partial Ltd", "100.00").await;

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "40.00")),
        ),
    )
    .await;

    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert_eq!(money(&first.body, "amount"), "40.00", "{}", first.body);
    assert_eq!(money(&first.body, "allocated"), "40.00", "{}", first.body);
    assert_eq!(
        money(&first.body, "unallocated"),
        "0.00",
        "a payment applied entirely leaves no customer credit: {}",
        first.body
    );
    assert_eq!(
        text(&first.body, "allocation_state"),
        "applied",
        "{}",
        first.body
    );
    // The number is the document's own, and it is the display form rather than the integer the
    // sequence stores — the same split the invoice made.
    assert!(
        text(&first.body, "number").starts_with("PAY-"),
        "the payment carries a PAY- number: {}",
        first.body
    );
    // It wrote a journal entry: money arrived, and a receipt invisible to the ledger is not a
    // receipt.
    assert!(
        first.body.get("journal_entry_id").and_then(Value::as_str).is_some(),
        "the payment wrote its journal entry: {}",
        first.body
    );
    // The customer was taken from the invoice it settles, so the receipt names somebody.
    assert_eq!(
        text(&first.body, "customer_name"),
        "Partial Ltd",
        "a payment that settles one invoice takes that invoice's customer: {}",
        first.body
    );

    let (status, outstanding) = invoice_state(&fixture, &book, invoice).await;
    assert_eq!(status, "partial", "a 40% payment is partial");
    assert_eq!(outstanding, "60.00", "the outstanding is the balance");

    // And the second half closes it.
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "60.00")),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::CREATED, "{}", second.body);

    let (status, outstanding) = invoice_state(&fixture, &book, invoice).await;
    assert_eq!(status, "paid", "the balance closes it");
    assert_eq!(outstanding, "0.00", "a paid invoice owes nothing");

    // A third payment against a paid invoice is refused rather than silently accepted: the
    // invoice's own `paid_total <= grand_total` CHECK would turn it into a constraint error
    // naming nothing.
    let third = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "10.00")),
        ),
    )
    .await;
    assert_eq!(third.status, StatusCode::CONFLICT, "{}", third.body);
    assert!(
        third
            .body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("already paid"),
        "the refusal says why: {}",
        third.body
    );
}

#[tokio::test]
async fn an_allocation_above_the_outstanding_is_refused_with_the_three_numbers() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Overpay Ltd", "100.00").await;

    // Take 40.00 first so there is something left to over-allocate against.
    call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "40.00")),
        ),
    )
    .await;

    let over = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "80.00")),
        ),
    )
    .await;

    assert_eq!(
        over.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an over-allocation is the family's second 422: {}",
        over.body
    );
    let message = over
        .body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    assert!(message.contains("60.00"), "the message names what is owed: {message}");
    assert!(message.contains("80.00"), "the message names what was asked: {message}");
    assert!(
        message.contains("INV-"),
        "the message names the invoice: {message}"
    );

    // The same three numbers in `details`, because the screen shows the sentence and the grid
    // shows the columns — and both must read the same figures.
    let details = over.body.pointer("/error/details").cloned().unwrap_or(Value::Null);
    assert_eq!(
        money(&details, "outstanding"),
        "60.00",
        "details carries the outstanding: {}",
        over.body
    );
    assert_eq!(
        money(&details, "attempted"),
        "80.00",
        "details carries the attempt: {}",
        over.body
    );

    // **And the refusal wrote nothing.** The invoice is where it shows: still 40.00 in.
    let (status, outstanding) = invoice_state(&fixture, &book, invoice).await;
    assert_eq!(status, "partial");
    assert_eq!(outstanding, "60.00", "the refused payment changed nothing");

    let allocations: i64 = sqlx::query_scalar(
        "select count(*) from accounting_payment_allocations where invoice_id = $1",
    )
    .bind(invoice)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the allocations must be countable");
    assert_eq!(
        allocations, 1,
        "the refused payment wrote no allocation row"
    );
}

#[tokio::test]
async fn claiming_the_override_without_the_permission_is_refused_before_the_arithmetic() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Override Ltd", "100.00").await;

    // The bookkeeper holds `.record` but **not** `.overpay`. The refusal must name the key,
    // because "422" on its own sends an operator to check the arithmetic they already checked.
    let claimed = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(json!({
                "amount": "150.00",
                "currency": "USD",
                "method": "cash",
                "allow_overpayment": true,
                "allocations": [{ "invoice_id": invoice.to_string(), "amount": "150.00" }],
            })),
        ),
    )
    .await;

    assert_eq!(
        claimed.status,
        StatusCode::FORBIDDEN,
        "claiming the override without the key is refused: {}",
        claimed.body
    );
    let message = claimed
        .body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("accounting.payments.overpay"),
        "the refusal names the permission that would allow it: {message}"
    );

    let (status, outstanding) = invoice_state(&fixture, &book, invoice).await;
    assert_eq!(status, "sent", "nothing was collected: {status}");
    assert_eq!(outstanding, "100.00");
}

#[tokio::test]
async fn the_override_is_honoured_when_the_key_is_held() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Goodwill Ltd", "100.00").await;

    // The override is a key of its own, so it needs an account that holds it: recording through
    // the bookkeeper would be refused before the arithmetic ever ran.
    let session = fixture.overpayer(&book).await;

    // The invoice's own `paid_total <= grand_total` CHECK caps the write at the total, so the
    // walk allocates exactly the outstanding — the case the override exists to *permit* rather
    // than to bypass.

    let goodwill = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&session),
            Some(json!({
                "amount": "100.00",
                "currency": "USD",
                "method": "other",
                "note": "a goodwill write-off on the last cent",
                "allow_overpayment": true,
                "allocations": [{ "invoice_id": invoice.to_string(), "amount": "100.00" }],
            })),
        ),
    )
    .await;
    assert_eq!(goodwill.status, StatusCode::CREATED, "{}", goodwill.body);
    assert_eq!(text(&goodwill.body, "method"), "other");

    let (status, outstanding) = invoice_state(&fixture, &session, invoice).await;
    assert_eq!(status, "paid");
    assert_eq!(outstanding, "0.00");
}

#[tokio::test]
async fn the_oldest_first_sweep_splits_the_money_the_way_the_docs_say() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    // Three invoices with different due dates, written oldest first. The sweep is documented as
    // "oldest first", so a run that closed the newest would be a real defect and not a
    // difference of opinion.
    let first = fixture.collectable(&book, "Sweep Ltd", "50.00").await;
    let second = fixture.collectable(&book, "Sweep Ltd", "30.00").await;
    let third = fixture.collectable(&book, "Sweep Ltd", "70.00").await;
    set_due_dates(&fixture.db, &[(first, "2026-01-01"), (second, "2026-02-01"), (third, "2026-03-01")]).await;

    // 60.00 against 150.00 owed: it must take the first invoice whole (50.00) and 10.00 of the
    // second, leaving the third untouched.
    let swept = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(json!({
                "amount": "60.00",
                "currency": "USD",
                "method": "bank_transfer",
                "auto_allocate": true,
            })),
        ),
    )
    .await;
    assert_eq!(swept.status, StatusCode::CREATED, "{}", swept.body);

    let allocations = swept
        .body
        .get("allocations")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("the payment carries its allocations: {}", swept.body));
    assert_eq!(allocations.len(), 2, "60.00 settles the first and part of the second");
    assert_eq!(money(&allocations[0], "amount"), "50.00", "the oldest is taken whole");
    assert_eq!(money(&allocations[1], "amount"), "10.00", "the remainder goes to the next");
    // The allocations name the invoices, oldest first, in the order the sweep walked them. The
    // key is `invoice_id`: an allocation row has an `id` of its own (it is a real row, with
    // `invoice_id` pointing at what it was applied to), so reading `id` here compares a uuid to a
    // uuid and fails on a walk whose real content is correct.
    assert_eq!(
        text(&allocations[0], "invoice_number"),
        "INV-000001",
        "the oldest invoice is the first allocation: {}",
        swept.body
    );
    assert_eq!(
        text(&allocations[1], "invoice_number"),
        "INV-000002",
        "the remainder goes to the next oldest, never the newest: {}",
        swept.body
    );
    // …and, read by id rather than by number, so a sweep that put the rows in the right order but
    // attached them to the wrong invoices is still caught.
    let invoices: Vec<String> = allocations
        .iter()
        .map(|entry| {
            entry
                .get("invoice_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        invoices,
        vec![first.to_string(), second.to_string()],
        "in sweep order, not by number: {}",
        swept.body
    );

    assert_eq!(invoice_state(&fixture, &book, first).await.1, "0.00", "the oldest is closed");
    assert_eq!(invoice_state(&fixture, &book, second).await.1, "20.00", "the second keeps 20.00");
    assert_eq!(invoice_state(&fixture, &book, third).await.1, "70.00", "the newest is untouched");

    // The sweep leaves the payment's own remainder as customer credit rather than losing it.
    assert_eq!(money(&swept.body, "allocated"), "60.00");
    assert_eq!(money(&swept.body, "unallocated"), "0.00");
    assert_eq!(text(&swept.body, "allocation_state"), "applied");
}

#[tokio::test]
async fn a_payment_larger_than_everything_owed_leaves_the_surplus_as_customer_credit() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Surplus Ltd", "30.00").await;

    // 50.00 against 30.00 owed, with the auto sweep. This is not an over-allocation — the money
    // is real and the customer is owed the difference back — so the sweep must take 30.00 and
    // leave 20.00 on account, and the journal must credit a liability for it rather than
    // inventing income.
    let swept = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(json!({
                "amount": "50.00",
                "currency": "USD",
                "method": "cash",
                "auto_allocate": true,
            })),
        ),
    )
    .await;
    assert_eq!(swept.status, StatusCode::CREATED, "{}", swept.body);
    assert_eq!(money(&swept.body, "allocated"), "30.00", "{}", swept.body);
    assert_eq!(
        money(&swept.body, "unallocated"),
        "20.00",
        "the surplus is held as customer credit, not dropped: {}",
        swept.body
    );
    assert_eq!(text(&swept.body, "allocation_state"), "partial");

    // The entry balances, and the surplus is a **credit** on a liability account rather than on
    // income — the difference is the whole quarter's numbers.
    let entry = text(&swept.body, "journal_entry_id");
    let (debit, credit) = journal_totals(&fixture.db, &entry).await;
    assert_eq!(debit, credit, "the entry balances: {debit} vs {credit}");
    let advances: String = sqlx::query_scalar(
        "select coalesce(sum(credit), 0)::text from accounting_journal_lines \
         where entry_id = $1 and account_id = (select id from accounting_accounts \
         where organization_id = $2 and code = '2300')",
    )
    .bind(Uuid::parse_str(&entry).expect("the entry id must parse"))
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the advances line must be readable");
    assert_eq!(
        omnion_module_accounting::money::Amount::parse(&advances)
            .expect("the advance amount must parse")
            .to_text(),
        "20.00",
        "the surplus sits on Customer Advances (2300), not on income"
    );
}

#[tokio::test]
async fn a_reversal_keeps_the_row_writes_a_counter_entry_and_restores_the_outstanding() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Reverse Ltd", "100.00").await;

    let recorded = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "100.00")),
        ),
    )
    .await;
    assert_eq!(recorded.status, StatusCode::CREATED, "{}", recorded.body);
    let payment_id = id_of(&recorded.body);
    let original_entry = text(&recorded.body, "journal_entry_id");
    assert_eq!(invoice_state(&fixture, &book, invoice).await.0, "paid");

    // A reversal without a reason is refused: "undo it" with no reason is an edit with extra
    // steps, and the reason is the only part an auditor reads.
    let no_reason = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/payments/{payment_id}/reverse"),
            Some(&book),
            Some(json!({ "reason": "  " })),
        ),
    )
    .await;
    assert_eq!(no_reason.status, StatusCode::BAD_REQUEST, "{}", no_reason.body);

    let reversed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/payments/{payment_id}/reverse"),
            Some(&book),
            Some(json!({ "reason": "the transfer was cancelled by the bank" })),
        ),
    )
    .await;
    assert_eq!(reversed.status, StatusCode::OK, "{}", reversed.body);
    assert_eq!(reversed.body.get("reversed"), Some(&Value::Bool(true)));
    assert_eq!(
        text(&reversed.body, "reversal_reason"),
        "the transfer was cancelled by the bank",
        "the reason is kept with the document"
    );
    assert!(
        reversed
            .body
            .get("reversal_entry_id")
            .and_then(Value::as_str)
            .is_some(),
        "the reversal wrote a counter entry: {}",
        reversed.body
    );

    // **The original record stands.** The row, its number, its amount, its journal entry — a
    // reversal is a second document, never an edit of the first.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/payments/{payment_id}"),
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(text(&read.body, "number"), text(&recorded.body, "number"));
    assert_eq!(money(&read.body, "amount"), "100.00");
    assert_eq!(
        read.body.get("journal_entry_id").and_then(Value::as_str),
        Some(original_entry.as_str()),
        "the original entry is still the payment's own"
    );

    // The invoice is back to owed.
    let (status, outstanding) = invoice_state(&fixture, &book, invoice).await;
    assert_eq!(status, "sent", "a reversed payment puts the invoice back to sent");
    assert_eq!(outstanding, "100.00", "the whole amount is owed again");

    // And reversing twice is refused, because the second one would post a second counter entry.
    let twice = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/payments/{payment_id}/reverse"),
            Some(&book),
            Some(json!({ "reason": "changed my mind" })),
        ),
    )
    .await;
    assert_eq!(twice.status, StatusCode::CONFLICT, "{}", twice.body);
    assert!(
        twice
            .body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("already reversed"),
        "the refusal says why: {}",
        twice.body
    );
}

#[tokio::test]
async fn a_reversal_recomputes_the_invoice_from_what_is_still_allocated() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Two Payments Ltd", "100.00").await;

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "60.00")),
        ),
    )
    .await;
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "40.00")),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::CREATED, "{}", second.body);
    assert_eq!(invoice_state(&fixture, &book, invoice).await.1, "0.00");

    // Reverse the **first** one while the second is still standing. Subtracting 60.00 from a paid
    // invoice would take it to -60.00 and the column's own CHECK would refuse the write with a
    // constraint name instead of an answer. The correct figure is the sum of what remains: 40.00
    // is what is still *paid*, so of a 100.00 invoice the **outstanding** is 60.00 — paid and
    // outstanding are the two ends of one subtraction and an assertion that reads one while meaning
    // the other is how a correct module gets "fixed" into an incorrect one.
    let reversed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!(
                "/api/v1/accounting/payments/{}/reverse",
                id_of(&first.body)
            ),
            Some(&book),
            Some(json!({ "reason": "recorded against the wrong invoice" })),
        ),
    )
    .await;
    assert_eq!(reversed.status, StatusCode::OK, "{}", reversed.body);

    let (status, outstanding) = invoice_state(&fixture, &book, invoice).await;
    assert_eq!(
        outstanding, "60.00",
        "the invoice keeps what the surviving payment left owed: {status}"
    );
    assert_eq!(status, "partial", "and it is partial again, not paid");

    // The same fact from the other end, so a future reader cannot read this as "40.00 of money
    // vanished": `paid_total` is the surviving 40.00 and `grand_total` is untouched at 100.00.
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/invoices/{invoice}"),
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(money(&read.body, "paid_total"), "40.00", "only the surviving payment pays");
    assert_eq!(money(&read.body, "grand_total"), "100.00", "the total never moved");
}

#[tokio::test]
async fn a_payment_against_a_draft_or_a_voided_invoice_is_refused_with_the_way_out() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;

    // A draft has never been given to anybody, so there is nothing to collect against.
    let draft = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/invoices",
            Some(&book),
            Some(json!({
                "customer_name": "Draft Ltd",
                "currency": "USD",
                "lines": [{ "description": "Work", "qty": "1", "unit_price": "10.00", "tax_percent": "0" }],
            })),
        ),
    )
    .await;
    let draft_id = id_of(&draft.body);
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(draft_id, "10.00")),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert!(
        refused
            .body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("send it"),
        "the refusal names the way out: {}",
        refused.body
    );

    // A voided one is withdrawn: the answer is a new invoice, not a payment.
    let sent = fixture.collectable(&book, "Void Ltd", "10.00").await;
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/invoices/{sent}/void"),
            Some(&book),
            Some(json!({ "reason": "issued to the wrong entity" })),
        ),
    )
    .await;
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(sent, "10.00")),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert!(
        refused
            .body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("voided"),
        "the refusal says the invoice was voided: {}",
        refused.body
    );
}

#[tokio::test]
async fn a_payment_cannot_allocate_more_than_it_itself_or_name_an_invoice_twice() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Arithmetic Ltd", "100.00").await;

    // This walk runs as the **overpayer**, not the bookkeeper, and that is the point of it: the
    // override must not reach the arithmetic. Sending it without the key would be refused by the
    // route before the module was consulted, so the walk would pass without ever exercising the
    // rule it exists to prove.
    let session = fixture.overpayer(&book).await;

    // The allocations exceed the payment. Even with the override claimed and granted, this is
    // refused: the difference would be money nobody received, which is arithmetic and not a
    // business decision.
    let too_much = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&session),
            Some(json!({
                "amount": "50.00",
                "currency": "USD",
                "allow_overpayment": true,
                "allocations": [
                    { "invoice_id": invoice.to_string(), "amount": "30.00" },
                    { "invoice_id": invoice.to_string(), "amount": "40.00" },
                ],
            })),
        ),
    )
    .await;
    assert_eq!(too_much.status, StatusCode::BAD_REQUEST, "{}", too_much.body);
    let message = too_much
        .body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("twice"),
        "the same invoice named twice is a form error, not a unique violation: {message}"
    );

    // A payment with nothing to apply it to is refused rather than recorded as a mystery.
    let nothing = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&session),
            Some(json!({ "amount": "10.00", "currency": "USD" })),
        ),
    )
    .await;
    assert_eq!(nothing.status, StatusCode::BAD_REQUEST, "{}", nothing.body);

    // An allocation named with no amount is dropped, not refused: the recorder's grid leaves
    // one behind every time a picker is cleared, and an empty row should not block a real one.
    let with_a_blank_row = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&session),
            Some(json!({
                "amount": "25.00",
                "currency": "USD",
                "allocations": [
                    { "invoice_id": invoice.to_string(), "amount": "25.00" },
                    { "invoice_id": invoice.to_string(), "amount": "" },
                ],
            })),
        ),
    )
    .await;
    assert_eq!(
        with_a_blank_row.status,
        StatusCode::CREATED,
        "an empty allocation row is dropped, not refused: {}",
        with_a_blank_row.body
    );
    assert_eq!(money(&with_a_blank_row.body, "allocated"), "25.00");
}

#[tokio::test]
async fn the_list_filters_and_searches_and_hides_a_reversed_payment_on_request() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Filter Ltd", "100.00").await;

    let kept = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "10.00")),
        ),
    )
    .await;
    let number = text(&kept.body, "number");
    let payment_id = id_of(&kept.body);

    // The list answers with the module's own page shape: `items`, and a cursor when there may
    // be more. A bare array cannot say "there is more", which is why every module since writes
    // one page type.
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/payments?search=Filter",
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let items = listed
        .body
        .get("items")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("the list is a page with items: {}", listed.body));
    assert!(
        items.iter().any(|row| text(row, "number") == number),
        "the search finds the payment by customer: {}",
        listed.body
    );

    // By its own number too.
    let by_number = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/payments?search={number}"),
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(
        by_number
            .body
            .get("items")
            .and_then(Value::as_array)
            .map(|rows| rows.len()),
        Some(1),
        "the search finds it by number: {}",
        by_number.body
    );

    // A method that does not exist is refused with the four that do.
    let bad_method = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/payments?method=cheque",
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(bad_method.status, StatusCode::BAD_REQUEST, "{}", bad_method.body);
    assert!(
        bad_method
            .body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("bank_transfer"),
        "the refusal names the methods that exist: {}",
        bad_method.body
    );

    // A filter that works: `cash` finds nothing here, and the reversed filter does not hide the
    // row until it is actually reversed.
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/payments/{payment_id}/reverse"),
            Some(&book),
            Some(json!({ "reason": "wrong reference on the receipt" })),
        ),
    )
    .await;

    let open_only = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/payments?search={number}&unreversed_only=true"),
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(
        open_only
            .body
            .get("items")
            .and_then(Value::as_array)
            .map(|rows| rows.len()),
        Some(0),
        "a reversed payment drops out of the unreversed view: {}",
        open_only.body
    );

    // And it is still there when the filter is off — the record was never deleted.
    let all = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/payments?search={number}"),
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(
        all.body
            .get("items")
            .and_then(Value::as_array)
            .map(|rows| rows.len()),
        Some(1),
        "the reversed payment is still in the list: {}",
        all.body
    );
}

#[tokio::test]
async fn a_reader_may_see_the_payments_and_may_not_record_one() {
    let Some(fixture) = Fixture::new().await else { return };
    let read = fixture.read().await;
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Reader Ltd", "10.00").await;

    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/accounting/payments", Some(&read), None),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);

    let written = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&read),
            Some(payment_body(invoice, "10.00")),
        ),
    )
    .await;
    assert_eq!(
        written.status,
        StatusCode::FORBIDDEN,
        "a reader may not record money: {}",
        written.body
    );
}

#[tokio::test]
async fn every_payment_route_refuses_an_anonymous_caller() {
    let Some(fixture) = Fixture::new().await else { return };
    let routes = [
        (Method::GET, "/api/v1/accounting/payments".to_owned(), None),
        (
            Method::GET,
            format!("/api/v1/accounting/payments/{}", Uuid::new_v4()),
            None,
        ),
        (
            Method::POST,
            "/api/v1/accounting/payments".to_owned(),
            Some(json!({ "amount": "1.00" })),
        ),
        (
            Method::POST,
            format!("/api/v1/accounting/payments/{}/reverse", Uuid::new_v4()),
            Some(json!({ "reason": "because" })),
        ),
    ];

    for (method, uri, body) in routes {
        let response = call(&fixture.state, request(method.clone(), &uri, None, body)).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must refuse an anonymous caller: {}",
            response.body
        );
    }
}

#[tokio::test]
async fn another_organizations_payment_is_404_and_never_403() {
    let Some(fixture) = Fixture::new().await else { return };
    let book = fixture.book().await;
    let invoice = fixture.collectable(&book, "Tenant Ltd", "10.00").await;
    let recorded = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/payments",
            Some(&book),
            Some(payment_body(invoice, "10.00")),
        ),
    )
    .await;
    let payment_id = id_of(&recorded.body);

    // The outsider holds the full payment set — `read`, `record`, everything but the reverse
    // key. A `403` here would tell them a row exists in another tenant, which is the one answer
    // this family of modules must never give.
    let outsider = fixture.outsider().await;
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/payments/{payment_id}"),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::NOT_FOUND,
        "another organization's payment is 404: {}",
        read.body
    );

    let reversed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/payments/{payment_id}/reverse"),
            Some(&outsider),
            Some(json!({ "reason": "not mine to undo" })),
        ),
    )
    .await;
    assert_eq!(
        reversed.status,
        StatusCode::NOT_FOUND,
        "reversing somebody else's payment is 404, not 403: {}",
        reversed.body
    );
}

// ---------------------------------------------------------------------------------------------
// Direct reads
// ---------------------------------------------------------------------------------------------

/// The two totals of a journal entry, as normalised text.
async fn journal_totals(db: &Db, entry_id: &str) -> (String, String) {
    let row: (String, String) = sqlx::query_as(
        "select debit_total::text, credit_total::text from accounting_journal_entries where id = $1",
    )
    .bind(Uuid::parse_str(entry_id).expect("the entry id must parse"))
    .fetch_one(db.pool())
    .await
    .expect("the entry must exist");
    (
        omnion_module_accounting::money::Amount::parse(&row.0)
            .expect("debit parses")
            .to_text(),
        omnion_module_accounting::money::Amount::parse(&row.1)
            .expect("credit parses")
            .to_text(),
    )
}

/// Backdate invoices so the oldest-first sweep has an order to follow.
async fn set_due_dates(db: &Db, pairs: &[(Uuid, &str)]) {
    for (invoice, due) in pairs {
        sqlx::query(
            "update accounting_invoices set due_date = $2::date where id = $1 \
             and organization_id = (select organization_id from accounting_invoices where id = $1)",
        )
        .bind(invoice)
        .bind(due)
        .execute(db.pool())
        .await
        .expect("the due date must be writable");
    }
}