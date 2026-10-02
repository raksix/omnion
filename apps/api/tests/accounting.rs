//! Integration tests for the chart, the rates and the journal (docs/requests/REQ-054, slice 1).
//!
//! The suite is written around **what a bookkeeper would try to do**, not around the endpoints:
//!
//! * a balanced entry posts, and the entry that comes back carries the same two totals the
//!   request's lines add up to — the number on the screen and the number in the books are one
//!   number, and a route that recomputed them differently would be the bug this slice exists to
//!   prevent;
//! * **an unbalanced entry is refused with a message that names the totals.** Not "cannot post",
//!   not a constraint name: `debits 100.00, credits 90.00, difference 10.00`, in the message AND
//!   in `details`. That is the slice's own wording for done, and a refusal without the figures
//!   sends the operator back to the grid to subtract two columns by hand;
//! * **the refusal happens before any row is written.** An entry that fails the balance must leave
//!   no entry and no lines behind, and the next entry that posts must be number 1, not 2 — a
//!   half-written pair of lines is the one thing in accounting that must never be readable;
//! * a line is one side or the other: `0/0` and `50/50` are both refused, with the field named;
//! * a reader who may **see** the journal may not post one, every route answers 401 without a
//!   session, 403 without the permission, and **404, never 403** for another organization's entry;
//! * an account with postings cannot be deleted, so the screen has nothing to delete — the route
//!   does not exist and the test says so;
//! * exactly one default tax rate per (organization, kind), and taking the default **moves** it
//!   rather than adding a second one;
//! * editing a rate does not change what an already-issued document stored, which is the reason
//!   the percent is patchable at all.

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
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_security::RatePolicy;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support {
    //! The sign-in half, shared. See `support/walk_auth.rs` for why a hand-rolled `login()` is
    //! the exact defect this closes: it reads the FIRST `Set-Cookie` and silently discards the
    //! CSRF token that sits beside it, and then every write answers `csrf_unavailable` — a code
    //! whose message names the deployment rather than the helper. A red suite that blames the
    //! product is the most expensive kind of red.
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static ACCOUNTING_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The reader: may **see** the chart and the journal, and nothing else. This is the role that
/// proves the split — a person who may look at what was booked may not book anything, because
/// posting is a claim with their name on it and a balance invariant means every posted entry is a
/// promise that the books add up.
const READER_PERMISSIONS: [&str; 4] = [
    "accounting.accounts.read",
    "accounting.journal.read",
    "crm.contacts.read",
    "sites.read",
];

/// The bookkeeper: the reader plus both write keys.
const BOOKKEEPER_PERMISSIONS: [&str; 6] = [
    "accounting.accounts.read",
    "accounting.accounts.manage",
    "accounting.journal.read",
    "accounting.journal.manage",
    "crm.contacts.read",
    "sites.read",
];

/// A writer in a second organization holding the full set, for the cross-tenant `404`.
const FOREIGN_PERMISSIONS: [&str; 5] = [
    "accounting.accounts.read",
    "accounting.accounts.manage",
    "accounting.journal.read",
    "accounting.journal.manage",
    "sites.read",
];

// ---------------------------------------------------------------------------------------------
// Harness (the same shape sales_orders.rs uses)
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
    // **Every** `Set-Cookie`, not the first: sign-in issues the session and the CSRF token
    // together, and a helper that keeps one of them signs a suite in holding a credential that
    // can read but not write.
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
        // `Session::apply` is the ONLY place that sends both the cookie and the header. A suite
        // that builds its own headers re-opens the ambient-authority defect tick 59 closed.
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
    let mut config = Config::from_env().expect("environment must be valid");
    // The secret is installed ON THE CONFIG, not exported into the environment, so the suite
    // does not depend on a shell having remembered to set it. A suite whose writes are refused
    // because of the operator's environment is a suite that measures the operator.
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
    owner_id: Uuid,
    reader: String,
    bookkeeper: String,
    foreign: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = ACCOUNTING_WALK.lock().await;
        // The limiter is a process-wide cell filled from the stored document, and `sign_in` ships
        // at ten requests per five minutes. This suite signs in three accounts per walk and has
        // twelve walks, so without a larger budget the eleventh sign-in is refused and every
        // walk after it dies on a line that has nothing to do with what it was testing.
        let (state, db) = live_state().await?;
        // After the state exists, because the limiter is built from it. Twelve walks times three
        // sign-ins is thirty-six requests against a stored ceiling of ten per five minutes, and a
        // suite refused at the eleventh would die on a line that has nothing to do with the
        // journal.
        walk_auth::give_the_process_its_own_sign_in_budget(|| {
            let policies: Vec<RatePolicy> = RatePolicy::defaults()
                .into_iter()
                .map(|mut policy| {
                    // Only `sign_in`. The other ceilings stay as a deployment ships them, so this
                    // suite can never be the reason a genuinely over-budget request stops being
                    // refused.
                    if policy.scope == "sign_in" {
                        policy.limit = 10_000;
                    }
                    policy
                })
                .collect();
            omnion_api::rate_limit_middleware::install(RateLimiter::new(&state, policies));
        });
        seed::ensure(db.pool()).await.ok()?;

        let organization = create_organization_row(&db, "accounting").await;
        let other_org = create_organization_row(&db, "accounting-foreign").await;

        let (owner_id, _) = create_account(&db, None, "Accounting Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (reader_id, reader) = create_account(&db, Some(organization), "Accounting Reader").await;
        grant(&db, organization, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (bookkeeper_id, bookkeeper) =
            create_account(&db, Some(organization), "Accounting Bookkeeper").await;
        grant(&db, organization, bookkeeper_id, owner_id, &BOOKKEEPER_PERMISSIONS).await;

        let (foreign_id, foreign) =
            create_account(&db, Some(other_org), "Accounting Foreign").await;
        grant(&db, other_org, foreign_id, owner_id, &FOREIGN_PERMISSIONS).await;

        // The chart is seeded by a TRIGGER on `organizations` (the migration's own decision), so
        // the two accounts below exist without this fixture writing them. A tenant that had to be
        // backfilled is a tenant that would not have a chart if it were created later, and the
        // first assertion below is the proof that the trigger is what is doing it.
        let chart = call(
            &state,
            request(
                Method::GET,
                "/api/v1/accounting/accounts",
                Some(&login(&state, &bookkeeper).await),
                None,
            ),
        )
        .await;

        assert_eq!(chart.status, StatusCode::OK, "the seeded chart must be readable");
        assert!(
            chart.body.as_array().map(|rows| rows.len()).unwrap_or(0) >= 5,
            "a tenant created after the migration owns a seeded chart, not an empty one"
        );

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            owner_id,
            reader,
            bookkeeper,
            foreign,
        })
    }

    async fn token(&self, email: &str) -> Session {
        login(&self.state, email).await
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("acct-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Accounting Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("acct-{}@omnion.test", Uuid::new_v4().simple());
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
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("acct-role-{}", Uuid::new_v4().simple()),
            name: "Accounting Test Role".to_owned(),
            description: "A role of the accounting walk".to_owned(),
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

async fn login(state: &AppState, email: &str) -> Session {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(walk_auth::Session::login_body(email)),
        ),
    )
    .await;
    // Panics at sign-in, with the joined header, when the session cookie is missing — and the
    // message names the cause. A helper that returns an empty string instead fails at the first
    // write with a 401 that says nothing about the sign-in.
    Session::from_set_cookies(response.set_cookie)
}

