//! Integration test for the form builder and its submission inbox (REQ-064, slice 2).
//!
//! Slice 2 is the "a stranger writes back" half of the CMS depth pack. What has to be true is:
//!
//! * a form is built, validated, saved and published, and a form with no fields is *refused*;
//! * every one of the eight field types accepts an answer, and required/pattern/length/choice
//!   validation produces the field's own message rather than a generic failure;
//! * the honeypot, the fill-time floor and the per-IP rate limit each hold, and each leaves a
//!   counter instead of a row;
//! * a valid submission appears in the inbox with the consent text stored verbatim, and the
//!   export of a *filtered* inbox returns exactly the filtered rows;
//! * reading the inbox is a different power from designing the form — an account that may build
//!   a contact form has no business reading its replies.
//!
//! It runs against the development stack. When PostgreSQL is not reachable the suite skips
//! itself with a printed reason, so `cargo test` stays usable on a machine without Docker — read
//! the `SKIP` line before believing a green count.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The builder's powers. Note what is absent: `forms.submissions.read` is deliberately not here,
/// because "may design a form" and "may read the answers" are different questions and folding
/// them together is the mistake the last test in this file exists to catch.
const BUILDER_PERMISSIONS: [&str; 3] = ["forms.read", "forms.manage", "content.pages.read"];

/// What the inbox adds on top.
const INBOX_EXTRA: [&str; 1] = ["forms.submissions.read"];

/// Result of one in-process HTTP call, in the pieces the assertions need.
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
        serde_json::from_slice(&bytes).expect("body must be JSON")
    };
    TestResponse {
        status,
        set_cookie,
        body,
    }
}

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&body).expect("body must serialize"),
            ))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

/// A request that carries extra headers, for the sender fingerprint the public route reads.
fn request_with_headers(
    method: Method,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&body).expect("body must serialize"),
            ))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({error}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
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
        test_storage(),
    );
    Some((state, db))
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("form-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Form Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
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
    assert!(
        response.status.is_success(),
        "login for {email} answered {}: {}",
        response.status,
        response.body
    );
    response
        .set_cookie
        .as_deref()
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("the cookie has a value")
        .split_once('=')
        .expect("the cookie is name=value")
        .1
        .to_owned()
}

async fn grant(db: &Db, organization_id: Uuid, user_id: Uuid, keys: &[&str], label: &str) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!(
                "{}-{}",
                label.to_lowercase().replace(' ', "-"),
                &Uuid::new_v4().simple().to_string()[..8]
            ),
            name: label.to_owned(),
            description: format!("{label} role"),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");
    let entries: Vec<RolePermissionInput> = keys
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
        scope: Scope::Organization { organization_id },
        granted_by: None,
        expires_at: None,
    };
    bindings::validate(db.pool(), &binding)
        .await
        .expect("the binding must validate");
    bindings::grant(db.pool(), binding)
        .await
        .expect("the binding must be granted");
}

/// The eight field types, in the shape the builder sends them.
fn all_field_types() -> Value {
    json!([
        { "key": "a_text", "label": "Text", "field_type": "text", "required": false, "width": "full", "rules": {}, "options": [] },
        { "key": "a_textarea", "label": "Textarea", "field_type": "textarea", "required": false, "width": "full", "rules": {}, "options": [] },
        { "key": "a_select", "label": "Select", "field_type": "select", "required": false, "width": "half", "rules": {}, "options": [{ "value": "gold", "label": "Gold" }, { "value": "silver", "label": "Silver" }] },
        { "key": "a_radio", "label": "Radio", "field_type": "radio", "required": false, "width": "half", "rules": {}, "options": [{ "value": "yes", "label": "Yes" }, { "value": "no", "label": "No" }] },
        { "key": "a_checkbox", "label": "Checkbox", "field_type": "checkbox", "required": false, "width": "half", "rules": {}, "options": [{ "value": "news", "label": "Send me news" }] },
        { "key": "a_date", "label": "Date", "field_type": "date", "required": false, "width": "half", "rules": {}, "options": [] },
        { "key": "a_file", "label": "File", "field_type": "file", "required": false, "width": "full", "rules": {}, "options": [] },
        { "key": "a_consent", "label": "I agree to the privacy policy", "field_type": "consent", "required": true, "width": "full", "rules": {}, "options": [] }
    ])
}

