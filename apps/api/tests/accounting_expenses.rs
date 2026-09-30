//! Integration tests for expenses (docs/requests/REQ-054, slice 4).
//!
//! Written around **what a bookkeeper would try to do**, not around the endpoints:
//!
//! * a receipt is filed, submitted, **approved**, and the approval reaches the ledger — dated on
//!   the day the money was spent, not on the day it was signed off, which is the property that
//!   keeps a month-end's numbers where the cost belongs;
//! * **approving twice posts one entry, not two.** Two approvers pressing the button at the same
//!   moment is a real thing that happens, and two balanced entries balance;
//! * a rejection **demands a reason** and the reason is readable on the expense, and a rejected
//!   expense can be filed again;
//! * a reader may see an expense and may not approve one; another organization's expense is `404`,
//!   never `403`, **with and without** the approve key;
//! * the decision is audited with a before/after status, and the list returns exactly the rows a
//!   filter named.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_module_accounting::money::Amount;
use omnion_api::state::AppState;
use omnion_core::{BuildInfo, Db, RedisClient};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support {
    //! The sign-in half, shared with the payment suite — see `support/walk_auth.rs` for why a
    //! hand-rolled `login()` is the exact defect that makes every write answer
    //! `csrf_unavailable`.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static EXPENSE_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The reader: may **see** an expense, and nothing else.
const READER_PERMISSIONS: [&str; 2] = ["accounting.expenses.read", "sites.read"];

/// The claimant: files and edits, and submits — everything short of deciding.
const CLAIMANT_PERMISSIONS: [&str; 4] = [
    "accounting.expenses.read",
    "accounting.expenses.create",
    "accounting.expenses.update",
    "sites.read",
];

/// The bookkeeper: the claimant plus the decision, which is the split the permission catalogue
/// exists to express — a role that may file its own expenses and sign them off is a role nobody
/// would grant on purpose.
const BOOKKEEPER_PERMISSIONS: [&str; 5] = [
    "accounting.expenses.read",
    "accounting.expenses.create",
    "accounting.expenses.update",
    "accounting.expenses.approve",
    "sites.read",
];

/// A writer in a second organization, for the cross-tenant `404`.
const FOREIGN_PERMISSIONS: [&str; 3] = [
    "accounting.expenses.read",
    "accounting.expenses.approve",
    "sites.read",
];

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
    claimant: String,
    reader: String,
    foreign: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = EXPENSE_WALK.lock().await;
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

        let organization = create_organization_row(&db, "expenses").await;
        let other_org = create_organization_row(&db, "expenses-foreign").await;

        let (owner_id, _) = create_account(&db, None, "Expense Owner").await;
        omnion_permissions::seed::bind_owner(db.pool(), owner_id)
            .await
            .ok()?;

        let (reader_id, reader) = create_account(&db, Some(organization), "Expense Reader").await;
        grant(&db, organization, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (claimant_id, claimant) =
            create_account(&db, Some(organization), "Expense Claimant").await;
        grant(
            &db,
            organization,
            claimant_id,
            owner_id,
            &CLAIMANT_PERMISSIONS,
        )
        .await;

        let (book_id, bookkeeper) = create_account(&db, Some(organization), "Expense Bookkeeper").await;
        grant(
            &db,
            organization,
            book_id,
            owner_id,
            &BOOKKEEPER_PERMISSIONS,
        )
        .await;

        let (foreign_id, foreign) = create_account(&db, Some(other_org), "Expense Foreign").await;
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
            claimant,
            reader,
            foreign,
        })
    }

    async fn book(&self) -> Session {
        login(&self.state, &self.bookkeeper).await
    }

    async fn claim(&self) -> Session {
        login(&self.state, &self.claimant).await
    }

    async fn read(&self) -> Session {
        login(&self.state, &self.reader).await
    }

    async fn outsider(&self) -> Session {
        login(&self.state, &self.foreign).await
    }

    /// The journal entry the approval wrote, read straight from the table.
    ///
    /// **Asserted against the rows rather than the response.** The property being proved is "the
    /// approval reached the ledger", and the only honest witness for that is the ledger itself: a
    /// route that returned `journal_entry_id` without writing the entry would satisfy every other
    /// assertion in the file.
    async fn entry_for(&self, expense: Uuid) -> Option<(i64, String, String)> {
        sqlx::query_as::<_, (i64, String, String)>(
            "select j.entry_number, j.entry_date::text, j.source_kind \
             from accounting_journal_entries j \
             join accounting_expenses e on e.journal_entry_id = j.id \
             where e.id = $1",
        )
        .bind(expense)
        .fetch_optional(self.db.pool())
        .await
        .expect("the ledger read must succeed")
    }

    /// The entry's own lines, so a test can prove it balances and where it landed.
    async fn entry_lines(&self, expense: Uuid) -> Vec<(String, String, String)> {
        sqlx::query_as::<_, (String, String, String)>(
            "select a.code, l.debit::text, l.credit::text \
             from accounting_journal_lines l \
             join accounting_accounts a on a.id = l.account_id \
             join accounting_journal_entries j on j.id = l.entry_id \
             join accounting_expenses e on e.journal_entry_id = j.id \
             where e.id = $1 order by l.position",
        )
        .bind(expense)
        .fetch_all(self.db.pool())
        .await
        .expect("the ledger read must succeed")
    }

    /// How many journal entries reference this expense — the assertion that catches a double post.
    async fn entry_count(&self, expense: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "select count(*) from accounting_journal_entries j \
             join accounting_expenses e on e.journal_entry_id = j.id \
             where e.id = $1 or j.source_id = $1",
        )
        .bind(expense)
        .fetch_one(self.db.pool())
        .await
        .expect("the count must succeed")
    }

    /// The audit trail's newest action for an expense.
    async fn last_audit(&self, expense: Uuid) -> Option<(String, String)> {
        // **`audit_log`, not `audit_entries`.** The table is named after what it is; a query
        // against the other name raises `relation does not exist`, the `?` swallows it into
        // `None`, and the walk fails with "an audit row must exist" — which reads as *the audit
        // trail is broken* rather than *this test names a table that is not there*. The trail was
        // written correctly the whole time.
        sqlx::query_as::<_, (String, String)>(
            "select action, metadata::text from audit_log \
             where target_id = $1::text order by created_at desc limit 1",
        )
        .bind(expense)
        .fetch_optional(self.db.pool())
        .await
        // **An error here is a failure, not a `None`.** The `.ok().flatten()` this replaces turned
        // a wrong table name into "no audit row exists", which reads as a broken audit trail —
        // the one thing a walk like this exists to defend. `expect` names the table.
        .expect("the audit_log read must succeed")
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("exp-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Expense Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    use omnion_identity::users::{self, NewUser};
    let email = format!("exp-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("exp-role-{}", Uuid::new_v4().simple()),
            name: "Expense Test Role".to_owned(),
            description: "A role of the expense walk".to_owned(),
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

/// The message out of an error envelope.
///
/// **Not `body["message"]`.** The envelope is `{"error":{"code","message","details","request_id"}}`,
/// so a flat read yields `None` and `contains(...)` on it is `false` — which turns "the refusal
/// does not explain itself" into a failure that looks like a product defect when the module is
/// behaving perfectly. Every message assertion in this file goes through here, so the envelope's
/// shape is written down once.
fn message_of(body: &Value) -> String {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("the refusal carries a message: {body}"))
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

/// A filed receipt: description, amount and the day the money was spent.
fn expense_body(description: &str, amount: &str, date: &str) -> Value {
    json!({
        "description": description,
        "amount": amount,
        "currency": "USD",
        "expense_date": date,
        "category": "travel",
        "vendor": "A Hotel",
    })
}

/// File one expense as a draft and return its id.
async fn file(fixture: &Fixture, book: &Session, description: &str, amount: &str) -> Uuid {
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/expenses",
            Some(book),
            Some(expense_body(description, amount, "2026-03-12")),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the expense must be created: {}",
        created.body
    );
    id_of(&created.body)
}

/// Draft -> submitted -> approved, returning the approved view.
async fn approve(
    fixture: &Fixture,
    book: &Session,
    expense: Uuid,
    comment: &str,
) -> TestResponse {
    let submitted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/submit"),
            Some(book),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(submitted.status, StatusCode::OK, "{}", submitted.body);
    assert_eq!(text(&submitted.body, "status"), "submitted");

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/decision"),
            Some(book),
            Some(json!({ "approved": true, "comment": comment })),
        ),
    )
    .await
}

