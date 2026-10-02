//! Integration tests for the report, the CSV export and the global search
//! (docs/requests/REQ-052, slice 4b).
//!
//! A report is the one place in this module where a wrong number becomes somebody else's decision,
//! so these walks are written around **the claims the screen makes**, not around the endpoints:
//!
//! * the four buckets add up to the number of quotes the filter matched — the invariant a board
//!   would check by hand, asserted here so nobody has to;
//! * an accepted quote that became an order is a **win**, and an accepted quote with no order is
//!   still **pending** rather than a loss or a win the desk has not earned;
//! * a **cancelled** quote is neither: the organization withdrew it, and folding that into "lost"
//!   makes a seller who tidied up their pipeline look beaten;
//! * conversion is won ÷ (won + lost), and it is **`null`**, not `0%`, when nothing was decided;
//! * the average is over **won** deals only — averaging over every quote reports what was asked
//!   for, not what was sold;
//! * a quote is counted by the day it was **decided**, not the day it was written;
//! * **the CSV holds the same rows as the table**, byte for byte, and says out loud when the cap
//!   cut either of them short;
//! * the search finds both documents by number and by customer, ranks an exact number first, and
//!   is open to a reader who holds **either** read key — the guard is an "any of" and the walk is
//!   the only thing that would notice if it silently became an "all of".

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

const PASSWORD: &str = "correct horse battery";

/// Serialises this suite: the organizations and the IAM seed are shared state.
static REPORTS_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A full seller: everything the report, the export and the search need.
///
/// The four non-sales keys are here because the **fixture** builds its own company and product
/// through the API. Without them the walk dies at line 1 with a `403` about contacts and says
/// nothing at all about reports — which is exactly what happened twice, and the reason every sales
/// walk in this suite carries the same set. The lesson generalises: a walk that cannot reach its
/// own fixture fails before it has tested anything, so the fixture's permissions belong in the
/// fixture's role rather than in a second role the test then has to remember.
const REPORTER_PERMISSIONS: [&str; 13] = [
    "sales.quotes.read",
    "sales.quotes.create",
    "sales.quotes.update",
    "sales.quotes.send",
    "sales.orders.read",
    "sales.orders.create",
    "sales.orders.confirm",
    "sales.reports.read",
    "sales.products.read",
    "sales.products.manage",
    "crm.contacts.read",
    "crm.contacts.create",
    "sites.read",
];

/// May **see** the report and nothing else. This is the role that proves the export is not a way
/// around the table's permission: an export readable by somebody who cannot see the table is not
/// a smaller copy of it, it is an export in a folder with nothing to explain it.
const READER_PERMISSIONS: [&str; 4] = [
    "sales.quotes.read",
    "sales.orders.read",
    "sales.reports.read",
    "sites.read",
];

/// Holds **only** the orders read key. This is the account the "any of" search guard exists for: a
/// person whose job is deliveries and not quoting must still find their order by typing a
/// customer's name, and requiring both keys would make the search silently absent for them.
const ORDERS_ONLY_PERMISSIONS: [&str; 3] = ["sales.orders.read", "sites.read", "crm.contacts.read"];

/// Holds **only** the quotes read key, for the mirror of that check.
const QUOTES_ONLY_PERMISSIONS: [&str; 3] = ["sales.quotes.read", "sites.read", "crm.contacts.read"];

/// Holds neither sales read key: the palette must not answer, and it must say why.
const NEITHER_PERMISSIONS: [&str; 2] = ["sites.read", "crm.contacts.read"];

// ---------------------------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------------------------

struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    /// Kept for the tenancy walk, which needs a second organization of its own.
    #[allow(dead_code, reason = "the tenant walk creates its own second organization")]
    organization: Uuid,
    company: Uuid,
    product: Uuid,
    seller: String,
    reader: String,
    orders_only: String,
    quotes_only: String,
    neither: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = REPORTS_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.ok()?;

        let organization = create_organization_row(&db).await;
        let (owner_id, _) = create_account(&db, None, "Reports Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (seller_id, seller) = create_account(&db, Some(organization), "Reports Seller").await;
        grant(&db, organization, seller_id, owner_id, &REPORTER_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(organization), "Reports Reader").await;
        grant(&db, organization, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (orders_id, orders_only) =
            create_account(&db, Some(organization), "Reports Orders Only").await;
        grant(&db, organization, orders_id, owner_id, &ORDERS_ONLY_PERMISSIONS).await;

        let (quotes_id, quotes_only) =
            create_account(&db, Some(organization), "Reports Quotes Only").await;
        grant(&db, organization, quotes_id, owner_id, &QUOTES_ONLY_PERMISSIONS).await;

        let (neither_id, neither) = create_account(&db, Some(organization), "Reports Neither").await;
        grant(&db, organization, neither_id, owner_id, &NEITHER_PERMISSIONS).await;

        let token = login(&state, &seller).await;
        let company = create_company(&state, &token).await;
        let product = create_product(&state, &token).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            company,
            product,
            seller,
            reader,
            orders_only,
            quotes_only,
            neither,
        })
    }

    async fn token(&self, email: &str) -> String {
        login(&self.state, email).await
    }
}

// ---------------------------------------------------------------------------------------------
// A quote in each state the report classifies
// ---------------------------------------------------------------------------------------------