/// Every answer the eight fields accept.
fn all_answers() -> Value {
    json!({
        "a_text": "Ada",
        "a_textarea": "A longer paragraph of text.",
        "a_select": "gold",
        "a_radio": "yes",
        "a_checkbox": true,
        "a_date": "2026-03-14",
        "a_file": "cv.pdf",
        "a_consent": true
    })
}

struct Fixture {
    state: AppState,
    db: Db,
    org: Uuid,
    site: Uuid,
    /// The site's own key — the public surface addresses a site by key or host, never by uuid.
    site_key: String,
    builder_email: String,
    inbox_email: String,
    outsider_email: String,
    accounts: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(org)
            .bind("Form Test Org")
            .bind(format!("form-org-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let site = Uuid::new_v4();
        let site_key = format!("frm{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)")
            .bind(site)
            .bind(org)
            .bind(&site_key)
            .bind("Form Site")
            .execute(db.pool())
            .await
            .expect("the site must be created");

        // The builder may design a form and may NOT read the inbox. That is the whole point of
        // the third permission, and this account is the fixture that proves it.
        let (builder_id, builder_email) = create_account(&db, Some(org)).await;
        grant(
            &db,
            org,
            builder_id,
            &BUILDER_PERMISSIONS,
            "Form Builder",
        )
        .await;

        let (inbox_id, inbox_email) = create_account(&db, Some(org)).await;
        let mut inbox_keys = BUILDER_PERMISSIONS.to_vec();
        inbox_keys.extend_from_slice(&INBOX_EXTRA);
        grant(&db, org, inbox_id, &inbox_keys, "Form Inbox").await;

        let (outsider_id, outsider_email) = create_account(&db, Some(org)).await;

        Some(Self {
            state,
            db,
            org,
            site,
            site_key,
            builder_email,
            inbox_email,
            outsider_email,
            accounts: vec![builder_id, inbox_id, outsider_id],
        })
    }

    async fn builder(&self) -> String {
        login(&self.state, &self.builder_email).await
    }
    async fn inbox(&self) -> String {
        login(&self.state, &self.inbox_email).await
    }
    async fn outsider(&self) -> String {
        login(&self.state, &self.outsider_email).await
    }

    /// Create a form with the eight field types, and return its id.
    async fn form_with_all_types(&self, token: &str, key: &str) -> String {
        let created = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/forms",
                Some(token),
                Some(json!({
                    "site_id": self.site,
                    "key": key,
                    "name": "Contact",
                    "fields": all_field_types(),
                })),
            ),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        created.body["id"].as_str().expect("an id").to_owned()
    }

    /// Publish a form.
    async fn publish(&self, token: &str, form_id: &str) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/forms/{form_id}/publish"),
                Some(token),
                Some(json!({ "status": "published" })),
            ),
        )
        .await
    }

    /// Submit through the public route, as a visitor with a given address would.
    async fn submit(
        &self,
        form_key: &str,
        answers: Value,
        sender: &str,
        filled_at_ms: i64,
        honeypot: &str,
    ) -> TestResponse {
        let uri = format!(
            "/api/v1/public/forms/{form_key}/submit?site={}",
            self.site_key
        );
        let mut body = json!({ "answers": answers, "filled_at_ms": filled_at_ms, "source_path": "/contact" });
        if !honeypot.is_empty() {
            body["honeypot"] = json!(honeypot);
        }
        call(
            &self.state,
            request_with_headers(
                Method::POST,
                &uri,
                Some(body),
                &[
                    ("x-forwarded-for", sender),
                    ("user-agent", "form-test-agent"),
                ],
            ),
        )
        .await
    }

    async fn cleanup(&self) {
        for account in &self.accounts {
            let _ = sqlx::query("delete from users where id = $1")
                .bind(account)
                .execute(self.db.pool())
                .await;
        }
        let _ = sqlx::query("delete from sites where id = $1")
            .bind(self.site)
            .execute(self.db.pool())
            .await;
        let _ = sqlx::query("delete from organizations where id = $1")
            .bind(self.org)
            .execute(self.db.pool())
            .await;
    }
}