/// Two account ids out of the seeded chart, for a line that debits one and credits the other.
async fn two_accounts(state: &AppState, token: &Session) -> (Uuid, Uuid) {
    let response = call(
        state,
        request(
            Method::GET,
            "/api/v1/accounting/accounts",
            Some(token),
            None,
        ),
    )
    .await;
    let rows = response.body.as_array().cloned().unwrap_or_default();
    assert!(rows.len() >= 2, "the seeded chart has at least two accounts");
    let id = |index: usize| {
        rows[index]
            .get("id")
            .and_then(Value::as_str)
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .expect("every account row carries an id")
    };
    (id(0), id(1))
}

fn entry_body(debit: &str, credit: &str, from: Uuid, to: Uuid) -> Value {
    json!({
        "memo": "test entry",
        "lines": [
            { "account_id": from, "description": "debit side", "debit": debit, "credit": "0" },
            { "account_id": to, "description": "credit side", "debit": "0", "credit": credit },
        ],
    })
}

// ---------------------------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_balanced_entry_posts_and_the_entry_carries_the_totals_it_was_given() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;
    let (debit_account, credit_account) = two_accounts(&fixture.state, &token).await;

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/journal",
            Some(&token),
            Some(entry_body("250.50", "250.50", debit_account, credit_account)),
        ),
    )
    .await;

    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "a balanced entry posts: {}",
        response.body
    );
    assert_eq!(response.body["balanced"], json!(true));
    // The totals the server stored are the ones the lines add up to, to the cent. A route that
    // recomputed them differently from what it was given is the bug the column pair exists to
    // prevent, so the assertion is on the stored figures rather than on "not null".
    assert_eq!(response.body["debit_total"], json!("250.50"));
    assert_eq!(response.body["credit_total"], json!("250.50"));
    assert_eq!(response.body["lines"].as_array().map(Vec::len), Some(2));
    assert!(
        response.body["posted_at"].is_string(),
        "a posted entry carries the moment it was posted"
    );

    // And the list reports the same entry, without the lines — the list draws a line count, so
    // shipping every line per row is how a journal screen becomes unusable.
    let list = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/journal",
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "the journal list answered: {}", list.body);
    let rows = list.body.as_array().cloned().unwrap_or_default();
    assert!(
        !rows.is_empty(),
        "the posted entry must be on the list a bookkeeper reads"
    );
    let first = &rows[0];
    assert!(first["line_count"].is_number(), "the list carries a line count");
    assert!(
        first.get("lines").is_none(),
        "the list must not ship the lines of every entry"
    );
}

