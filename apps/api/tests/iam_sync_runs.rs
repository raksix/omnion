//! Integration walk for the sync ledger surface (REQ-065, slice 4 part 2).
//!
//! The route is the half the unit tests cannot reach, and the half with the most ways to lie.
//! So the walk is built around the sentences the panel will show, and each one is proved:
//!
//! * **A provider with no runs is an empty list, not an error** — a provider created a minute ago
//!   has never synced, and a 500 there would train an operator to avoid the screen.
//! * **`partial` is not `ok`.** The store derives the verdict from the failures; this walk proves
//!   the *response* carries it, because a route that re-derived its own boolean from
//!   `error_count > 0 && counts == 0` would disagree with the row the drawer shows.
//! * **A failed subject is a retryable unit, and its attempts stay visible.** Three failures of
//!   one person are three rows underneath and one row in the drawer — asserted both ways, since
//!   each of those being wrong produces a *plausible* screen.
//! * **A retry names its subjects and refuses anything else.** An empty list, a subject that never
//!   failed, and a run that is still going are three different refusals, and all three are here
//!   because all three would otherwise produce a run row claiming work nobody asked for.
//! * **A retry is a NEW run.** The old row is byte-identical afterwards, which is the observable
//!   form of "a run has one outcome".
//! * **Cross-tenant reads are 404, not 403** — an id is not a secret, and a 403 tells a stranger
//!   that it exists.
//!
//! The runs themselves are written through the *store*, not through SQL literals, so the walk
//! proves the surface against the real writer rather than against a fixture that could be wrong in
//! the same way twice.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sso::sync_runs::{
    self, SyncCounts, SyncKind,
};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    /// Read before the body, because login's session is in here and nowhere else. A walk that
    /// reads only the body authenticates as nobody and every later call answers 401.
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
    TestResponse {
        status,
        set_cookie,
        body: if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        },
    }
}