/// Every field type renders and accepts its own answer, and consent stores the text shown.
///
/// This is acceptance criterion 5 and 7's first half: a form that stores the eight answers and
/// keeps the consent sentence is a form whose *definitions* are honoured end to end.
#[tokio::test]
async fn all_eight_field_types_accept_their_answers_and_keep_the_consent_text() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.inbox().await;
    let form_id = fixture.form_with_all_types(&token, "all-types").await;
    let published = fixture.publish(&token, &form_id).await;
    assert_eq!(published.status, StatusCode::OK, "{}", published.body);

    let stored_fields = published.body["fields"].as_array().expect("the fields array");
    assert_eq!(
        stored_fields.len(),
        8,
        "all eight palette types must survive the save: {}",
        published.body
    );

    let submitted = fixture
        .submit("all-types", all_answers(), "203.0.113.10", 9_000, "")
        .await;
    assert_eq!(submitted.status, StatusCode::ACCEPTED, "{}", submitted.body);
    assert_eq!(submitted.body["stored"], json!(true), "{}", submitted.body);

    let inbox = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/forms/{form_id}/submissions"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(inbox.status, StatusCode::OK, "{}", inbox.body);
    let row = &inbox.body["submissions"][0];
    assert_eq!(row["answers"]["a_select"], json!("gold"));
    assert_eq!(row["answers"]["a_date"], json!("2026-03-14"));
    // The consent *text*, not a boolean: a row that says "agreed" cannot answer "agreed to what"
    // a year later, which is the only question it is ever asked.
    assert_eq!(
        row["consent_text"],
        json!("I agree to the privacy policy"),
        "the stored consent is the sentence that was shown: {}",
        row
    );
    fixture.cleanup().await;
}