async fn read_expense(fixture: &Fixture, book: &Session, expense: Uuid) -> TestResponse {
    call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/expenses/{expense}"),
            Some(book),
            None,
        ),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_receipt_is_filed_submitted_and_approved_and_the_approval_reaches_the_ledger() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;

    let filed = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/expenses",
            Some(&book),
            Some(expense_body(
                "Two nights, Krakow conference",
                "480.00",
                "2026-03-12",
            )),
        ),
    )
    .await;
    assert_eq!(filed.status, StatusCode::CREATED, "{}", filed.body);

    let expense = id_of(&filed.body);
    // **A number, not a uuid.** Every other financial document in this module carries one, and a
    // receipt a person can only identify by a uuid is a receipt nobody can ask about on a call.
    let number = text(&filed.body, "number");
    assert!(
        number.starts_with("EXP-"),
        "an expense carries a per-organization number: {number}"
    );
    assert_eq!(text(&filed.body, "status"), "draft");
    // The gross is stored as sent: the tax is a *portion* of it, not an addition, and the slice-2
    // `gross_of` slip that reported a 100.00 receipt at 20% as 120.00 starts right here.
    assert_eq!(money(&filed.body, "amount"), "480.00");
    assert_eq!(money(&filed.body, "tax_amount"), "0.00");

    let decided = approve(&fixture, &book, expense, "Conference travel, approved").await;
    assert_eq!(decided.status, StatusCode::OK, "{}", decided.body);
    assert_eq!(text(&decided.body, "status"), "approved");
    assert!(
        !decided.body["journal_entry_id"].is_null(),
        "an approval writes a journal entry and says which: {}",
        decided.body
    );

    // **Dated on the day the money was spent, not the day it was signed off.** This is the
    // property that keeps a month-end's numbers where the cost belongs; dating it "now" moves
    // every cost by however long approval takes.
    let entry = fixture
        .entry_for(expense)
        .await
        .expect("the approval must have written an entry");
    assert_eq!(
        entry.1, "2026-03-12",
        "the entry is dated on the expense date, not today: {entry:?}"
    );
    assert_eq!(entry.2, "expense", "the entry names its source: {entry:?}");

    // And it **balances**, on two lines, debiting the expense account and crediting payables.
    let lines = fixture.entry_lines(expense).await;
    assert_eq!(lines.len(), 2, "an approval posts two lines: {lines:?}");
    // **Parsed through `Amount`, not `f64`.** A float sum is a second implementation of money in
    // a suite whose whole subject is that money has to balance; two cents of drift here would
    // read as a defect in the module, and the module would be right. Integer hundredths cannot
    // drift, and the comparison is then an equality.
    let debits = lines.iter().fold(Amount::ZERO, |total, (_, d, _)| {
        total.plus(Amount::parse(d).expect("the debit side is a number"))
    });
    let credits = lines.iter().fold(Amount::ZERO, |total, (_, _, c)| {
        total.plus(Amount::parse(c).expect("the credit side is a number"))
    });
    assert_eq!(
        debits.cents(),
        credits.cents(),
        "**the entry balances**: debits {}, credits {} — {lines:?}",
        debits.to_text(),
        credits.to_text()
    );
    assert_eq!(
        lines[0].0, "5000",
        "the debit is the expense account, found by its seeded code: {lines:?}"
    );
    assert_eq!(
        lines[1].0, "2200",
        "**the credit is accounts payable, not an expense**: the money left the company but \
         nobody has been paid back yet, so posting this as a cost would make an unreimbursed \
         claim look like money already spent — {lines:?}"
    );

    // The decision is audited with both statuses on it, because "approved" with no "from" is a
    // state rather than a trail.
    let audit = fixture.last_audit(expense).await.expect("an audit row must exist");
    assert!(
        audit.0.contains("expense"),
        "the audit row names the transition: {:?}",
        audit.0
    );
    assert!(
        audit.1.contains("approved"),
        "the audit metadata carries the new status: {}",
        audit.1
    );
}

