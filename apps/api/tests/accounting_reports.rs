//! Reports: the four read-only questions and the export beside them (REQ-054, slice 4b).
//!
//! These walks exist for the two acceptance boxes that are **identities rather than examples**:
//!
//! * *"aging buckets (0–30/31–60/61–90/90+) that sum to the outstanding total"* — asserted as
//!   `bucket_sum == outstanding_sum` over the report's own rows, so a bucket table that does not
//!   add up to its own footer fails here rather than in a reader's spreadsheet.
//! * *"CSV exports contain exactly the rows shown in the on-screen table"* — asserted as
//!   `csv_data_rows == report_rows`, counting the CSV's own lines rather than trusting a header.
//!
//! Everything else is the shape a report can be wrong in quietly: a boundary off by one, a
//! not-yet-due invoice filed as 20 days late, a draft counted as a debt, a void invoice still in
//! the receivables, a renamed contact rewriting an invoice already sent, and a tax rate edited
//! after issue restating a filed period.
//!
//! One suite, serialised on a mutex — the organizations and the IAM seed are shared state, and a
//! walk that runs beside another one races the catalogue.

use axum::body::Body;
use axum::http::{Request, StatusCode, header, Method};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::{BuildInfo, Db, RedisClient};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

mod support {
    pub mod walk_auth;
}

use support::walk_auth::{self, Session};

/// Serialises this suite: the organizations and the IAM seed are shared state.
static REPORT_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The reader: may see the three internal reports, and **not** the tax summary.
const READER_PERMISSIONS: [&str; 2] = ["accounting.reports.read", "sites.read"];

/// The tax reader: the section's read plus the filing's own key.
///
/// The split is the point of `accounting.reports.tax` being separate, so it is proved rather
/// than assumed: two roles, same organization, one may read the tax summary and the other may
/// not.
const TAX_READER_PERMISSIONS: [&str; 3] = [
    "accounting.reports.read",
    "accounting.reports.tax",
    "sites.read",
];