/// Required and length/choice validation produce the field's own messages, in one answer.
///
/// Criterion 5's second half. The 422 carries *every* wrong field, because one field per round
/// trip turns a six-field form into six submissions and the fifth is what trips the rate limit.
#[tokio::test]
async fn validation_answers_with_every_field_error_at_once() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.inbox().await;
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/forms",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "key": "validated",
                "name": "Validated",
                "fields": [
                    { "key": "who", "label": "Who", "field_type": "text", "required": true, "width": "full", "rules": { "min_length": 3 }, "options": [] },
                    { "key": "plan", "label": "Plan", "field_type": "select", "required": true, "width": "full", "rules": {}, "options": [{ "value": "gold", "label": "Gold" }] },
                    { "key": "terms", "label": "I accept", "field_type": "consent", "required": true, "width": "full", "rules": {}, "options": [] }
                ],
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let form_id = created.body["id"].as_str().expect("an id").to_owned();
    assert_eq!(
        fixture.publish(&token, &form_id).await.status,
        StatusCode::OK
    );

    let refused = fixture
        .submit(
            "validated",
            json!({ "who": "ab", "plan": "bronze", "terms": false }),
            "203.0.113.20",
            9_000,
            "",
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an invalid submission is the one refusal a visitor is told about: {}",
        refused.body
    );
    // The envelope is `{"error": {"code", "message", "details"}}`, so `details` is one level
    // down. Reading it at the top level returns Null, and `as_object()` on Null is the panic —
    // which reads as "the server sent no field errors" rather than as "the test read the wrong
    // key", the same shape of mistake as the payload-vs-struct note in the menu ledger.
    let errors = refused.body["error"]["details"]["errors"]
        .as_object()
        .expect("the errors object");
    // All three, not the first one: the shape is the assertion, not the wording.
    assert_eq!(errors.len(), 3, "every wrong field at once: {errors:?}");
    assert_eq!(errors["who"], json!("please use at least 3 characters"));
    assert!(
        errors["plan"]
            .as_str()
            .expect("a message")
            .contains("not one of the offered options"),
        "a choice field must name what it offers: {:?}",
        errors["plan"]
    );
    assert_eq!(errors["terms"], json!("this field has to be accepted"));

    // And nothing was stored: an invalid submission is not a half-written row.
    let count: i64 = sqlx::query_scalar("select count(*) from cms_form_submissions where form_id = $1")
        .bind(Uuid::parse_str(&form_id).expect("a uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("a count");
    assert_eq!(count, 0, "a refused submission stores no row");
    fixture.cleanup().await;
}

/// A filled honeypot stores nothing and is answered as a success the visitor cannot read.
///
/// Criterion 6, first half. The route answers 202 either way: telling the visitor which
/// protection fired is a free oracle for "is this IP blocked" and it teaches a bot what to
/// work around.
#[tokio::test]
async fn a_filled_honeypot_stores_nothing_and_is_not_told() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.inbox().await;
    let form_id = fixture.form_with_all_types(&token, "honeypot").await;
    assert_eq!(
        fixture.publish(&token, &form_id).await.status,
        StatusCode::OK
    );

    let bot = fixture
        .submit(
            "honeypot",
            all_answers(),
            "203.0.113.30",
            9_000,
            "http://spam.example",
        )
        .await;
    assert_eq!(
        bot.status,
        StatusCode::ACCEPTED,
        "the refusal is not reported to the sender: {}",
        bot.body
    );
    assert_eq!(
        bot.body["stored"], json!(false),
        "the stored flag is the only signal, and it is not a verdict: {}",
        bot.body
    );
    assert!(
        bot.body.get("errors").is_none(),
        "a spam refusal must not leak field errors: {}",
        bot.body
    );

    let count: i64 = sqlx::query_scalar("select count(*) from cms_form_submissions where form_id = $1")
        .bind(Uuid::parse_str(&form_id).expect("a uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("a count");
    assert_eq!(count, 0, "a bot's submission leaves no row");
    fixture.cleanup().await;
}

/// A submission filled in under the floor is refused, and the floor is the form's own column.
#[tokio::test]
async fn a_too_fast_submission_is_refused_by_the_forms_own_floor() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.inbox().await;
    let form_id = fixture.form_with_all_types(&token, "fast").await;
    // Raise the floor so the default 3 seconds is not what the test is measuring.
    let raised = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/forms/{form_id}"),
            Some(&token),
            Some(json!({ "min_fill_seconds": 5 })),
        ),
    )
    .await;
    assert_eq!(raised.status, StatusCode::OK, "{}", raised.body);
    assert_eq!(
        fixture.publish(&token, &form_id).await.status,
        StatusCode::OK
    );

    let fast = fixture
        .submit("fast", all_answers(), "203.0.113.40", 1_000, "")
        .await;
    assert_eq!(fast.status, StatusCode::ACCEPTED, "{}", fast.body);
    assert_eq!(fast.body["stored"], json!(false), "{}", fast.body);

    let slow = fixture
        .submit("fast", all_answers(), "203.0.113.40", 9_000, "")
        .await;
    assert_eq!(slow.body["stored"], json!(true), "{}", slow.body);

    let count: i64 = sqlx::query_scalar("select count(*) from cms_form_submissions where form_id = $1")
        .bind(Uuid::parse_str(&form_id).expect("a uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("a count");
    assert_eq!(count, 1, "only the one that was actually filled in");
    fixture.cleanup().await;
}

/// The hourly limit holds per sender, and the throttle answers the *next* sender normally.
///
/// Criterion 6, second half. The count is taken BEFORE validation on purpose: a flood of invalid
/// submissions would otherwise be the cheapest way to fill an owner's inbox.
#[tokio::test]
async fn the_hourly_limit_is_per_sender_and_does_not_stop_the_next_one() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.inbox().await;
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/forms",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "key": "limited",
                "name": "Limited",
                "fields": [
                    { "key": "note", "label": "Note", "field_type": "text", "required": false, "width": "full", "rules": {}, "options": [] }
                ],
            })),
        ),
    )
    .await;
    let form_id = created.body["id"].as_str().expect("an id").to_owned();
    let raised = call(
        &fixture.state,
        request(
            Method::PUT,
            &format!("/api/v1/forms/{form_id}"),
            Some(&token),
            Some(json!({ "rate_limit_per_hour": 2 })),
        ),
    )
    .await;
    assert_eq!(raised.status, StatusCode::OK, "{}", raised.body);
    assert_eq!(
        fixture.publish(&token, &form_id).await.status,
        StatusCode::OK
    );

    for attempt in 0..2 {
        let sent = fixture
            .submit(
                "limited",
                json!({ "note": format!("message {attempt}") }),
                "203.0.113.50",
                9_000,
                "",
            )
            .await;
        assert_eq!(
            sent.body["stored"], json!(true),
            "submission {attempt} is inside the limit: {}",
            sent.body
        );
    }
    let third = fixture
        .submit(
            "limited",
            json!({ "note": "over the line" }),
            "203.0.113.50",
            9_000,
            "",
        )
        .await;
    assert_eq!(
        third.body["stored"], json!(false),
        "the third is over the limit: {}",
        third.body
    );

    // A *different* sender is untouched: a per-sender limit that throttled everybody would be a
    // denial-of-service an anonymous visitor could aim at a site's contact form.
    let other = fixture
        .submit(
            "limited",
            json!({ "note": "hello from elsewhere" }),
            "198.51.100.7",
            9_000,
            "",
        )
        .await;
    assert_eq!(
        other.body["stored"], json!(true),
        "the limit is per sender: {}",
        other.body
    );

    let count: i64 = sqlx::query_scalar("select count(*) from cms_form_submissions where form_id = $1")
        .bind(Uuid::parse_str(&form_id).expect("a uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("a count");
    assert_eq!(count, 3, "two from the throttled sender plus one from the other");
    fixture.cleanup().await;
}

/// The CSV export of a *filtered* inbox returns exactly the filtered rows.
///
/// Criterion 8. An export that ignores the filter is how a filtered inbox leaks its whole
/// history through a button labelled "Export".
#[tokio::test]
async fn the_export_of_a_filtered_inbox_returns_exactly_the_filtered_rows() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.inbox().await;
    let form_id = fixture.form_with_all_types(&token, "exported").await;
    assert_eq!(
        fixture.publish(&token, &form_id).await.status,
        StatusCode::OK
    );

    for (index, text) in ["Ada", "Grace", "Alan"].iter().enumerate() {
        let sent = fixture
            .submit(
                "exported",
                json!({ "a_text": text, "a_consent": true }),
                &format!("203.0.113.{}", 60 + index),
                9_000,
                "",
            )
            .await;
        assert_eq!(sent.body["stored"], json!(true), "{}", sent.body);
    }

    // Mark one as spam so the status filter has something to exclude.
    let inbox = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/forms/{form_id}/submissions?status=new"),
            Some(&token),
            None,
        ),
    )
    .await;
    let spamme = inbox.body["submissions"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let marked = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/forms/{form_id}/submissions/{spamme}"),
            Some(&token),
            Some(json!({ "status": "spam" })),
        ),
    )
    .await;
    assert_eq!(marked.status, StatusCode::OK, "{}", marked.body);

    let response = fixture
        .state
        .clone();
    let raw = routes::router(response)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "/api/v1/forms/{form_id}/submissions/export?status=new"
                ))
                .header(header::COOKIE, format!("omnion_session={token}"))
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router answers");
    assert_eq!(raw.status(), StatusCode::OK);
    let bytes = raw.into_body().collect().await.expect("body").to_bytes();
    let csv = String::from_utf8(bytes.to_vec()).expect("UTF-8");
    let lines: Vec<&str> = csv.lines().filter(|line| !line.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        3,
        "the header plus exactly the two unread rows: {csv}"
    );
    assert!(lines[0].starts_with("received,status"), "{}", lines[0]);
    // Assert on the DATA lines only. `spam` is a column value *and* a substring of nothing else
    // here — but checking the whole CSV for "spam" tests the header, not the rows, and it failed
    // for exactly that reason: the header line is the only line containing the word.
    assert!(csv.lines().any(|line| line.contains("Ada")), "{csv}");
    assert!(csv.lines().any(|line| line.contains("Grace")), "{csv}");
    assert!(
        !csv.lines().skip(1).any(|line| line.contains(",spam,")),
        "the row filtered out is absent from the data: {csv}"
    );
    fixture.cleanup().await;
}