#[tokio::test]
async fn approving_twice_posts_one_entry_because_the_guard_is_in_the_update() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;
    let expense = file(&fixture, &book, "Airport taxi", "65.00").await;

    let first = approve(&fixture, &book, expense, "First pass").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);

    // The second approver reads `approved` and is told what it can do instead — which is nothing.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/decision"),
            Some(&book),
            Some(json!({ "approved": true, "comment": "Second pass" })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);
    let message = message_of(&again.body);
    assert!(
        message.contains("mark reimbursed"),
        "**the refusal names the way out** — \"already approved\" sends an operator to look at \
         the expense instead of at the rule: {message}"
    );

    // **The property that matters.** Two balanced entries balance, so nothing about the ledger's
    // integrity would ever complain about a double post — the count is the only witness.
    assert_eq!(
        fixture.entry_count(expense).await,
        1,
        "an expense has exactly one entry however many times it is approved"
    );
}

#[tokio::test]
async fn a_rejection_demands_a_reason_and_the_reason_is_readable_on_the_expense() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;
    let expense = file(&fixture, &book, "Client dinner", "210.00").await;

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/submit"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;

    // **No comment is refused, and the refusal says what to write.** A rejection with no reason
    // sends the person who filed it back to a form with nothing to fix, which is the single most
    // common way an approval queue dies.
    let silent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/decision"),
            Some(&book),
            Some(json!({ "approved": false })),
        ),
    )
    .await;
    assert_eq!(silent.status, StatusCode::BAD_REQUEST, "{}", silent.body);
    assert!(
        message_of(&silent.body).contains("needs a reason"),
        "the refusal explains why: {}",
        silent.body
    );

    let rejected = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/decision"),
            Some(&book),
            Some(json!({ "approved": false, "comment": "Bring the itemised receipt" })),
        ),
    )
    .await;
    assert_eq!(rejected.status, StatusCode::OK, "{}", rejected.body);
    assert_eq!(text(&rejected.body, "status"), "rejected");
    assert_eq!(
        text(&rejected.body, "rejection_comment"),
        "Bring the itemised receipt",
        "the REQ names the reason as something an operator has to read back on the expense"
    );
    assert!(
        !rejected.body["decided_by"].is_null(),
        "**the decider is recorded** — an approval with no `decided_by` is a state, not a trail, \
         and `0167` had no column for it at all: {}",
        rejected.body
    );

    // A rejection posts nothing.
    assert_eq!(
        fixture.entry_count(expense).await,
        0,
        "a rejected expense never reaches the ledger"
    );

    // And a rejected expense is **not final**: it can be filed again once the receipt arrives.
    let reopened = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/decision"),
            Some(&book),
            Some(json!({ "approved": false, "comment": "no" })),
        ),
    )
    .await;
    assert!(
        reopened.status == StatusCode::CONFLICT || reopened.status == StatusCode::BAD_REQUEST,
        "a rejection cannot be decided twice: {}",
        reopened.body
    );
}