/// A writer in a second organization, for the cross-tenant check.
const FOREIGN_PERMISSIONS: [&str; 2] = ["accounting.reports.read", "sites.read"];

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct TestResponse {
    status: StatusCode,
    set_cookie: Vec<String>,
    body: Value,
    /// The raw text, for the CSV export — which is not JSON.
    raw: String,
    headers: Vec<(String, String)>,
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
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let raw = String::from_utf8_lossy(&bytes).to_string();
    // **A body that is not JSON is a string, never a panic** — and the export route answers CSV
    // on the same `call`, so this is load-bearing here rather than defensive.
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::String(raw.clone()))
    };
    TestResponse {
        status,
        set_cookie,
        body,
        raw,
        headers,
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
    owner_id: Uuid,
    reader: String,
    tax_reader: String,
    foreign: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = REPORT_WALK.lock().await;
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

        let organization = create_organization_row(&db, "reports").await;
        let other_org = create_organization_row(&db, "reports-foreign").await;

        let (owner_id, _) = create_account(&db, None, "Report Owner").await;
        omnion_permissions::seed::bind_owner(db.pool(), owner_id)
            .await
            .ok()?;

        let (reader_id, reader) = create_account(&db, Some(organization), "Report Reader").await;
        grant(&db, organization, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (tax_id, tax_reader) = create_account(&db, Some(organization), "Report Tax Reader").await;
        grant(&db, organization, tax_id, owner_id, &TAX_READER_PERMISSIONS).await;

        let (foreign_id, foreign) = create_account(&db, Some(other_org), "Report Foreign").await;
        grant(&db, other_org, foreign_id, owner_id, &FOREIGN_PERMISSIONS).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            owner_id,
            reader,
            tax_reader,
            foreign,
        })
    }

    async fn read(&self) -> Session {
        login(&self.state, &self.reader).await
    }

    async fn tax(&self) -> Session {
        login(&self.state, &self.tax_reader).await
    }

    async fn outsider(&self) -> Session {
        login(&self.state, &self.foreign).await
    }

    /// A sent invoice with a known total and a chosen due date, written straight into the table.
    ///
    /// **Direct SQL, not the API**, and the reason is the test's subject: the report must be
    /// proved against rows whose every field is *known* — the due date in particular, since days
    /// past due decides the bucket and an invoice created "now" through the API has no due date
    /// to place. Going through the API would make the walk assert its own fixture.
    async fn invoice(&self, label: &str, total: &str, paid: &str, due: Option<&str>) -> Uuid {
        let number = format!("RPT-{label}-{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query_scalar::<_, Uuid>(
            "insert into accounting_invoices \
                 (organization_id, number, invoice_status, currency, issue_date, due_date, \
                  subtotal, discount_total, tax_total, grand_total, paid_total, customer_name) \
             values ($1, $2, 'sent', 'USD', current_date, $3::date, $4::numeric, 0, 0, \
                     $4::numeric, $5::numeric, $6) returning id",
        )
        .bind(self.organization)
        .bind(&number)
        .bind(due)
        .bind(total)
        .bind(paid)
        .bind(format!("Customer {label}"))
        .fetch_one(self.db.pool())
        .await
        .expect("the fixture invoice must be written")
    }

    /// An invoice in a status the receivables must not count.
    async fn invoice_with_status(&self, status: &str, total: &str, due: Option<&str>) -> Uuid {
        let number = format!("RPT-{status}-{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query_scalar::<_, Uuid>(
            "insert into accounting_invoices \
                 (organization_id, number, invoice_status, currency, issue_date, due_date, \
                  subtotal, discount_total, tax_total, grand_total, paid_total, customer_name) \
             values ($1, $2, $3, 'USD', current_date, $4::date, $5::numeric, 0, 0, \
                     $5::numeric, 0, 'Excluded') returning id",
        )
        .bind(self.organization)
        .bind(&number)
        .bind(status)
        .bind(due)
        .bind(total)
        .fetch_one(self.db.pool())
        .await
        .expect("the fixture invoice must be written")
    }

    /// A payment against a real sent invoice, so the income and cashflow reports have money in.
    ///
    /// **The invoice is not decoration.** `accounting_payments.invoice_id` is nullable only since
    /// 0175 (slice 3's change, for the transfer that pays several invoices at once), and a
    /// payment with no invoice at all is a case neither report is about. Writing one keeps the
    /// fixture on the ordinary path.
    async fn payment(&self, amount: &str, paid_on: &str) -> Uuid {
        let invoice = self.invoice("PAID", amount, "0.00", Some(paid_on)).await;
        sqlx::query("update accounting_invoices set invoice_status = 'sent' where id = $1")
            .bind(invoice)
            .execute(self.db.pool())
            .await
            .expect("the paid invoice must be sent");
        let reference = format!("RPT-PAY-{}", &Uuid::new_v4().simple().to_string()[..8]);
        // **`payment_number` is a `bigint`, not a text label** -- a per-organization sequence,
        // unique with it. `reference` is the text one (an external bank id, which may be empty).
        // Writing text answers 42804 and writing a fixed number would collide with the unique
        // index as soon as two walks shared an organization, so the number comes from the same
        // sequence the module reads, and the reference carries the walk's own uuid.
        let number: i64 = sqlx::query_scalar(
            "select coalesce(max(payment_number), 0) + 1 from accounting_payments \
             where organization_id = $1",
        )
        .bind(self.organization)
        .fetch_one(self.db.pool())
        .await
        .expect("the payment number must be readable");
        // `number` is NOT NULL and is the payment's own label; `payment_number` is the bigint
        // sequence; `reference` is the external bank id and may be empty. Three similar columns,
        // none of which the others may be used for.
        let label = format!("PAY-{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query_scalar::<_, Uuid>(
            "insert into accounting_payments \
                 (organization_id, invoice_id, payment_number, number, customer_name, reference, \
                  method, amount, paid_on, created_by) \
             values ($1, $2, $3, $4, $5, $6, 'bank_transfer', $7::numeric, $8::date, $9) returning id",
        )
        .bind(self.organization)
        .bind(invoice)
        .bind(number)
        .bind(&label)
        .bind("Report Payer")
        .bind(&reference)
        .bind(amount)
        .bind(paid_on)
        .bind(self.owner_id)
        .fetch_one(self.db.pool())
        .await
        .expect("the fixture payment must be written")
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("rpt-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Report Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    use omnion_identity::users::{self, NewUser};
    let email = format!("rpt-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("rpt-role-{}", Uuid::new_v4().simple()),
            name: "Report Test Role".to_owned(),
            description: "A role of the report walk".to_owned(),
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

/// The message out of an error envelope.
///
/// **Not `body["message"]`.** The envelope is `{"error":{"code","message",...}}`, so a flat read
/// yields `None` and `contains(...)` on it is `false` — which turns "the refusal does not explain
/// itself" into a failure that looks like a product defect when the module behaves perfectly.
fn message_of(body: &Value) -> String {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("the refusal carries a message: {body}"))
        .to_owned()
}

fn money(text: &str) -> omnion_module_accounting::money::Amount {
    omnion_module_accounting::money::Amount::parse(text)
        .unwrap_or_else(|error| panic!("{text} is not an amount: {error}"))
}

/// The sum of a report's rows, read from the JSON rather than from the module.
///
/// **Deliberately independent of `ReportPayload`.** The acceptance box is about what a *reader*
/// sees, so the number is summed the way a spreadsheet would sum it — off the wire.
fn sum_of(rows: &[Value], key: &str) -> String {
    let total = rows.iter().fold(omnion_module_accounting::money::Amount::ZERO, |acc, row| {
        acc.plus(money(row.get(key).and_then(Value::as_str).unwrap_or("0.00")))
    });
    total.to_text()
}

/// The data rows of a CSV: the lines that are not comments, not the header, and not a total.
fn csv_data_rows(csv: &str) -> Vec<&str> {
    csv.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('#'))
        .filter(|line| !line.starts_with("TOTAL"))
        .filter(|line| !line.starts_with("BUCKET:"))
        .filter(|line| !line.starts_with("number,") && !line.starts_with("month,") && !line.starts_with("week_start,") && !line.starts_with("rate_name,"))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_report_answers_for_a_caller_who_may_read_them() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;

    for report in ["income-expense", "aging", "cashflow"] {
        let response = call(
            &fixture.state,
            request(
                Method::GET,
                &format!("/api/v1/accounting/reports/{report}"),
                Some(&session),
                None,
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{report} must answer 200: {}",
            message_of(&response.body)
        );
        // The header is part of the payload, not a convenience: the definition travels with the
        // numbers so the screen and the export cannot describe different windows.
        let meta = &response.body["meta"];
        assert_eq!(meta["kind"], report.replace("aging", "aging"));
        assert!(
            meta["definition"].as_str().is_some_and(|d| !d.is_empty()),
            "{report} must carry its own definition: {meta}"
        );
        assert!(meta["generated_on"].as_str().is_some_and(|d| !d.is_empty()));
    }
}

#[tokio::test]
async fn the_tax_summary_is_refused_without_its_own_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let reader = fixture.read().await;
    let tax_reader = fixture.tax().await;

    // The split is the whole reason `accounting.reports.tax` is a separate key, so it is proved
    // with two roles in ONE organization rather than asserted in a comment.
    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/reports/tax-summary",
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a reader without accounting.reports.tax must be refused: {}",
        refused.body
    );
    assert!(
        message_of(&refused.body).contains("accounting.reports.tax"),
        "the refusal must name the key to ask for: {}",
        message_of(&refused.body)
    );

    let allowed = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/reports/tax-summary",
            Some(&tax_reader),
            None,
        ),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "a reader WITH the key must be served: {}",
        message_of(&allowed.body)
    );
}