/// Write one quote and drive it to the outcome named, returning its id.
///
/// `sent` writes the two-line grid every walk in this file shares, because a report that counted
/// one free-text line would never notice a totals bug — and the won value is the sum of
/// `grand_total`, so it is exactly the number the arithmetic is checked against.
async fn quote(
    fixture: &Fixture,
    token: &str,
    customer: &str,
    outcome: &str,
) -> Uuid {
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/sales/quotes",
            Some(token),
            Some(json!({
                "customer_id": fixture.company,
                "customer_type": "company",
                "customer_name": customer,
                "currency": "TRY",
                "valid_until": "2030-12-31",
                "lines": [
                    { "product_id": fixture.product, "description": "Report widget",
                      "quantity": "2", "unit_price": "100.00", "tax_percent": 20 },
                    { "description": "On-site work", "quantity": "1", "unit_price": "50.00" },
                ],
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "quote: {}", created.body);
    let id = Uuid::parse_str(created.body["quote"]["id"].as_str().expect("an id")).expect("a uuid");

    match outcome {
        // A draft needs nothing else: it is the `pending` bucket's simplest member.
        "draft" | "sent" => {}
        "accepted" | "converted" => {
            send(fixture, token, id).await;
            accept(fixture, token, id).await;
            if outcome == "converted" {
                let order = call(
                    &fixture.state,
                    request(
                        Method::POST,
                        "/api/v1/sales/orders",
                        Some(token),
                        Some(json!({ "quote_id": id })),
                    ),
                )
                .await;
                assert_eq!(order.status, StatusCode::CREATED, "convert: {}", order.body);
            }
        }
        "declined" => {
            send(fixture, token, id).await;
            let link = call(
                &fixture.state,
                request(
                    Method::POST,
                    &format!("/api/v1/sales/quotes/{id}/link"),
                    Some(token),
                    None,
                ),
            )
            .await;
            assert_eq!(link.status, StatusCode::OK, "link: {}", link.body);
            let token_part = link.body["url"]
                .as_str()
                .expect("a url")
                .rsplit('/')
                .next()
                .expect("a segment")
                .to_string();
            let declined = call(
                &fixture.state,
                request(
                    Method::POST,
                    &format!("/api/v1/sales/public/quotes/{token_part}/decline"),
                    None,
                    Some(json!({ "note": "Budget moved" })),
                ),
            )
            .await;
            assert_eq!(declined.status, StatusCode::OK, "decline: {}", declined.body);
        }
        "cancelled" => {
            let cancelled = call(
                &fixture.state,
                request(
                    Method::POST,
                    &format!("/api/v1/sales/quotes/{id}/cancel"),
                    Some(token),
                    Some(json!({ "reason": "Withdrawn by the customer" })),
                ),
            )
            .await;
            assert_eq!(cancelled.status, StatusCode::OK, "cancel: {}", cancelled.body);
        }
        other => panic!("unknown outcome {other}"),
    }
    id
}

async fn send(fixture: &Fixture, token: &str, id: Uuid) {
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
    assert_eq!(response.status, StatusCode::OK, "send: {}", response.body);
}

/// Accept through a token the seller just issued.
async fn accept(fixture: &Fixture, token: &str, id: Uuid) {
    let link = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{id}/link"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(link.status, StatusCode::OK, "link: {}", link.body);
    let public = link.body["url"]
        .as_str()
        .expect("a url")
        .rsplit('/')
        .next()
        .expect("a segment")
        .to_string();
    let accepted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/public/quotes/{public}/accept"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "accept: {}", accepted.body);
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_four_buckets_add_up_and_a_cancelled_quote_is_neither_a_win_nor_a_loss() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.seller).await;
    let converted = quote(&fixture, &token, "Won Co", "converted").await;
    quote(&fixture, &token, "Draft Co", "draft").await;
    quote(&fixture, &token, "Gone Co", "declined").await;
    quote(&fixture, &token, "Withdrawn Co", "cancelled").await;

    let report = summary(&fixture, &token, "").await;

    // A cancelled quote is its own bucket. Folding it into `lost` would make a seller who tidied
    // up their pipeline look beaten, and folding it into `won` would be worse.
    assert_eq!(report["totals"]["cancelled"], 1, "{}", report["totals"]);
    assert_eq!(report["totals"]["lost"], 1, "only the declined one is lost");
    assert_eq!(report["totals"]["won"], 1, "the converted one is won");
    assert_eq!(report["totals"]["pending"], 1, "the draft is pending");

    // And the invariant a board would check by hand, checked here so nobody has to.
    let totals = &report["totals"];
    let sum = totals["won"].as_i64().expect("a count")
        + totals["lost"].as_i64().expect("a count")
        + totals["pending"].as_i64().expect("a count")
        + totals["cancelled"].as_i64().expect("a count");
    assert_eq!(
        sum,
        totals["quotes_seen"].as_i64().expect("a count"),
        "the four buckets must be the whole population: {totals}"
    );
    assert_eq!(sum, 4);

    // The won row names the order, which is what makes "won" checkable by reading the table.
    let row = report["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["quote_id"] == converted.to_string())
        .expect("the converted quote's row");
    assert_eq!(row["outcome"], "won");
    assert!(row["order_number"].is_string(), "a won row names its order: {row}");
}

#[tokio::test]
async fn an_accepted_quote_with_no_order_is_still_pending_and_the_average_covers_won_deals_only() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.seller).await;
    quote(&fixture, &token, "Ordered Co", "converted").await;
    quote(&fixture, &token, "Accepted Co", "accepted").await;
    // A much larger declined quote, so an average over *every* quote would be visibly wrong.
    quote(&fixture, &token, "Huge Lost Co", "declined").await;

    let report = summary(&fixture, &token, "").await;
    let totals = &report["totals"];

    assert_eq!(totals["won"], 1);
    assert_eq!(totals["pending"], 1, "an accepted quote with no order is pending");
    assert_eq!(totals["accepted_with_order"], 1);
    assert_eq!(totals["accepted_without_order"], 1);

    // The two conversion rates are different numbers and both are printed, because a seller told
    // only one of them would be misled by the other.
    assert_eq!(report["order_conversion_bps"], 5_000, "1 of 2 accepted became an order");

    // The average is over the one won deal. Averaging over the declined one would report a figure
    // that was never sold, and a board reading it as revenue plans against it.
    let average = report["average_deal"].as_str().expect("an average");
    let won = report["won_value"].as_str().expect("a value");
    assert_eq!(average, won, "with one won deal the mean is that deal: {report}");
    assert!(!average.is_empty());
}

#[tokio::test]
async fn a_report_with_nothing_decided_prints_no_conversion_rather_than_zero() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.seller).await;
    quote(&fixture, &token, "Only Drafts Co", "draft").await;

    let report = summary(&fixture, &token, "").await;
    // `null`, not `0`: "nobody decided yet" and "nobody won anything" are different facts, and a
    // 0% on a desk that has not quoted yet is a fact about a competitor that is not there.
    assert!(
        report["conversion_bps"].is_null(),
        "an undecided report has no conversion: {}",
        report["conversion_bps"]
    );
    assert!(
        report["average_deal"].is_null(),
        "an unwon report has no average: {}",
        report["average_deal"]
    );
    assert_eq!(report["totals"]["quotes_seen"], 1);
}