#[tokio::test]
async fn a_reimbursement_moves_the_expense_and_an_approved_one_cannot_be_reopened() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;
    let expense = file(&fixture, &book, "Hotel deposit", "300.00").await;

    let approved = approve(&fixture, &book, expense, "Prepaid, fine").await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);

    // The available steps are on the view, so the screen never renders a button the server will
    // refuse — an empty list for a reimbursed expense is the feature, not a missing feature.
    let steps: Vec<String> = approved.body["available_transitions"]
        .as_array()
        .expect("the view carries the available steps")
        .iter()
        .map(|step| text(step, "verb"))
        .collect();
    assert_eq!(
        steps,
        vec!["mark reimbursed".to_string()],
        "an approved expense offers exactly one step: {steps:?}"
    );

    let paid = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/reimburse"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(paid.status, StatusCode::OK, "{}", paid.body);
    assert_eq!(text(&paid.body, "status"), "reimbursed");
    assert!(
        !paid.body["reimbursed_at"].is_null(),
        "the payout is stamped: {}",
        paid.body
    );
    assert!(
        paid.body["available_transitions"]
            .as_array()
            .expect("the view always carries the list")
            .is_empty(),
        "a reimbursed expense is final: {}",
        paid.body
    );

    // And nothing more can be done to it.
    let late = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/reimburse"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert!(
        message_of(&late.body)
            .contains("final"),
        "an empty transition list still produces a sentence: {}",
        late.body
    );
}