#[tokio::test]
async fn an_unknown_report_names_the_four_that_exist() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/reports/profit-and-loss",
            Some(&session),
            None,
        ),
    )
    .await;
    assert!(
        response.status.is_client_error(),
        "an unknown report must be refused, not 404: {}",
        response.status
    );
    let message = message_of(&response.body);
    for name in ["income-expense", "aging", "cashflow", "tax-summary"] {
        assert!(message.contains(name), "the refusal must list {name}: {message}");
    }
}

#[tokio::test]
async fn an_anonymous_caller_may_not_read_a_report() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/accounting/reports/aging", None, None),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "an anonymous caller must be refused before the handler runs: {}",
        response.body
    );
}

#[tokio::test]
async fn the_aging_buckets_sum_to_the_outstanding_total() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;

    // One invoice in every bucket the report can produce, with the due dates that put them
    // there **relative to the server's today**, which is what the report buckets on.
    let today = time::OffsetDateTime::now_utc().date();
    let ago = |days: i64| (today - time::Duration::days(days)).to_string();
    let ahead = (today + time::Duration::days(10)).to_string();

    fixture.invoice("CUR", "10.00", "0.00", Some(&ahead)).await; // not yet due
    fixture.invoice("D5", "20.00", "0.00", Some(&ago(5))).await; // 1-30
    fixture.invoice("D45", "30.50", "10.50", Some(&ago(45))).await; // 31-60, 20.00 out
    fixture.invoice("D75", "40.00", "0.00", Some(&ago(75))).await; // 61-90
    fixture.invoice("D200", "50.00", "0.00", Some(&ago(200))).await; // 90+

    // **The window is named, and that is the fix.** The default is the last thirty days, and
    // these fixtures are up to 200 days late: read with the default, the due_date filter drops
    // every one of them and the identity below is asserted over a fraction of its own rows. The
    // report was right and the walk was reading a window it had not asked for.
    // **The window reaches FORWARD as well as back, and that is not decoration.** A period
    // ending today cannot contain an invoice that is not yet due -- its due date is in the
    // future -- so the one fixture that proves the `current` bucket is exactly the row an
    // "everything up to today" filter drops. An aging report asked about today alone is
    // structurally blind to the largest bucket it has.
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/accounting/reports/aging?from={}&to={}",
                today - time::Duration::days(400),
                today + time::Duration::days(60)
            ),
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", message_of(&response.body));

    let rows = response.body["rows"].as_array().expect("rows must be an array");
    let buckets = response.body["buckets"]
        .as_array()
        .expect("buckets must be an array");
    assert_eq!(
        rows.len(),
        5,
        "all five fixtures must be inside the window: {}",
        response.body["meta"]["row_count"]
    );

    // **The identity, summed off the wire the way a reader would.**
    let outstanding = sum_of(rows, "outstanding");
    let bucket_total = buckets.iter().fold(
        omnion_module_accounting::money::Amount::ZERO,
        |acc, bucket| acc.plus(money(bucket["amount"].as_str().unwrap_or("0.00"))),
    );
    assert_eq!(
        bucket_total.to_text(),
        outstanding,
        "the bucket totals must sum to the outstanding total"
    );
    assert_eq!(
        outstanding, "140.00",
        "10.00 current + 20.00 (1-30) + 20.00 outstanding of 30.50 (31-60) + 40.00 (61-90) + 50.00 (90+)"
    );

    // Every invoice placed itself, and the placement is the boundary the REQ lists.
    let bucket_of = |label: &str| {
        rows.iter()
            .find(|row| row["number"].as_str().is_some_and(|n| n.contains(label)))
            .map(|row| (row["bucket"].as_str().unwrap_or("").to_owned(), row["days_past_due"].as_i64()))
    };
    assert_eq!(bucket_of("D5").map(|b| b.0), Some("1-30".to_owned()));
    assert_eq!(bucket_of("D45").map(|b| b.0), Some("31-60".to_owned()));
    assert_eq!(bucket_of("D75").map(|b| b.0), Some("61-90".to_owned()));
    assert_eq!(bucket_of("D200").map(|b| b.0), Some("90+".to_owned()));
    assert_eq!(
        bucket_of("CUR").map(|b| b.0),
        Some("current".to_owned()),
        "an invoice that is not yet due is current, not 0-30 days late"
    );
    assert_eq!(
        bucket_of("D5").map(|b| b.1),
        Some(Some(5)),
        "days past due is the count itself, and an absent due date is None rather than 0"
    );
}