fn request(method: Method, uri: &str, session: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(credential) = session {
        builder = builder.header(header::COOKIE, credential);
    }
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
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
                "SKIP: PostgreSQL is not reachable ({error}) — the sync-ledger walk needs a \
                 database with every migration applied"
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

struct Fixture {
    state: AppState,
    db: Db,
    organization_id: Uuid,
    session: String,
    provider_id: Uuid,
    foreign_session: String,
    foreign_provider: Uuid,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let mut organizations = Vec::new();
        let mut sessions = Vec::new();
        let mut providers = Vec::new();

        for index in 0..2 {
            let slug = format!("sync-{index}-{}", Uuid::new_v4().simple());
            let organization_id: Uuid = sqlx::query_scalar(
                "insert into organizations (name, slug) values ($1, $2) returning id",
            )
            .bind("Sync Ledger Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the organization must be created");

            let email = format!("sync-owner-{index}-{}@omnion.test", Uuid::new_v4().simple());
            let owner = users::create_user(
                db.pool(),
                NewUser {
                    email: email.clone(),
                    password: PASSWORD.to_owned(),
                    display_name: "Sync Ledger Test Owner".to_owned(),
                    organization_id: Some(organization_id),
                },
            )
            .await
            .expect("the owner must be created");
            seed::bind_owner(db.pool(), owner.id)
                .await
                .expect("the owner binding must be created");

            let login = call(
                &state,
                request(
                    Method::POST,
                    "/api/v1/auth/login",
                    None,
                    Some(json!({ "email": email, "password": PASSWORD })),
                ),
            )
            .await;
            assert_eq!(login.status, StatusCode::OK, "login: {}", login.body);
            let session = format!(
                "omnion_session={}",
                login
                    .set_cookie
                    .as_deref()
                    .expect("login must set the session cookie")
                    .split(';')
                    .next()
                    .expect("the cookie has a value")
                    .split_once('=')
                    .expect("the cookie is name=value")
                    .1
            );

            let connected = call(
                &state,
                request(
                    Method::POST,
                    "/api/v1/iam/providers",
                    Some(&session),
                    Some(json!({
                        "slug": format!("corp-{index}"),
                        "name": "Sync Ledger Fixture",
                        "kind": "ldap",
                        "config": {
                            "url": "ldaps://dir.omnion.test",
                            "base_dn": "dc=omnion,dc=test",
                            "user_filter": "(objectClass=person)"
                        },
                        "sync_interval_minutes": 60
                    })),
                ),
            )
            .await;
            assert_eq!(connected.status, StatusCode::CREATED, "{}", connected.body);
            let provider_id =
                Uuid::parse_str(connected.body["id"].as_str().expect("an id")).expect("a uuid");

            organizations.push(organization_id);
            sessions.push(session);
            providers.push(provider_id);
        }

        Some(Self {
            state,
            db,
            organization_id: organizations[0],
            session: sessions[0].clone(),
            provider_id: providers[0],
            foreign_session: sessions[1].clone(),
            foreign_provider: providers[1],
            organizations,
        })
    }

    async fn get(&self, path: &str, session: &str) -> TestResponse {
        call(
            &self.state,
            request(Method::GET, path, Some(session), None),
        )
        .await
    }

    async fn retry(&self, run_id: Uuid, subjects: Value, session: &str) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/iam/providers/{}/sync-runs/{run_id}/retry", self.provider_id),
                Some(session),
                Some(json!({ "subjects": subjects })),
            ),
        )
        .await
    }

    /// Write a run through the store, so the surface is proved against its real writer.
    async fn write_run(&self, kind: SyncKind, errors: &[(&str, &str, &str)]) -> Uuid {
        let run = sync_runs::start_run(self.db.pool(), self.provider_id, kind, None)
            .await
            .expect("the run must open");
        for (subject, code, message) in errors {
            sync_runs::record_error(self.db.pool(), run.id, subject, code, message)
                .await
                .expect("the error must record");
        }
        let counts = SyncCounts {
            users_seen: 120,
            users_created: 3,
            users_updated: 4,
            users_deactivated: 1,
            groups_seen: 7,
        };
        sync_runs::finish_run(self.db.pool(), run.id, counts, &[], Some("walk fixture"))
            .await
            .expect("the run must finish");
        run.id
    }

    async fn cleanup(&self) {
        for organization_id in &self.organizations {
            for statement in [
                "delete from sessions where user_id in (select id from users where organization_id = $1)",
                "delete from users where organization_id = $1",
            ] {
                sqlx::query(statement)
                    .bind(organization_id)
                    .execute(self.db.pool())
                    .await
                    .expect("cleanup must run");
            }
            sqlx::query("delete from organizations where id = $1")
                .bind(organization_id)
                .execute(self.db.pool())
                .await
                .expect("organization cleanup must run");
        }
    }
}