#[tokio::test]
async fn a_quote_is_counted_by_the_day_it_was_decided_and_not_the_day_it_was_written() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.seller).await;
    let declined = quote(&fixture, &token, "Decided Co", "declined").await;

    // The walk drives the decline itself, so the outcome is stamped **now** — and the assertion
    // is shaped around that rather than around a fixture that fakes a date. What it pins down is
    // the expression: the report counts a quote by `coalesce(accepted_at, declined_at,
    // cancelled_at, created_at)`, and for a quote decided in the same second it was written, all
    // four agree, so the day is today either way.
    //
    // The interesting half is the second call, where the four disagree. A window that ends
    // *yesterday* must not contain the quote even though the quote's `created_at` is inside
    // today and inside every 30-day window: if the filter were `created_at` alone, narrowing to a
    // single day would behave identically, and the two expressions would be indistinguishable
    // from a walk. A `set created_at` into the past is what separates them — the quote is written
    // long ago and decided now, so a report over its **written** day must come back empty.
    let today = time::OffsetDateTime::now_utc().date();
    let written_long_ago = today - time::Duration::days(200);

    // Rewind the document's own creation date by SQL, which is the only honest way to say "this
    // quote was written in January and the customer answered today".
    sqlx::query("update sales_quotes set created_at = $2 where id = $1")
        .bind(declined)
        .bind(written_long_ago)
        .execute(fixture.db.pool())
        .await
        .expect("the fixture quote must be rewindable");

    // A window over the day it was **written** must now be empty: the outcome happened today.
    let written_day = summary(
        &fixture,
        &token,
        &format!("from={}&to={}", written_long_ago, written_long_ago),
    )
    .await;
    assert_eq!(
        written_day["totals"]["quotes_seen"], 0,
        "the decided day is the day it counts, not the day it was written: {written_day}"
    );

    // And today's window must contain it, naming the order of columns the row prints.
    let decided_today = summary(
        &fixture,
        &token,
        &format!("from={}&to={}", today, today),
    )
    .await;
    assert_eq!(
        decided_today["totals"]["quotes_seen"], 1,
        "a quote decided today counts today whatever day it was written: {decided_today}"
    );
    let row = decided_today["rows"].as_array().expect("rows")[0].clone();
    assert_eq!(row["quote_id"], declined.to_string());
    assert_eq!(row["date"], today.to_string());
    assert_eq!(row["outcome"], "lost", "a declined quote is lost: {row}");

    // The default window is the last thirty days, which contains a quote written 200 days ago
    // and decided today — the case a `created_at`-only filter would have dropped from every
    // report a seller ever looked at.
    let default_window = summary(&fixture, &token, "").await;
    assert_eq!(default_window["from"], (today - time::Duration::days(29)).to_string());
    assert_eq!(
        default_window["totals"]["quotes_seen"], 1,
        "the default window counts the decision, not the writing: {default_window}"
    );
}

#[tokio::test]
async fn the_export_holds_the_same_rows_as_the_table() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.seller).await;
    quote(&fixture, &token, "Table Co", "converted").await;
    quote(&fixture, &token, "Export Co", "declined").await;
    quote(&fixture, &token, "Pending Co", "draft").await;

    let report = summary(&fixture, &token, "").await;
    let csv = export(&fixture, &token, "").await;

    // One line per row plus the header. The BOM rides on the first byte so Excel reads a
    // customer's name with a Turkish character in it.
    assert!(csv.starts_with('\u{feff}'), "the export opens with a BOM");
    let lines: Vec<&str> = csv.lines().collect();
    let rows = report["rows"].as_array().expect("rows");
    assert_eq!(
        lines.len(),
        1 + rows.len(),
        "the export must hold exactly the table's rows: {csv}"
    );
    for row in rows {
        let number = row["number"].as_str().expect("a number");
        assert!(
            lines[1..].iter().any(|line| line.contains(number)),
            "the export is missing {number}: {csv}"
        );
    }
}