#[tokio::test]
async fn a_draft_and_a_void_invoice_are_not_receivables() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    let today = time::OffsetDateTime::now_utc().date();
    let ago = (today - time::Duration::days(10)).to_string();

    // A draft is not a claim on anybody and a void invoice has been taken out of the
    // receivables — the same exclusion `voiding_keeps_the_number` asserts for the totals. If a
    // report counted either, the aging table would overstate what is owed.
    fixture.invoice_with_status("draft", "500.00", Some(&ago)).await;
    fixture.invoice_with_status("void", "700.00", Some(&ago)).await;
    fixture.invoice("REAL", "42.00", "0.00", Some(&ago)).await;

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/reports/aging",
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let rows = response.body["rows"].as_array().expect("rows must be an array");
    let numbers: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["number"].as_str())
        .collect();
    assert!(
        numbers.iter().any(|n| n.contains("REAL")),
        "the sent invoice must be listed: {numbers:?}"
    );
    assert!(
        !numbers.iter().any(|n| n.contains("draft")),
        "a draft is not a receivable: {numbers:?}"
    );
    assert!(
        !numbers.iter().any(|n| n.contains("void")),
        "a void invoice is not a receivable: {numbers:?}"
    );
    assert_eq!(sum_of(rows, "outstanding"), "42.00");
}