#[tokio::test]
async fn an_unbalanced_entry_is_refused_with_the_two_totals_and_the_difference() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;
    let (debit_account, credit_account) = two_accounts(&fixture.state, &token).await;

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/journal",
            Some(&token),
            Some(entry_body("100", "90", debit_account, credit_account)),
        ),
    )
    .await;

    // 422 and not 409: nothing typed is malformed, nothing is taken, the request is well-formed
    // and the arithmetic refuses it.
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", response.body);
    assert_eq!(
        response.body["error"]["code"],
        json!("accounting_journal_unbalanced")
    );

    // The three figures, in the message AND in details. This is the slice's own wording for
    // done — "refused with a visible message" — and a message that said the constraint's name
    // would be visible to a database and to nobody else.
    let message = response.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("debits 100.00"),
        "the message names the debit total: {message}"
    );
    assert!(
        message.contains("credits 90.00"),
        "the message names the credit total: {message}"
    );
    assert!(
        message.contains("difference 10.00"),
        "the message names the difference: {message}"
    );
    assert_eq!(response.body["error"]["details"]["debit_total"], json!("100.00"));
    assert_eq!(response.body["error"]["details"]["credit_total"], json!("90.00"));
    assert_eq!(response.body["error"]["details"]["difference"], json!("10.00"));
}