#[tokio::test]
async fn a_capped_report_says_so_in_both_the_table_and_the_file() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.seller).await;
    for index in 0..3 {
        quote(&fixture, &token, &format!("Cap Co {index}"), "draft").await;
    }

    let report = summary(&fixture, &token, "limit=1").await;
    assert_eq!(report["rows"].as_array().expect("rows").len(), 1);
    assert_eq!(report["totals"]["quotes_seen"], 3, "the counts are not capped by the table");
    assert!(report["truncated"].as_bool().expect("a flag"), "the cap must be announced");
    assert_eq!(report["rows_matched"], 3);

    // The file says the same thing in its own first line, because a header is invisible and two
    // spreadsheets that disagree are somebody's afternoon.
    let csv = export(&fixture, &token, "limit=1").await;
    assert!(
        csv.lines().next().expect("a line").starts_with("# showing the first 1 of 3"),
        "the export must announce the cap: {csv}"
    );
}

#[tokio::test]
async fn a_window_that_makes_no_sense_is_refused_by_name() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.seller).await;

    let backwards = summary_status(&fixture, &token, "from=2026-03-10&to=2026-03-01").await;
    assert_eq!(backwards.0, StatusCode::BAD_REQUEST, "{}", backwards.1);
    assert!(backwards.1.contains("starts after it ends"), "{}", backwards.1);

    let forever = summary_status(&fixture, &token, "from=2000-01-01&to=2026-01-01").await;
    assert_eq!(forever.0, StatusCode::BAD_REQUEST, "{}", forever.1);
    assert!(forever.1.contains("at most"), "{}", forever.1);

    let nonsense = summary_status(&fixture, &token, "from=last%20tuesday").await;
    assert_eq!(nonsense.0, StatusCode::BAD_REQUEST, "{}", nonsense.1);
    assert!(nonsense.1.contains("from"), "the refusal names the field: {}", nonsense.1);

    let invented = summary_status(&fixture, &token, "status=notastatus").await;
    assert_eq!(invented.0, StatusCode::BAD_REQUEST, "{}", invented.1);
    assert!(invented.1.contains("notastatus"), "{}", invented.1);
}

#[tokio::test]
async fn the_search_finds_both_documents_by_number_and_by_customer_with_the_number_first() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.seller).await;
    let converted = quote(&fixture, &token, "Searchable Co", "converted").await;

    let order = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sales/orders?limit=50", Some(&token), None),
    )
    .await;
    let order_id = order.body["items"]
        .as_array()
        .expect("orders")
        .iter()
        .find(|item| item["quote_id"] == converted.to_string())
        .and_then(|item| item["id"].as_str())
        .expect("the order the conversion made")
        .to_string();
    let order_number: String = order.body["items"]
        .as_array()
        .expect("orders")
        .iter()
        .find(|item| item["id"] == order_id)
        .and_then(|item| item["number"].as_str())
        .expect("a number")
        .to_string();

    // By the customer's name: both documents, in one list, with what matched said out loud.
    let by_customer = search(&fixture, &token, "Searchable Co").await;
    let kinds: Vec<&str> = by_customer["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| hit["kind"].as_str().unwrap_or_default())
        .collect();
    assert!(kinds.contains(&"quote"), "the quote is found by its customer: {by_customer}");
    assert!(kinds.contains(&"order"), "the order is found by its customer: {by_customer}");
    assert!(by_customer["quotes"].as_i64().expect("a count") >= 1);
    assert!(by_customer["orders"].as_i64().expect("a count") >= 1);
    let first = &by_customer["hits"][0];
    assert_eq!(first["matched_on"], "customer");

    // By a number, exactly: the exact match outranks everything, which is the whole reason the
    // search is one ranked statement rather than two lists.
    let by_number = search(&fixture, &token, &order_number).await;
    let hits = by_number["hits"].as_array().expect("hits");
    assert_eq!(hits[0]["number"], order_number.as_str());
    assert_eq!(hits[0]["matched_on"], "number");
    assert_eq!(hits[0]["url"], format!("/sales/orders/{order_id}"));

    // A term nobody ever typed answers with nothing, not with an error and not with everything.
    let nothing = search(&fixture, &token, "no-such-customer-anywhere-9f3a").await;
    assert_eq!(nothing["hits"].as_array().expect("hits").len(), 0);
    assert_eq!(nothing["quotes"], 0);
    assert_eq!(nothing["orders"], 0);
}