#[tokio::test]
async fn an_invoice_with_no_due_date_is_listed_but_claims_no_bucket() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    fixture.invoice("NOTERM", "60.00", "0.00", None).await;

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/reports/aging",
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let rows = response.body["rows"].as_array().expect("rows must be an array");
    let row = rows
        .iter()
        .find(|row| row["number"].as_str().is_some_and(|n| n.contains("NOTERM")))
        .expect("an invoice with no due date is still a receivable and must be listed");
    // **The judgement call, asserted rather than commented.** `days_past_due` is null — not 0.
    // Filing it as "current" would report a debt nobody can age as though it were not late.
    assert!(
        row["days_past_due"].is_null(),
        "an invoice with no due date has no days past due: {row}"
    );
    assert!(row["due_date"].is_null());
    assert_eq!(sum_of(rows, "outstanding"), "60.00");
}

#[tokio::test]
async fn the_export_carries_exactly_the_rows_the_table_shows() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    let today = time::OffsetDateTime::now_utc().date();
    let ago = |d: i64| (today - time::Duration::days(d)).to_string();

    fixture.invoice("E1", "11.00", "0.00", Some(&ago(3))).await;
    fixture.invoice("E2", "22.00", "2.00", Some(&ago(40))).await;
    fixture.invoice("E3", "33.00", "0.00", Some(&ago(120))).await;

    // The SAME named window on both calls, for the same reason: the export must carry the rows
    // the screen shows, and both were being asked for the default thirty days over fixtures up
    // to 120 days old. A row-count comparison between two different windows proves nothing --
    // it would pass with an export that dropped every old row and a screen that did not.
    let window = format!(
        "?from={}&to={}",
        today - time::Duration::days(400),
        today
    );
    let on_screen = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/reports/aging{window}"),
            Some(&session),
            None,
        ),
    )
    .await;
    let exported = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/accounting/reports/aging/export{window}"),
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(exported.status, StatusCode::OK, "{}", message_of(&exported.body));

    // **The acceptance box, as a row COUNT rather than a promise.** The header carries the
    // number too, and a caller that believes the header must be able to check it.
    let screen_rows = on_screen.body["rows"].as_array().expect("rows must be an array").len();
    assert_eq!(
        screen_rows,
        3,
        "all three fixtures must be in the window: {}",
        on_screen.body["meta"]["row_count"]
    );
    let csv_rows = csv_data_rows(&exported.raw).len();
    assert_eq!(
        csv_rows, screen_rows,
        "the CSV must carry exactly the rows the table shows"
    );
    let header_count: Option<&str> = exported
        .headers
        .iter()
        .find(|(name, _)| name == "x-omnion-row-count")
        .map(|(_, value)| value.as_str());
    assert_eq!(
        header_count.map(str::parse::<usize>).transpose().ok().flatten(),
        Some(screen_rows),
        "x-omnion-row-count must agree with the table"
    );
    assert!(
        exported
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value.starts_with("text/csv")),
        "the export must be CSV: {:?}",
        exported.headers
    );
    // The definition travels with the data, so a CSV opened six months later is answerable.
    assert!(
        exported.raw.contains("# definition: Bucketed by days past due"),
        "the CSV must state how it buckets: {}",
        exported.raw
    );
}

#[tokio::test]
async fn the_income_report_totals_the_payments_of_the_period() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    let today = time::OffsetDateTime::now_utc().date();
    let this_month = today.to_string();
    let last_month = (today - time::Duration::days(35)).to_string();

    fixture.payment("100.00", &this_month).await;
    fixture.payment("50.00", &this_month).await;
    fixture.payment("25.00", &last_month).await;

    // The period is named, because the DEFAULT is the last thirty days and 35 days ago falls
    // outside it — a walk that read the default would be asserting the window it happened to get.
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/accounting/reports/income-expense?from={}&to={}",
                today - time::Duration::days(90),
                today
            ),
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", message_of(&response.body));
    let rows = response.body["rows"].as_array().expect("rows must be an array");
    assert_eq!(
        sum_of(rows, "income"),
        "175.00",
        "both months' payments must be in the rows"
    );
    // The totals line is the sum of the rows, not a second query.
    assert_eq!(
        response.body["totals"]["income"].as_str(),
        Some("175.00"),
        "the total must be the sum of the rows: {}",
        response.body["totals"]
    );
    assert_eq!(response.body["totals"]["expense"].as_str(), Some("0.00"));
    assert_eq!(response.body["totals"]["net"].as_str(), Some("175.00"));
}