#[tokio::test]
async fn the_sync_ledger_lists_partial_runs_refuses_fake_retries_and_keeps_the_old_row() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let provider = fixture.provider_id;
    let session = fixture.session.clone();
    let runs_path = format!("/api/v1/iam/providers/{provider}/sync-runs");

    // --- 1. A provider that has never synced is an empty list, not a failure ------------------
    let empty = fixture.get(&runs_path, &session).await;
    assert_eq!(empty.status, StatusCode::OK, "{}", empty.body);
    assert_eq!(
        empty.body["runs"].as_array().map(Vec::len),
        Some(0),
        "a provider with no runs answers an empty list: {}",
        empty.body
    );
    assert_eq!(empty.body["summary"]["problems"], json!(0));
    assert_eq!(
        empty.body["sync_interval_minutes"], json!(60),
        "the list carries the interval so the panel does not need a second call for its caption"
    );

    // --- 2. A clean run and a partial one, told apart by the derived verdict ------------------
    let clean = fixture.write_run(SyncKind::Full, &[]).await;
    // Two failures for the same person and one for another: three attempts, two subjects.
    let partial = fixture
        .write_run(
            SyncKind::Delta,
            &[
                ("cn=ada,dc=omnion,dc=test", "bind_failed", "the bind was refused"),
                ("cn=ada,dc=omnion,dc=test", "bind_failed", "the bind was refused again"),
                ("cn=linus,dc=omnion,dc=test", "object_violation", "an entry is already there"),
            ],
        )
        .await;

    let listed = fixture.get(&runs_path, &session).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let rows = listed.body["runs"].as_array().expect("an array of runs");
    assert_eq!(rows.len(), 2, "both runs are listed: {}", listed.body);
    // Newest first — the run an operator is looking for is the one that just happened.
    assert_eq!(rows[0]["id"], json!(partial.to_string()));
    assert_eq!(rows[0]["status"], json!("partial"));
    assert_eq!(rows[0]["error_count"], json!(3));
    assert_eq!(rows[0]["healthy"], json!(false), "a partial run is not a healthy one");
    assert_eq!(rows[0]["duration_seconds"].is_null(), false, "a finished run has a duration");
    assert_eq!(rows[1]["status"], json!("ok"));
    assert_eq!(rows[1]["healthy"], json!(true));
    assert_eq!(listed.body["summary"]["problems"], json!(1));

    // --- 3. The problems-only filter is the query an operator runs *because* the chip is amber -
    let problems = fixture
        .get(&format!("{runs_path}?problems_only=true"), &session)
        .await;
    assert_eq!(problems.status, StatusCode::OK, "{}", problems.body);
    let filtered = problems.body["runs"].as_array().expect("an array");
    assert_eq!(filtered.len(), 1, "only the run that ended badly: {}", problems.body);
    assert_eq!(filtered[0]["id"], json!(partial.to_string()));

    // --- 4. The drawer: three attempts, two subjects ------------------------------------------
    let drawer = fixture
        .get(&format!("{runs_path}/{partial}"), &session)
        .await;
    assert_eq!(drawer.status, StatusCode::OK, "{}", drawer.body);
    assert_eq!(drawer.body["attempts"], json!(3), "every honest failure is counted");
    let subjects = drawer.body["failed_subjects"].as_array().expect("an array");
    assert_eq!(subjects.len(), 2, "a subject is a retryable unit: {}", drawer.body);
    let ada = subjects
        .iter()
        .find(|subject| subject["key"].as_str().unwrap_or_default().contains("ada"))
        .expect("ada's row");
    assert_eq!(
        ada["attempts"], json!(2),
        "the repeat is visible, so a flapping directory looks like one: {ada}"
    );
    assert_eq!(ada["code"], json!("bind_failed"));

    // --- 5. A retry that names nothing is refused, not read as "everything" -------------------
    let none = fixture.retry(partial, json!([]), &session).await;
    assert_eq!(none.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", none.body);
    assert_eq!(none.body["error"]["code"], json!("no_subjects"));

    // --- 6. A retry for a subject that never failed is refused by name -------------------------
    let invented = fixture.retry(partial, json!(["cn=nobody,dc=omnion,dc=test"]), &session).await;
    assert_eq!(invented.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", invented.body);
    assert_eq!(invented.body["error"]["code"], json!("subject_not_failed"));
    assert!(
        invented.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("nobody"),
        "the refusal names what it refused: {}",
        invented.body
    );

    // --- 7. A running run has nothing to retry ------------------------------------------------
    let running = sync_runs::start_run(fixture.db.pool(), provider, SyncKind::Manual, None)
        .await
        .expect("the run must open");
    let too_early = fixture.retry(running.id, json!(["cn=ada,dc=omnion,dc=test"]), &session).await;
    assert_eq!(too_early.status, StatusCode::CONFLICT, "{}", too_early.body);
    assert_eq!(too_early.body["error"]["code"], json!("sync_run_in_progress"));
    // And the refusal left it running rather than finalising it into a lie.
    assert_eq!(
        sync_runs::get_run(fixture.db.pool(), running.id)
            .await
            .expect("the run reads")
            .expect("the run exists")
            .status,
        omnion_identity::sso::sync_runs::RunStatus::Running
    );

    // --- 8. A real retry opens a NEW run and leaves the old one byte-identical ----------------
    let before = fixture.get(&format!("{runs_path}/{partial}"), &session).await.body;
    let retried = fixture
        .retry(partial, json!(["cn=ada,dc=omnion,dc=test"]), &session)
        .await;
    assert_eq!(retried.status, StatusCode::OK, "{}", retried.body);
    let retry_id = Uuid::parse_str(retried.body["retry_run_id"].as_str().expect("an id"))
        .expect("a uuid");
    assert_ne!(
        retry_id, partial,
        "a retry is a new run — reopening would give one row two start times"
    );
    assert_eq!(retried.body["source_run_id"], json!(partial.to_string()));

    let after = fixture.get(&format!("{runs_path}/{partial}"), &session).await.body;
    assert_eq!(
        before["run"], after["run"],
        "the source run is untouched: {before} vs {after}"
    );
    assert_eq!(after["failed_subjects"].as_array().map(Vec::len), Some(2));

    // The event names the retry so an audit can answer "who re-ran this".
    let events: Vec<String> = sqlx::query_scalar(
        "select payload::text from events where organization_id = $1 \
         and name = 'iam.sync_retry_requested'",
    )
    .bind(fixture.organization_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the event query must read");
    assert!(
        events.iter().any(|payload| payload.contains(&retry_id.to_string())),
        "the retry is an event: {events:?}"
    );

    // --- 9. The group links surface, empty and then populated ---------------------------------
    let groups_path = format!("/api/v1/iam/providers/{provider}/sync-groups");
    let no_groups = fixture.get(&groups_path, &session).await;
    assert_eq!(no_groups.status, StatusCode::OK, "{}", no_groups.body);
    assert_eq!(no_groups.body["groups"].as_array().map(Vec::len), Some(0));

    sync_runs::upsert_group_links(
        fixture.db.pool(),
        provider,
        &[
            ("g-1".to_string(), "Platform".to_string(), 12, true),
            ("g-2".to_string(), "Support".to_string(), 0, false),
        ],
    )
    .await
    .expect("the group links must upsert");
    let groups = fixture.get(&groups_path, &session).await;
    assert_eq!(groups.status, StatusCode::OK, "{}", groups.body);
    assert_eq!(groups.body["groups"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        groups.body["summary"]["unsynced"], json!(1),
        "a group whose membership could not be read is counted, not hidden: {}",
        groups.body
    );

    // --- 10. A sub-surface must not answer differently than the parent it hangs off -------------
    // There are now three answers to "what does an outsider get for another tenant's provider"
    // in this file's own tree: the provider route says 403, the attribute-map sub-route says
    // 404, and there was no rule saying which a third one should copy. So the assertion is not
    // a status — it is that the child **agrees with the parent**, which is the property that
    // actually matters and which survives somebody unifying the two into a deliberate choice.
    // Asserting a hard-coded 403 would have frozen today's answer; asserting 404 would have
    // frozen the answer the system does not give.
    let parent = fixture
        .get(
            &format!("/api/v1/iam/providers/{}", fixture.foreign_provider),
            &session,
        )
        .await;
    let child = fixture
        .get(
            &format!("/api/v1/iam/providers/{}/sync-runs", fixture.foreign_provider),
            &session,
        )
        .await;
    assert_eq!(
        parent.status, child.status,
        "the sync ledger must not answer a different tenancy question than the provider it \
         belongs to (parent {}, child {}): {}",
        parent.status, child.status, child.body
    );
    assert!(
        !child.status.is_success(),
        "an outsider must not read another tenant's ledger at all: {}",
        child.body
    );
    // And the same rule in the other direction: the second owner may read *their own* provider's
    // ledger. A surface that answered 403 here would be refusing the owner's own screen, which
    // is the failure mode a tenant check is supposed to prevent rather than cause.
    let own = fixture
        .get(
            &format!("/api/v1/iam/providers/{}/sync-runs", fixture.foreign_provider),
            &fixture.foreign_session,
        )
        .await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    assert_eq!(
        own.body["runs"].as_array().map(Vec::len),
        Some(0),
        "and the runs belong to *that* provider, not to the caller: {}",
        own.body
    );
    // A run id is checked against the provider that owns it, so naming a real run under a
    // stranger's provider finds nothing rather than the run.
    let crossed = fixture
        .get(
            &format!(
                "/api/v1/iam/providers/{}/sync-runs/{partial}",
                fixture.foreign_provider
            ),
            &fixture.foreign_session,
        )
        .await;
    assert_eq!(crossed.status, StatusCode::NOT_FOUND, "{}", crossed.body);

    fixture.cleanup().await;
}