/// Designing a form and reading its replies are two different powers.
#[tokio::test]
async fn designing_a_form_does_not_grant_the_inbox() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // The builder account holds forms.manage but NOT forms.submissions.read.
    let builder = fixture.builder().await;
    let inbox = fixture.inbox().await;
    let form_id = fixture.form_with_all_types(&builder, "separated").await;
    assert_eq!(
        fixture.publish(&inbox, &form_id).await.status,
        StatusCode::OK
    );

    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/forms/{form_id}/submissions"),
            Some(&builder),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "an account that may build a contact form may not read its replies: {}",
        refused.body
    );
    fixture.cleanup().await;
}

/// Another tenant's form is not reachable, and not even as an existence oracle.
#[tokio::test]
async fn another_organizations_form_is_not_reachable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let inbox = fixture.inbox().await;
    let form_id = fixture.form_with_all_types(&inbox, "private").await;

    // A second organization, its own site, its own reader.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other_org)
        .bind("Other Org")
        .bind(format!("form-other-{}", Uuid::new_v4().simple()))
        .execute(fixture.db.pool())
        .await
        .expect("the organization must be created");
    // The account needs its permissions *scoped to its own organization*, or the guard refuses
    // it before the handler runs and the test reads "403 where I expected 404" as a scope bug
    // in the route when the scope bug is in the fixture. Same lesson as the queue's
    // `no_organization` account: a fixture that does not build the shape being tested proves
    // nothing about the shape.
    let (outsider_id, outsider_email) = create_account(&fixture.db, Some(other_org)).await;
    grant(
        &fixture.db,
        other_org,
        outsider_id,
        &["forms.read", "forms.manage", "forms.submissions.read"],
        "Other Form Reader",
    )
    .await;
    let outsider = login(&fixture.state, &outsider_email).await;

    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/forms/{form_id}"),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::NOT_FOUND,
        "a cross-tenant reader gets 404, never 403: {}",
        read.body
    );

    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/forms/{form_id}"),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NOT_FOUND, "{}", removed.body);
    let still_there: i64 = sqlx::query_scalar("select count(*) from cms_forms where id = $1")
        .bind(Uuid::parse_str(&form_id).expect("a uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("a count");
    assert_eq!(still_there, 1, "the other tenant changed nothing");

    let _ = sqlx::query("delete from users where id = $1")
        .bind(outsider_id)
        .execute(fixture.db.pool())
        .await;
    let _ = sqlx::query("delete from organizations where id = $1")
        .bind(other_org)
        .execute(fixture.db.pool())
        .await;
    fixture.cleanup().await;
}

/// A draft form answers 404 to the public route, so the endpoint cannot confirm it exists.
#[tokio::test]
async fn a_draft_form_is_invisible_to_the_public_route() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let inbox = fixture.inbox().await;
    let form_id = fixture.form_with_all_types(&inbox, "unpublished").await;

    let sent = fixture
        .submit("unpublished", all_answers(), "203.0.113.70", 9_000, "")
        .await;
    assert_eq!(
        sent.status,
        StatusCode::NOT_FOUND,
        "a draft and a form that does not exist answer the same way: {}",
        sent.body
    );
    // Scoped to THIS form, not to the table. A count over every submission in the database is a
    // count of whatever an earlier test left behind, so it fails for reasons that have nothing
    // to do with the draft — the same family as "a test suite's fixtures can hide the account
    // that matters", one level up.
    let count: i64 =
        sqlx::query_scalar("select count(*) from cms_form_submissions where form_id = $1")
            .bind(Uuid::parse_str(&form_id).expect("a uuid"))
            .fetch_one(fixture.db.pool())
            .await
            .expect("a count");
    assert_eq!(count, 0, "a draft's key accepts nothing");
    fixture.cleanup().await;
}