#[tokio::test]
async fn the_cashflow_weekly_sum_matches_the_payments_of_the_period() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    let today = time::OffsetDateTime::now_utc().date();
    fixture.payment("70.00", &today.to_string()).await;
    fixture.payment("30.00", &(today - time::Duration::days(20)).to_string()).await;

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/accounting/reports/cashflow?from={}&to={}",
                today - time::Duration::days(60),
                today
            ),
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", message_of(&response.body));
    let rows = response.body["rows"].as_array().expect("rows must be an array");
    // **The identity the REQ names: the weekly sum matches the payments for the period.**
    assert_eq!(sum_of(rows, "money_in"), "100.00");
    assert_eq!(sum_of(rows, "money_out"), "0.00");
    assert_eq!(response.body["totals"]["money_in"].as_str(), Some("100.00"));
    // A week start is a Monday, and the series is sorted oldest first so a chart reads forwards.
    let weeks: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["week_start"].as_str())
        .collect();
    let mut sorted = weeks.clone();
    sorted.sort_unstable();
    assert_eq!(weeks, sorted, "the series must be oldest first: {weeks:?}");
    for week in weeks.iter() {
        let date = omnion_module_accounting::dates::parse(week).expect("a week start is a date");
        assert_eq!(
            date.weekday(),
            time::Weekday::Monday,
            "weeks start on Monday, not {weekday}",
            weekday = date.weekday()
        );
    }
}

#[tokio::test]
async fn the_tax_summary_reads_the_rate_copied_onto_the_line() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.tax().await;
    let today = time::OffsetDateTime::now_utc().date();

    // A rate, an invoice line carrying it at 20%, and then the rate EDITED to 25%. The report
    // must still say 20%: the line keeps the percentage it was issued under, and a report that
    // joined the rate table would retroactively restate a filed period.
    // The rate is written and then EDITED, and the report is expected to keep saying 20%. An
    // accounting line does not reference a rate at all -- it keeps the percentage it was issued
    // at -- so this row exists purely so that "somebody corrected a rate" is a real event in the
    // database rather than a comment in the walk.
    let rate_id: Uuid = sqlx::query_scalar(
        "insert into accounting_tax_rates (organization_id, name, percent, kind, is_default) \
         values ($1, 'RPT Sales 20', 20, 'sales', false) returning id",
    )
    .bind(fixture.organization)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the fixture rate must be written");

    let invoice_id = fixture.invoice("TAXED", "120.00", "0.00", Some(&today.to_string())).await;
    sqlx::query(
        "insert into accounting_invoice_lines \
             (invoice_id, organization_id, position, description, qty, unit_price, \
              net_amount, tax_percent, tax_amount, line_total) \
         values ($1, $2, 0, 'Taxed line', 1, 100.00, 100.00, 20.00, 20.00, 120.00)",
    )
    .bind(invoice_id)
    .bind(fixture.organization)
    .execute(fixture.db.pool())
    .await
    .expect("the fixture line must be written");

    sqlx::query("update accounting_tax_rates set percent = 25 where id = $1")
        .bind(rate_id)
        .execute(fixture.db.pool())
        .await
        .expect("the rate must be editable");

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/accounting/reports/tax-summary?from={}&to={}",
                today - time::Duration::days(30),
                today
            ),
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", message_of(&response.body));
    let rows = response.body["rows"].as_array().expect("rows must be an array");
    let row = rows
        .iter()
        .find(|row| row["percent"].as_str() == Some("20.00"))
        .unwrap_or_else(|| panic!("the taxed rate must be listed: {rows:?}"));
    assert_eq!(
        row["percent"].as_str(),
        Some("20.00"),
        "the report must use the rate the line was ISSUED at, not the edited one: {row}"
    );
    assert_eq!(row["tax"].as_str(), Some("20.00"));
    assert_eq!(row["base"].as_str(), Some("100.00"));
    // The label is DERIVED from the percentage, so it cannot name 25% -- and that is the point:
    // a label read from the rate table would have followed the edit and disagreed with the
    // number printed beside it.
    assert_eq!(row["rate_name"].as_str(), Some("Tax at 20.00%"));
    assert_eq!(row["kind"].as_str(), Some("Collected"));
}