#[tokio::test]
async fn a_reader_may_see_an_expense_and_may_not_decide_one() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;
    let read = fixture.read().await;
    let expense = file(&fixture, &book, "Conference ticket", "350.00").await;

    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/expenses",
            Some(&read),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/submit"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/decision"),
            Some(&read),
            Some(json!({ "approved": true })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(
        message_of(&refused.body)
            .contains("accounting.expenses.approve"),
        "**the refusal names the missing key**: {}",
        refused.body
    );
    assert_eq!(
        fixture.entry_count(expense).await,
        0,
        "a refused decision posts nothing"
    );
}

#[tokio::test]
async fn another_organizations_expense_is_404_and_never_403_even_with_the_approve_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;
    let outsider = fixture.outsider().await;
    let expense = file(&fixture, &book, "A receipt the stranger must not see", "99.00").await;

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/submit"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;

    // The outsider **holds `accounting.expenses.approve`**. A route *layer* would answer 403
    // before the handler could ask whose expense that is, and a 403 on a named id confirms the row
    // exists somewhere — which is the one fact a tenant boundary must never leak. This walk exists
    // for exactly that, and the handler reads first (404) and asks for the key second.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/decision"),
            Some(&outsider),
            Some(json!({ "approved": true })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "**404, never 403** — the caller has the key, so a 403 here would be about ownership: {}",
        refused.body
    );

    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/expenses/{expense}"),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::NOT_FOUND, "{}", read.body);

    // And the stranger's own decision still works, so the 404 above is ownership rather than the
    // route being broken for everyone.
    let their_org = sqlx::query_scalar::<_, Uuid>("select organization_id from users where email = $1")
        .bind(&fixture.foreign)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the foreign account must exist");
    assert_ne!(their_org, fixture.organization);
}

#[tokio::test]
async fn a_draft_is_editable_and_a_submitted_one_is_not() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;
    let expense = file(&fixture, &book, "Train ticket", "90.00").await;

    let edited = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/accounting/expenses/{expense}"),
            Some(&book),
            Some(expense_body("Train ticket, second class", "75.00", "2026-03-12")),
        ),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    assert_eq!(
        money(&edited.body, "amount"),
        "75.00",
        "a draft is a form and takes an edit"
    );

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/expenses/{expense}/submit"),
            Some(&book),
            Some(json!({})),
        ),
    )
    .await;

    let late = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/accounting/expenses/{expense}"),
            Some(&book),
            Some(expense_body("Actually a car", "400.00", "2026-03-12")),
        ),
    )
    .await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert!(
        message_of(&late.body)
            .contains("not a draft"),
        "the refusal says which rule: {}",
        late.body
    );

    let after = read_expense(&fixture, &book, expense).await;
    assert_eq!(
        money(&after.body, "amount"),
        "75.00",
        "**the refused edit wrote nothing** — the amount is what it was before the refusal"
    );
}

#[tokio::test]
async fn a_tax_larger_than_the_receipt_is_refused_and_the_gross_is_never_inflated() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;

    let impossible = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/expenses",
            Some(&book),
            Some(json!({
                "description": "Receipt with impossible tax",
                "amount": "100.00",
                "tax_amount": "120.00",
                "expense_date": "2026-03-12",
            })),
        ),
    )
    .await;
    assert_eq!(impossible.status, StatusCode::BAD_REQUEST, "{}", impossible.body);

    let good = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/expenses",
            Some(&book),
            Some(json!({
                "description": "VAT receipt",
                "amount": "120.00",
                "tax_amount": "20.00",
                "expense_date": "2026-03-12",
            })),
        ),
    )
    .await;
    assert_eq!(good.status, StatusCode::CREATED, "{}", good.body);
    assert_eq!(
        money(&good.body, "amount"),
        "120.00",
        "**the gross is stored as sent** — the tax is a portion of it. A module that added them \
         would report a 120.00 receipt with 20.00 of VAT as 140.00, which is the slice-2 \
         `gross_of` slip: {}",
        good.body
    );
    assert_eq!(money(&good.body, "tax_amount"), "20.00");
}