#[tokio::test]
async fn the_search_is_open_to_either_read_key_and_closed_to_neither() {
    let Some(fixture) = Fixture::new().await else { return };
    let orders_only = fixture.token(&fixture.orders_only).await;
    let quotes_only = fixture.token(&fixture.quotes_only).await;
    let neither = fixture.token(&fixture.neither).await;

    // The "any of" guard. This is the assertion that would fail if `require_any` silently became
    // "all of": the palette is on every screen, and a search that vanishes for half the sales
    // desk with no message is worse than a search that is not there.
    let by_orders = search_status(&fixture, &orders_only, "anything").await;
    assert_eq!(
        by_orders.0,
        StatusCode::OK,
        "an orders-only reader must reach the search: {}",
        by_orders.1
    );
    let by_quotes = search_status(&fixture, &quotes_only, "anything").await;
    assert_eq!(
        by_quotes.0,
        StatusCode::OK,
        "a quotes-only reader must reach the search: {}",
        by_quotes.1
    );

    // And with neither, the refusal names the whole set rather than the last name tried.
    let refused = search_status(&fixture, &neither, "anything").await;
    assert_eq!(refused.0, StatusCode::FORBIDDEN, "{}", refused.1);
    assert!(refused.1.contains("sales.quotes.read"), "{}", refused.1);
    assert!(refused.1.contains("sales.orders.read"), "{}", refused.1);

    // 401 without a session at all, on all three routes.
    for path in [
        "/api/v1/sales/reports/summary",
        "/api/v1/sales/reports/export",
        "/api/v1/sales/search?q=x",
    ] {
        let anonymous = call(&fixture.state, request(Method::GET, path, None, None)).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn a_quote_or_order_of_another_organization_is_absent_from_the_report_and_the_search() {
    let Some(fixture) = Fixture::new().await else { return };
    let token = fixture.token(&fixture.seller).await;
    let mine = quote(&fixture, &token, "Private Co", "converted").await;

    // A second organization, with a document of its own, written as the owner so the row exists
    // with the full permission set behind it.
    let other_org = create_organization_row(&&fixture.db).await;
    let (other_id, other) = create_account(&&fixture.db, Some(other_org), "Reports Stranger").await;
    let (owner_id, _) = create_account(&&fixture.db, None, "Reports Owner 2").await;
    seed::bind_owner(fixture.db.pool(), owner_id).await.ok();
    grant(&fixture.db, other_org, other_id, owner_id, &REPORTER_PERMISSIONS).await;
    let other_token = login(&fixture.state, &other).await;
    let other_quote = quote_for(&fixture, &other_token, other_org, "Stranger Co").await;

    let report = summary(&fixture, &token, "").await;
    let mine_in_report = report["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .any(|row| row["quote_id"] == mine.to_string());
    assert!(mine_in_report, "my own quote is in my report");
    let stranger_in_report = report["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .any(|row| row["quote_id"] == other_quote.to_string());
    assert!(
        !stranger_in_report,
        "another organization's quote must not appear: {report}"
    );

    // The search is tenant-scoped for the same reason, and the stranger's customer name finds
    // nothing here even though it exists one row over.
    let found = search(&fixture, &token, "Stranger Co").await;
    assert_eq!(found["hits"].as_array().expect("hits").len(), 0);
    assert_eq!(found["quotes"], 0);
}

// ---------------------------------------------------------------------------------------------
// Harness
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

/// A GET whose body is **not** JSON, because the export is a file. A harness that always parses
/// would answer a CSV with a parse fallback and the walk could not tell a refusal from a file.
async fn call_text(state: &AppState, request: Request<Body>) -> (StatusCode, String, String) {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let disposition = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    (
        status,
        String::from_utf8_lossy(&bytes).to_string(),
        disposition,
    )
}

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
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

async fn summary(fixture: &Fixture, token: &str, query: &str) -> Value {
    let path = if query.is_empty() {
        "/api/v1/sales/reports/summary".to_string()
    } else {
        format!("/api/v1/sales/reports/summary?{query}")
    };
    let response = call(&fixture.state, request(Method::GET, &path, Some(token), None)).await;
    assert_eq!(response.status, StatusCode::OK, "summary: {}", response.body);
    response.body
}

/// A summary whose body may be a refusal, so the walk can read the message.
async fn summary_status(fixture: &Fixture, token: &str, query: &str) -> (StatusCode, String) {
    let path = if query.is_empty() {
        "/api/v1/sales/reports/summary".to_string()
    } else {
        format!("/api/v1/sales/reports/summary?{query}")
    };
    let response = call(&fixture.state, request(Method::GET, &path, Some(token), None)).await;
    let message = response.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    (response.status, message)
}

async fn export(fixture: &Fixture, token: &str, query: &str) -> String {
    let path = if query.is_empty() {
        "/api/v1/sales/reports/export".to_string()
    } else {
        format!("/api/v1/sales/reports/export?{query}")
    };
    let (status, text, disposition) =
        call_text(&fixture.state, request(Method::GET, &path, Some(token), None)).await;
    assert_eq!(status, StatusCode::OK, "export: {text}");
    assert!(
        disposition.contains("attachment;") && disposition.contains(".csv"),
        "the export must arrive as a named file: {disposition}"
    );
    assert!(
        disposition.contains("sales-report-"),
        "the filename carries the window: {disposition}"
    );
    text
}

async fn search(fixture: &Fixture, token: &str, term: &str) -> Value {
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/search?q={}", encode(term)),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "search: {}", response.body);
    response.body
}

/// A search term, percent-encoded by hand.
///
/// The alternative is to build a `url::Url` and read the query back out, which is a lot of
/// machinery for three of the walks here — and the two that pass a term with a space in it are
/// exactly the two that would notice a broken encoder.
fn encode(term: &str) -> String {
    term.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => other
                .to_string()
                .bytes()
                .map(|b| format!("%{b:02X}"))
                .collect(),
        })
        .collect()
}

async fn search_status(fixture: &Fixture, token: &str, term: &str) -> (StatusCode, String) {
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/search?q={}", encode(term)),
            Some(token),
            None,
        ),
    )
    .await;
    let message = response.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    (response.status, message)
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

async fn create_organization_row(db: &Db) -> Uuid {
    let slug = format!("reports-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind("Reports Test")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

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
            key: format!("reports-role-{}", Uuid::new_v4().simple()),
            name: "Reports Test Role".to_owned(),
            description: "A role of the reports walk".to_owned(),
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
            Some(json!({ "name": "Reports Test Co" })),
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
            Some(json!({ "sku": "RPT-1", "name": "Report widget", "default_price": "100.00" })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "product: {}", response.body);
    Uuid::parse_str(response.body["id"].as_str().expect("a product id")).expect("a uuid")
}

/// One plain draft quote for a walk that only needs *a* document to exist.
///
/// The customer is created through **this** account's own token, inside whichever organization
/// the token names. Handing it a company id from another organization would be a cross-tenant
/// reference — the walk would be testing a leak rather than the tenancy rule, and the module
/// (correctly) refuses it with "a quote needs a customer".
async fn quote_for(fixture: &Fixture, token: &str, organization: Uuid, customer: &str) -> Uuid {
    let company = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/crm/companies?organization_id={organization}"),
            Some(token),
            Some(json!({ "name": customer })),
        ),
    )
    .await;
    assert_eq!(company.status, StatusCode::CREATED, "stranger company: {}", company.body);
    let company_id = company.body["id"].as_str().expect("a company id").to_string();

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes?organization_id={organization}"),
            Some(token),
            Some(json!({
                "customer_id": company_id,
                "customer_type": "company",
                "currency": "TRY",
                "valid_until": "2030-12-31",
                "lines": [
                    { "description": "Consulting", "quantity": "1", "unit_price": "500.00" }
                ],
            })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "quote: {}", response.body);
    Uuid::parse_str(response.body["quote"]["id"].as_str().expect("an id")).expect("a uuid")
}