#[tokio::test]
async fn a_window_that_ends_before_it_starts_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    let today = time::OffsetDateTime::now_utc().date();
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/accounting/reports/aging?from={}&to={}",
                today,
                today - time::Duration::days(10)
            ),
            Some(&session),
            None,
        ),
    )
    .await;
    // Without this the query returns nothing and the screen says "no rows in this period" for a
    // period that cannot exist — an answer that reads as data rather than as a mistake.
    assert!(
        response.status.is_client_error(),
        "a reversed window must be refused: {}",
        response.status
    );
    assert!(
        message_of(&response.body).contains("ends before it starts"),
        "the refusal must say why: {}",
        message_of(&response.body)
    );
}

#[tokio::test]
async fn a_malformed_day_is_refused_by_the_shared_parser() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/reports/aging?from=not-a-date",
            Some(&session),
            None,
        ),
    )
    .await;
    assert!(
        response.status.is_client_error(),
        "a malformed day must be refused: {}",
        response.status
    );
    let message = message_of(&response.body);
    assert!(message.contains("from"), "the refusal must name the parameter: {message}");
    assert!(
        message.contains("2026-01-31"),
        "the refusal must show the shape it wanted: {message}"
    );
}

#[tokio::test]
async fn another_organizations_invoice_is_not_in_the_report() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    let other = create_organization_row(&fixture.db, "reports-other").await;
    let today = time::OffsetDateTime::now_utc().date();
    let ago = (today - time::Duration::days(5)).to_string();

    // A stranger's receivable, written directly. If the report's organization filter were
    // missing, this amount would appear in the reader's aging and in the export with it.
    sqlx::query(
        "insert into accounting_invoices \
             (organization_id, number, invoice_status, currency, issue_date, due_date, subtotal, \
              discount_total, tax_total, grand_total, paid_total, customer_name) \
         values ($1, 'RPT-FOREIGN-1', 'sent', 'USD', current_date, $2::date, 9999, 0, 0, 9999, 0, \
                 'Somebody Else')",
    )
    .bind(other)
    .bind(&ago)
    .execute(fixture.db.pool())
    .await
    .expect("the foreign invoice must be written");

    let response = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/reports/aging",
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let raw = response.body.to_string();
    assert!(
        !raw.contains("Somebody Else"),
        "another organization's customer must not appear: {raw}"
    );
    assert!(
        !raw.contains("9999"),
        "another organization's money must not be counted: {raw}"
    );
}

#[tokio::test]
async fn the_period_label_says_what_the_window_was() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let session = fixture.read().await;
    let today = time::OffsetDateTime::now_utc().date();

    // A named window, an open one and the default — because a period label has to be able to
    // describe all three, and a report that cannot say "all time" pushes the caller to invent a
    // date rather than admit they want everything.
    let named = call(
        &fixture.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/accounting/reports/aging?from={}&to={}",
                today - time::Duration::days(30),
                today
            ),
            Some(&session),
            None,
        ),
    )
    .await;
    let label = named.body["meta"]["period_label"].as_str().unwrap_or_default().to_owned();
    assert!(label.contains(" to "), "a named window says both ends: {label}");

    let open = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/accounting/reports/aging?from=2020-01-01",
            Some(&session),
            None,
        ),
    )
    .await;
    assert_eq!(open.status, StatusCode::OK);
    let open_label = open.body["meta"]["period_label"].as_str().unwrap_or_default();
    assert!(
        open_label.contains("onwards"),
        "a window open at the end says so: {open_label}"
    );

    let default = call(
        &fixture.state,
        request(Method::GET, "/api/v1/accounting/reports/aging", Some(&session), None),
    )
    .await;
    assert_eq!(default.status, StatusCode::OK);
    assert_eq!(
        default.body["meta"]["from"].as_str(),
        Some((today - time::Duration::days(29)).to_string().as_str()),
        "the default window is the last thirty days inclusive"
    );
}