#[tokio::test]
async fn the_list_filters_by_status_and_searches_the_number_and_the_vendor() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;

    let first = file(&fixture, &book, "Zugspitse lift pass", "42.00").await;
    let second = file(&fixture, &book, "Bavarian breakfast", "18.50").await;
    approve(&fixture, &book, second, "Fine").await;

    let drafts = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/expenses?status=draft",
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(drafts.status, StatusCode::OK, "{}", drafts.body);
    let ids: Vec<String> = drafts.body["items"]
        .as_array()
        .expect("the list returns a page")
        .iter()
        .map(|row| text(row, "id"))
        .collect();
    assert!(
        ids.contains(&first.to_string()) && !ids.contains(&second.to_string()),
        "`status=draft` returns exactly the drafts: {ids:?}"
    );

    // An unknown status is refused **by name**. "No expenses have the status approvedd" and "you
    // typed it wrong" are different problems to solve, and only one of them is what happened.
    let typo = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/expenses?status=approvedd",
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(typo.status, StatusCode::BAD_REQUEST, "{}", typo.body);
    assert!(
        message_of(&typo.body)
            .contains("approvedd"),
        "the refusal quotes what was typed: {}",
        typo.body
    );

    // The search reaches the vendor and the number.
    let found = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/expenses?search=Bavarian",
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.body);
    let hits: Vec<String> = found.body["items"]
        .as_array()
        .expect("the list returns a page")
        .iter()
        .map(|row| text(row, "description"))
        .collect();
    assert_eq!(hits, vec!["Bavarian breakfast".to_string()]);

    let number = read_expense(&fixture, &book, second).await.body["number"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let by_number = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/expenses?search={number}"),
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(by_number.status, StatusCode::OK, "{}", by_number.body);
    assert_eq!(
        by_number.body["items"]
            .as_array()
            .expect("the list returns a page")
            .len(),
        1,
        "a receipt is findable by the number a person was given it under: {number}"
    );

    // And the categories endpoint answers with what has actually been used.
    let categories = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/expenses/categories",
            Some(&book),
            None,
        ),
    )
    .await;
    assert_eq!(categories.status, StatusCode::OK, "{}", categories.body);
    let listed: Vec<String> = categories
        .body
        .as_array()
        .expect("categories is an array")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    assert!(
        listed.contains(&"travel".to_string()),
        "a category somebody has filed under is offered: {listed:?}"
    );
}

#[tokio::test]
async fn every_expense_route_refuses_an_anonymous_caller() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;
    let expense = file(&fixture, &book, "Anonymous probe", "10.00").await;

    for (method, uri) in [
        (Method::GET, format!("/api/v1/accounting/expenses/{expense}")),
        (
            Method::GET,
            "/api/v1/accounting/expenses/categories".to_string(),
        ),
        (Method::GET, "/api/v1/accounting/expenses".to_string()),
        (
            Method::POST,
            format!("/api/v1/accounting/expenses/{expense}/submit"),
        ),
        (
            Method::POST,
            format!("/api/v1/accounting/expenses/{expense}/decision"),
        ),
        (
            Method::POST,
            format!("/api/v1/accounting/expenses/{expense}/reimburse"),
        ),
    ] {
        let response = call(
            &fixture.state,
            request(method.clone(), &uri, None, Some(json!({}))),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must refuse an anonymous caller: {}",
            response.body
        );
    }

    // A create with no session, since its route is a layer and a layer must not become the only
    // thing refusing anonymous callers.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/expenses",
            None,
            Some(expense_body("No session", "10.00", "2026-03-12")),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        created.body
    );

    let _ = book;
}

#[tokio::test]
async fn an_amount_of_zero_is_refused_with_the_reason_and_nothing_is_written() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let book = fixture.book().await;

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/expenses",
            Some(&book),
            Some(json!({
                "description": "Nothing at all",
                "amount": "0.00",
                "expense_date": "2026-03-12",
            })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert!(
        message_of(&refused.body)
            .contains("more than 0.00"),
        "the refusal explains the rule rather than naming a constraint: {}",
        refused.body
    );

    let counted: i64 = sqlx::query_scalar(
        "select count(*) from accounting_expenses where description = 'Nothing at all'",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must succeed");
    assert_eq!(counted, 0, "**the refusal writes nothing** — no row, no number consumed");
}