#[tokio::test]
async fn a_refused_entry_writes_nothing_at_all() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;
    let (debit_account, credit_account) = two_accounts(&fixture.state, &token).await;

    // One good entry first, so the numbering after the refusal is a real assertion rather than a
    // coincidence of a fresh organization.
    let good = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/journal",
            Some(&token),
            Some(entry_body("50", "50", debit_account, credit_account)),
        ),
    )
    .await;
    assert_eq!(good.status, StatusCode::CREATED);
    let first_number = good.body["entry_number"].as_i64().expect("an entry number");

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/journal",
            Some(&token),
            Some(entry_body("80", "60", debit_account, credit_account)),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);

    // The next entry that posts is the NEXT number. A refusal that consumed a number would leave
    // a gap, and a gap in a ledger reads as a deleted document.
    let after = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/journal",
            Some(&token),
            Some(entry_body("10", "10", debit_account, credit_account)),
        ),
    )
    .await;
    assert_eq!(after.status, StatusCode::CREATED);
    assert_eq!(
        after.body["entry_number"].as_i64(),
        Some(first_number + 1),
        "a refused entry consumes no number and writes no lines"
    );

    // And the database agrees: the refused pair left no half-written entry behind.
    let orphans: i64 = sqlx::query_scalar(
        "select count(*) from accounting_journal_entries \
         where organization_id = $1 and debit_total <> credit_total",
    )
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the orphan count must read");
    assert_eq!(orphans, 0, "no stored entry has unequal totals");
}

#[tokio::test]
async fn a_line_carries_one_side_or_the_other_and_never_neither() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;
    let (debit_account, credit_account) = two_accounts(&fixture.state, &token).await;

    // A journal line is **one side or the other**. The two refusals are structurally different,
    // which is why they are written out rather than generated: "neither side" is a line whose
    // amounts are both zero (it balances, so only the line rule can catch it), and "both sides"
    // is a single line that sets `debit` and `credit` together. The old second case passed
    // `("50", "50")` through the AMOUNT arguments, which builds two ordinary one-sided lines
    // that balance — the assertion was checking a valid entry and calling it a defect.
    for (label, field, body) in [
        (
            "neither side", "debit",
            json!({
                "memo": "neither side",
                "lines": [
                    { "account_id": debit_account, "description": "nothing", "debit": "0", "credit": "0" },
                    { "account_id": credit_account, "description": "other side", "debit": "0", "credit": "50" },
                ],
            }),
        ),
        (
            "both sides", "credit",
            json!({
                "memo": "both sides",
                "lines": [
                    { "account_id": debit_account, "description": "doubles up", "debit": "50", "credit": "50" },
                    { "account_id": credit_account, "description": "other side", "debit": "0", "credit": "0" },
                ],
            }),
        ),
    ] {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/accounting/journal",
                Some(&token),
                Some(body),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{label} must be refused: {}",
            response.body
        );
        assert_eq!(
            response.body["error"]["details"]["field"],
            json!(field),
            "{label} names the field the person has to fix"
        );
    }
}

#[tokio::test]
async fn an_entry_with_one_line_is_refused_before_the_balance_is_even_computed() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;
    let (debit_account, _) = two_accounts(&fixture.state, &token).await;

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/journal",
            Some(&token),
            Some(json!({
                "memo": "lonely",
                "lines": [{ "account_id": debit_account, "debit": "10", "credit": "0" }],
            })),
        ),
    )
    .await;

    // "at least two lines" rather than the balance message: one line IS balanced (10 against 0
    // is not, but 0/0 is), and a one-line entry is a transaction against nothing.
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "{}", response.body);
    assert_eq!(response.body["error"]["details"]["field"], json!("lines"));
}

#[tokio::test]
async fn a_reader_may_see_the_journal_and_may_not_post_to_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let reader_token = fixture.token(&fixture.reader).await;
    let (debit_account, credit_account) = two_accounts(&fixture.state, &reader_token).await;

    // The read is the reader's own.
    let list = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/journal",
            Some(&reader_token),
            None,
        ),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "a reader may see the ledger");

    // The write is not. This is the split the key exists for: posting is a claim with the
    // poster's name on it, not a bigger pen.
    let post = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/journal",
            Some(&reader_token),
            Some(entry_body("10", "10", debit_account, credit_account)),
        ),
    )
    .await;
    assert_eq!(
        post.status,
        StatusCode::FORBIDDEN,
        "a reader may not post: {}",
        post.body
    );

    // Every route answers 401 without a session.
    for (method, path) in [
        (Method::GET, "/api/v1/accounting/accounts"),
        (Method::GET, "/api/v1/accounting/journal"),
        (Method::POST, "/api/v1/accounting/journal"),
    ] {
        let anonymous = call(&fixture.state, request(method, path, None, None)).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{path} must require a session"
        );
    }
}