/// A form with no fields cannot be created, and a form with a duplicate key is refused by name.
#[tokio::test]
async fn a_form_that_cannot_work_is_refused_at_creation() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.inbox().await;

    let empty = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/forms",
            Some(&token),
            Some(json!({ "site_id": fixture.site, "key": "empty", "name": "Empty", "fields": [] })),
        ),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{}", empty.body);
    assert!(
        empty.body.to_string().contains("at least one field"),
        "the refusal names what is missing: {}",
        empty.body
    );

    fixture.form_with_all_types(&token, "unique").await;
    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/forms",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "key": "unique",
                "name": "Another",
                "fields": [{ "key": "a", "label": "A", "field_type": "text", "width": "full", "rules": {}, "options": [] }]
            })),
        ),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT, "{}", duplicate.body);
    // `code` lives under `error`, like every other field of the envelope.
    assert_eq!(duplicate.body["error"]["code"], json!("form_key_taken"), "{}", duplicate.body);

    // Two fields sharing a key: answers are stored by key, so the second would overwrite the
    // first. Refused at save time, where the editor is looking at it.
    let clashing = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/forms",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "key": "clashing",
                "name": "Clashing",
                "fields": [
                    { "key": "same", "label": "First", "field_type": "text", "width": "full", "rules": {}, "options": [] },
                    { "key": "same", "label": "Second", "field_type": "text", "width": "full", "rules": {}, "options": [] }
                ]
            })),
        ),
    )
    .await;
    assert_eq!(clashing.status, StatusCode::BAD_REQUEST, "{}", clashing.body);
    assert!(
        clashing.body.to_string().contains("share the key"),
        "{}",
        clashing.body
    );

    // A choice field with no options is a control with nothing in it.
    let optionless = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/forms",
            Some(&token),
            Some(json!({
                "site_id": fixture.site,
                "key": "optionless",
                "name": "Optionless",
                "fields": [{ "key": "plan", "label": "Plan", "field_type": "select", "width": "full", "rules": {}, "options": [] }]
            })),
        ),
    )
    .await;
    assert_eq!(optionless.status, StatusCode::BAD_REQUEST, "{}", optionless.body);
    fixture.cleanup().await;
}