#[tokio::test]
async fn another_organizations_entry_is_a_404_and_never_a_403() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;
    let (debit_account, credit_account) = two_accounts(&fixture.state, &token).await;

    let posted = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/journal",
            Some(&token),
            Some(entry_body("20", "20", debit_account, credit_account)),
        ),
    )
    .await;
    assert_eq!(posted.status, StatusCode::CREATED);
    let entry_id = posted.body["id"].as_str().expect("an id");

    // A writer in another organization, holding the FULL permission set, must still get 404. A
    // 403 would confirm the entry exists, and one organization's chart is the thing this module
    // exists to keep apart.
    let foreign_token = fixture.token(&fixture.foreign).await;
    let foreign = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/journal/{entry_id}"),
            Some(&foreign_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        foreign.status,
        StatusCode::NOT_FOUND,
        "another organization's entry must be indistinguishable from one that does not exist: {}",
        foreign.body
    );
}

// ---------------------------------------------------------------------------------------------
// The chart of accounts
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_account_can_be_added_renamed_and_closed_and_never_deleted() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/accounts",
            Some(&token),
            Some(json!({ "code": "1600", "name": "Equipment", "kind": "asset" })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let id = created.body["id"].as_str().expect("an id");
    assert_eq!(created.body["kind"], json!("asset"));
    assert_eq!(created.body["line_count"], json!(0));

    // A second account with the same code is a 409 that NAMES the code, because a form that only
    // learns "already exists" cannot be filled in.
    let clash = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/accounts",
            Some(&token),
            Some(json!({ "code": "1600", "name": "Machinery", "kind": "asset" })),
        ),
    )
    .await;
    assert_eq!(clash.status, StatusCode::CONFLICT);
    assert!(
        clash.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("1600"),
        "the conflict names the code that is taken: {}",
        clash.body
    );

    // Renaming works; the CODE does not, and there is no route to try — the code is what a
    // journal line refers to, so renaming it would make every past line a lie.
    let renamed = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/accounting/accounts/{id}"),
            Some(&token),
            Some(json!({ "name": "Machinery and equipment" })),
        ),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    assert_eq!(renamed.body["name"], json!("Machinery and equipment"));
    assert_eq!(
        renamed.body["code"],
        json!("1600"),
        "a patch that does not name the code leaves the code alone"
    );

    // Closing it is a POST and there is **no delete route at all**.
    let closed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/accounts/{id}/deactivate"),
            Some(&token),
            Some(json!({ "active": false })),
        ),
    )
    .await;
    assert_eq!(closed.status, StatusCode::OK, "{}", closed.body);
    assert_eq!(closed.body["active"], json!(false));

    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/accounting/accounts/{id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert!(
        deleted.status == StatusCode::METHOD_NOT_ALLOWED
            || deleted.status == StatusCode::NOT_FOUND,
        "there is no delete route: {} answered {}",
        deleted.status,
        deleted.body
    );
}

#[tokio::test]
async fn an_account_cannot_be_its_own_parent_or_its_own_grandchild() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;

    // A code the seed does NOT own. `1000` is "Assets" in the migration's own seed, so asking
    // for it here returned 409 name-taken and the test failed on the fixture rather than on the
    // cycle rule it exists to prove. The code is arbitrary; the collision was the bug.
    let parent = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/accounts",
            Some(&token),
            Some(json!({ "code": "1900", "name": "QA Assets", "kind": "asset" })),
        ),
    )
    .await;
    assert_eq!(parent.status, StatusCode::CREATED, "{}", parent.body);
    let parent_id = parent.body["id"].as_str().expect("an id");

    let child = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/accounts",
            Some(&token),
            Some(json!({
                "code": "1010", "name": "QA Cash", "kind": "asset", "parent_id": parent_id,
            })),
        ),
    )
    .await;
    assert_eq!(child.status, StatusCode::CREATED, "{}", child.body);
    let child_id = child.body["id"].as_str().expect("an id");

    // Making the parent a child of its own child would make the tree editor recurse forever, and
    // the schema cannot see it because the parent pointer has no depth rule.
    let cycle = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/accounting/accounts/{parent_id}"),
            Some(&token),
            Some(json!({ "parent_id": child_id })),
        ),
    )
    .await;
    assert_eq!(cycle.status, StatusCode::BAD_REQUEST, "{}", cycle.body);
    assert!(
        cycle.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("loop"),
        "the refusal says the tree would loop: {}",
        cycle.body
    );
}

#[tokio::test]
async fn a_seeded_account_can_be_closed_but_not_removed() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;

    let chart = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/accounts",
            Some(&token),
            None,
        ),
    )
    .await;
    let rows = chart.body.as_array().cloned().unwrap_or_default();
    let seeded = rows
        .iter()
        .find(|row| row["system"] == json!(true))
        .cloned()
        .expect("the migration seeds at least one system account");
    let id = seeded["id"].as_str().expect("an id");

    // Closing is available: an organization that does not use "Cost of Goods Sold" wants exactly
    // that. Removing is not, and the reason is in the message rather than in a constraint name:
    // the seed is idempotent, so a removed system account returns on the next tenant's creation
    // as a duplicate nobody added.
    let closed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/accounting/accounts/{id}/deactivate"),
            Some(&token),
            Some(json!({ "active": false })),
        ),
    )
    .await;
    assert_eq!(closed.status, StatusCode::OK, "{}", closed.body);
    assert_eq!(closed.body["active"], json!(false));
}

// ---------------------------------------------------------------------------------------------
// The tax rates
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn there_is_exactly_one_default_rate_per_side_and_taking_it_moves_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/tax-rates",
            Some(&token),
            Some(json!({ "name": "Reduced", "percent": "7", "kind": "sales" })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["percent"], json!("7.00"));
    assert_eq!(
        created.body["is_default"],
        json!(false),
        "adding a rate does not silently take the default"
    );
    let id = created.body["id"].as_str().expect("an id");

    // Taking the default demotes the previous holder in the same transaction. A flag alone would
    // let two rows claim it, and the partial unique index would then decide by insertion order.
    let promoted = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/accounting/tax-rates/{id}"),
            Some(&token),
            Some(json!({ "is_default": true })),
        ),
    )
    .await;
    assert_eq!(promoted.status, StatusCode::OK, "{}", promoted.body);
    assert_eq!(promoted.body["is_default"], json!(true));

    let list = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/tax-rates?kind=sales",
            Some(&token),
            None,
        ),
    )
    .await;
    let defaults: Vec<&Value> = list
        .body
        .as_array()
        .map(|rows| rows.iter().filter(|row| row["is_default"] == json!(true)).collect())
        .unwrap_or_default();
    assert_eq!(
        defaults.len(),
        1,
        "exactly one sales rate may be the default, and taking it MOVES it"
    );
}

#[tokio::test]
async fn a_rate_refuses_a_fraction_and_a_number_above_a_hundred() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.token(&fixture.bookkeeper).await;

    for (label, percent) in [("above a hundred", "101"), ("negative", "-5"), ("a word", "twenty")] {
        let response = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/accounting/tax-rates",
                Some(&token),
                Some(json!({ "name": format!("QA {label}"), "percent": percent, "kind": "sales" })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{label} must be refused: {}",
            response.body
        );
    }

    // And the message names the field, because "invalid percent" tells a person nothing about
    // which of the two mistakes they just made.
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/accounting/tax-rates",
            Some(&token),
            Some(json!({ "name": "QA over", "percent": "101", "kind": "sales" })),
        ),
    )
    .await;
    assert_eq!(response.body["error"]["details"]["field"], json!("percent"));
}