/// A submission stores no address, only a hash — the inbox is not a log of visitors.
#[tokio::test]
async fn the_inbox_stores_a_hash_of_the_sender_and_not_the_address() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.inbox().await;
    let form_id = fixture.form_with_all_types(&token, "hashed").await;
    assert_eq!(
        fixture.publish(&token, &form_id).await.status,
        StatusCode::OK
    );
    let sent = fixture
        .submit(
            "hashed",
            json!({ "a_text": "Ada", "a_consent": true }),
            "203.0.113.80",
            9_000,
            "",
        )
        .await;
    assert_eq!(sent.body["stored"], json!(true), "{}", sent.body);

    let (ip_hash, user_agent_hash): (Option<String>, Option<String>) =
        sqlx::query_as("select ip_hash, user_agent_hash from cms_form_submissions limit 1")
            .fetch_one(fixture.db.pool())
            .await
            .expect("a row");
    let ip_hash = ip_hash.expect("the sender is recorded as a hash");
    assert!(!ip_hash.contains("203.0.113.80"), "the raw address is stored: {ip_hash}");
    assert!(!ip_hash.contains('.'), "a hash, not an address: {ip_hash}");
    assert!(user_agent_hash.is_some(), "the agent is recorded as a hash too");
    fixture.cleanup().await;
}

/// A submission emits `content.form.submitted`, which is the contract an automation subscribes to.
#[tokio::test]
async fn a_submission_emits_the_event_the_req_names_as_contract() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture.inbox().await;
    let form_id = fixture.form_with_all_types(&token, "emitting").await;
    assert_eq!(
        fixture.publish(&token, &form_id).await.status,
        StatusCode::OK
    );
    let sent = fixture
        .submit(
            "emitting",
            json!({ "a_text": "Ada", "a_consent": true }),
            "203.0.113.90",
            9_000,
            "",
        )
    .await;
    assert_eq!(sent.body["stored"], json!(true), "{}", sent.body);

    // The bus is what an automation reads, so the assertion is on the row rather than on a
    // mock: an event that was emitted and lost would satisfy anything but this. The table is
    // `events` and the type is `name` — written from memory as `event_outbox.event_type` the
    // query answers "relation does not exist", which is indistinguishable from "the bus
    // dropped it".
    let emitted: i64 = sqlx::query_scalar(
        "select count(*) from events where name = 'content.form.submitted'",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("a count");
    assert!(emitted >= 1, "the submission must reach the bus: {emitted} rows");
    fixture.cleanup().await;
}
